//! RFC 0013 — object-storage backend acceptance scenarios (§5).
//!
//! Local-backend scenarios (`.2`, `.5`) run in the default `cargo test`. The
//! S3-backed scenarios (`.1`, `.3`, `.4`, `.7`) are `#[ignore]`d and run only
//! in the `s3-integration` CI job (testcontainers + `LocalStack`, which needs a
//! Docker-API runtime); the job invokes them by name via `--ignored`. `.6`
//! (WAL-stays-local) is greened end to end in `ourios-server`
//! (`tests/rfc0013_6_wal_stays_local.rs`), which wires this `Store` into the
//! server's RFC 0014 data write path — it can't live here, where there is no
//! WAL or server to observe. `.8` (reader forward-compat) is covered by the
//! colocated reader §3.9 tests (`rfc0005_2`/`_3`/`_4`), which read through the
//! same `Store` seam.
//!
//! See `docs/rfcs/0013-object-storage.md` §5/§6.

// Anchor the stubs to the public surface so they fail to compile if the
// `Store` seam is removed out from under them.
use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{
    DEFAULT_ZSTD_LEVEL, Manifest, PartitionKey, Published, Reader, S3Config, Store, Writer,
    encode_records_to_parquet,
};
use testcontainers_modules::localstack::LocalStack;
use testcontainers_modules::testcontainers::core::ExecCommand;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};

/// Start a `LocalStack` S3 container, create `bucket` with the image's own
/// `awslocal` (no S3-client dependency), and return the running container
/// (the caller keeps it alive — dropping it stops `LocalStack`) paired with a
/// `Store::s3` pointed at it via the endpoint override. Credentials come from
/// the `AWS_*` env the `s3-integration` CI job sets.
async fn localstack_s3(bucket: &str) -> (ContainerAsync<LocalStack>, Store) {
    let container = LocalStack::default()
        .with_env_var("SERVICES", "s3")
        .start()
        .await
        .expect("start localstack");
    let host = container.get_host().await.expect("container host");
    let port = container
        .get_host_port_ipv4(4566)
        .await
        .expect("container port");
    let endpoint = format!("http://{host}:{port}");

    let mut mb = container
        .exec(ExecCommand::new([
            "awslocal".to_string(),
            "s3".to_string(),
            "mb".to_string(),
            format!("s3://{bucket}"),
        ]))
        .await
        .expect("exec awslocal s3 mb");
    // Drain both streams before reading the exit code — testcontainers reports
    // `exit_code()` as `None` until the exec's output has been consumed.
    let stdout =
        String::from_utf8_lossy(&mb.stdout_to_vec().await.expect("mb stdout")).into_owned();
    let stderr =
        String::from_utf8_lossy(&mb.stderr_to_vec().await.expect("mb stderr")).into_owned();
    let code = mb.exit_code().await.expect("mb exit code");
    assert_eq!(
        code,
        Some(0),
        "awslocal s3 mb failed (code {code:?}): stdout={stdout:?} stderr={stderr:?}",
    );

    let store = Store::s3(
        S3Config::new(bucket)
            .with_endpoint(endpoint)
            .with_region("us-east-1"),
    )
    .expect("build s3 store");
    (container, store)
}

/// A clean-round-trip record for `tenant` at a fixed in-hour offset `i`.
fn rec_for(tenant: &str, i: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new(tenant),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("lib.cart".to_string()),
        scope_version: Some("1.0.0".to_string()),
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: 1_775_127_480_000_000_000 + i * 1_000,
        observed_time_unix_nano: Some(1_775_127_480_000_000_000 + i * 1_000 + 1),
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0x01,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ParamType::Num,
            value: format!("{i}"),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

/// Write `records` (one partition) through the Store-backed [`Writer`] and
/// return the published absolute path.
fn write_through_store(
    bucket: &std::path::Path,
    partition: &PartitionKey,
    records: &[MinedRecord],
) -> std::path::PathBuf {
    let mut writer = Writer::open(bucket, partition.clone()).expect("open writer");
    writer.append_records(records).expect("append");
    writer.close().expect("close").path
}

/// Scenario RFC0013.1 — a `MinedRecord` batch written and read through the `AmazonS3`
/// backend recovers byte-for-byte against the local backend.
/// See `docs/rfcs/0013-object-storage.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "RFC0013.1 — S3 integration; run via the `s3-integration` CI job (needs Docker + AWS_* env)"]
async fn rfc0013_1_round_trip_through_s3_backend() {
    let records: Vec<MinedRecord> = (0..300).map(|i| rec_for("tenant-a", i)).collect();
    let bytes = encode_records_to_parquet(&records, DEFAULT_ZSTD_LEVEL).expect("encode");
    let key = "data/tenant_id=tenant-a/year=2026/month=04/day=02/hour=10/file.parquet";

    let (_node, s3) = localstack_s3("ourios-it-roundtrip").await;
    s3.put(key, bytes.clone()).await.expect("s3 put");
    let s3_bytes = s3.get(key).await.expect("s3 get");

    // The same bytes through the local backend.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let local = Store::local(dir.path()).expect("local store");
    local.put(key, bytes.clone()).await.expect("local put");
    let local_bytes = local.get(key).await.expect("local get");

    assert_eq!(s3_bytes, bytes, "S3 returns the written bytes");
    assert_eq!(
        s3_bytes, local_bytes,
        "S3 and local recover identical bytes"
    );

    let decoded = Reader::open_bytes(bytes::Bytes::from(s3_bytes))
        .expect("open_bytes")
        .read_all()
        .expect("read_all");
    assert_eq!(
        decoded, records,
        "records recover byte-for-byte through the S3 backend"
    );
}

/// Scenario RFC0013.2 — the existing RFC 0005 / 0009 suites pass unchanged against the
/// `LocalFileSystem` backend after the seam refactor.
/// See `docs/rfcs/0013-object-storage.md` §5.
#[test]
fn rfc0013_2_local_backend_regresses_nothing() {
    // The Writer (encode + `Store.put`) and Reader (`Store.get` + decode) are
    // the LocalFileSystem-backed seam the existing RFC 0005 / 0009 suites
    // (round_trip, sizing, partition_layout, manifest, compaction) now run
    // through by default — a clean round-trip here is the behaviour-preserving
    // evidence for the local case.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let records: Vec<MinedRecord> = (0..200).map(|i| rec_for("tenant-a", i)).collect();
    let partition = PartitionKey::derive(&records[0]).expect("derive partition");
    let path = write_through_store(dir.path(), &partition, &records);
    let got = Reader::open_partition(&path, partition)
        .expect("open_partition")
        .read_all()
        .expect("read_all");
    assert_eq!(
        got, records,
        "round-trips byte-for-byte through the local Store"
    );
}

/// Scenario RFC0013.3 — two `compact_partition` runs racing on one partition: exactly
/// one manifest generation wins; no torn / doubled / missing rows.
/// See `docs/rfcs/0013-object-storage.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "RFC0013.3 — S3 integration; run via the `s3-integration` CI job"]
async fn rfc0013_3_atomic_publish_under_contention() {
    const KEY: &str = "data/tenant_id=t/year=2026/month=04/day=02/hour=10/manifest.json";
    let (_node, s3) = localstack_s3("ourios-it-cas3").await;

    // Seed generation 1, then capture its ETag.
    let seed = Manifest {
        generation: 1,
        files: vec!["seed.parquet".to_string()],
    };
    assert_eq!(
        seed.publish_cas(&s3, KEY, None).expect("seed"),
        Published::Won
    );
    let (_, etag) = Manifest::read_with_etag(&s3, KEY)
        .expect("read")
        .expect("present");
    let etag = etag.expect("S3 exposes an ETag");

    // Two writers race to swap gen 1 → gen 2 against the *same* ETag.
    let (sa, sb) = (s3.clone(), s3.clone());
    let (ea, eb) = (etag.clone(), etag.clone());
    let a = tokio::task::spawn_blocking(move || {
        Manifest {
            generation: 2,
            files: vec!["a.parquet".to_string()],
        }
        .publish_cas(&sa, KEY, Some(&ea))
    });
    let b = tokio::task::spawn_blocking(move || {
        Manifest {
            generation: 2,
            files: vec!["b.parquet".to_string()],
        }
        .publish_cas(&sb, KEY, Some(&eb))
    });
    let ra = a.await.expect("join a").expect("publish a");
    let rb = b.await.expect("join b").expect("publish b");

    // Exactly one wins; the other loses the compare-and-swap.
    let wins = [ra, rb].iter().filter(|p| **p == Published::Won).count();
    assert_eq!(wins, 1, "exactly one publisher wins (a={ra:?}, b={rb:?})");

    // The published generation is a consistent, un-torn gen 2 (one writer's).
    let (final_m, _) = Manifest::read_with_etag(&s3, KEY)
        .expect("read")
        .expect("present");
    assert_eq!(final_m.generation, 2);
    assert!(
        final_m.files == vec!["a.parquet".to_string()]
            || final_m.files == vec!["b.parquet".to_string()],
        "no torn / doubled state, got {:?}",
        final_m.files,
    );
}

/// Scenario RFC0013.4 — generation publish uses conditional PUT (`PutMode::Create` /
/// `Update{ETag}`) with no `rename` dependency.
/// See `docs/rfcs/0013-object-storage.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "RFC0013.4 — S3 integration; run via the `s3-integration` CI job"]
async fn rfc0013_4_manifest_swap_via_conditional_put() {
    const KEY: &str = "data/tenant_id=t/year=2026/month=04/day=02/hour=10/manifest.json";
    let (_node, s3) = localstack_s3("ourios-it-cas4").await;

    // First publish: create-if-absent (`If-None-Match`).
    let gen1 = Manifest {
        generation: 1,
        files: vec!["a.parquet".to_string()],
    };
    assert_eq!(
        gen1.publish_cas(&s3, KEY, None).expect("create"),
        Published::Won
    );
    // Create-if-absent again now loses — the object exists.
    assert_eq!(
        gen1.publish_cas(&s3, KEY, None).expect("re-create"),
        Published::Lost,
    );

    // Swap to gen 2 via compare-and-swap (`If-Match` on the read ETag).
    let (read, etag) = Manifest::read_with_etag(&s3, KEY)
        .expect("read")
        .expect("present");
    assert_eq!(read.generation, 1);
    let etag = etag.expect("S3 exposes an ETag");
    let gen2 = Manifest {
        generation: 2,
        files: vec!["b.parquet".to_string()],
    };
    assert_eq!(
        gen2.publish_cas(&s3, KEY, Some(&etag)).expect("swap"),
        Published::Won,
    );

    // A swap against the now-stale ETag loses (no blind overwrite, no rename).
    let stale = Manifest {
        generation: 3,
        files: vec!["c.parquet".to_string()],
    };
    assert_eq!(
        stale
            .publish_cas(&s3, KEY, Some(&etag))
            .expect("stale swap"),
        Published::Lost,
    );

    let (final_m, _) = Manifest::read_with_etag(&s3, KEY)
        .expect("read")
        .expect("present");
    assert_eq!(final_m.generation, 2);
    assert_eq!(final_m.files, vec!["b.parquet".to_string()]);
}

/// Scenario RFC0013.5 — operations in tenant X's context address only X's key
/// sub-prefix; no read/write touches tenant Y's keys (`CLAUDE.md` §3.7).
/// See `docs/rfcs/0013-object-storage.md` §5.
#[test]
fn rfc0013_5_tenant_isolation_across_prefix() {
    use std::path::PathBuf;

    // Two tenants' data in one store, same time bucket. Each tenant's file
    // lands under its own `data/tenant_id=<tenant>/…` key sub-prefix (the
    // partition key carries the tenant id), and reading one tenant's partition
    // surfaces only that tenant's rows — no cross-tenant key access.
    let dir = tempfile::TempDir::new().expect("temp dir");
    let x: Vec<MinedRecord> = (0..50).map(|i| rec_for("tenant-x", i)).collect();
    let y: Vec<MinedRecord> = (0..50).map(|i| rec_for("tenant-y", i)).collect();
    let px = PartitionKey::derive(&x[0]).expect("derive x");
    let py = PartitionKey::derive(&y[0]).expect("derive y");
    let xpath = write_through_store(dir.path(), &px, &x);
    let ypath = write_through_store(dir.path(), &py, &y);

    // Component-wise prefix check (portable across path separators).
    let rel = |p: &std::path::Path| {
        p.strip_prefix(dir.path())
            .expect("under bucket")
            .to_path_buf()
    };
    let tenant_prefix =
        |t: &str| -> PathBuf { ["data", &format!("tenant_id={t}")].iter().collect() };
    assert!(
        rel(&xpath).starts_with(tenant_prefix("tenant-x")),
        "tenant-x file under its own prefix: {:?}",
        rel(&xpath),
    );
    assert!(
        rel(&ypath).starts_with(tenant_prefix("tenant-y")),
        "tenant-y file under its own prefix: {:?}",
        rel(&ypath),
    );
    assert!(
        !rel(&xpath).starts_with(tenant_prefix("tenant-y")),
        "tenant-x must not land under tenant-y's prefix",
    );

    // Reading tenant-x's partition surfaces only tenant-x rows.
    let gx = Reader::open_partition(&xpath, px)
        .expect("open x")
        .read_all()
        .expect("read x");
    assert_eq!(gx.len(), x.len());
    assert!(
        gx.iter().all(|r| r.tenant_id.as_str() == "tenant-x"),
        "no tenant-y rows leak into a tenant-x read",
    );
}

// Scenario RFC0013.6 (WAL stays local) is greened end to end in `ourios-server`
// (`tests/rfc0013_6_wal_stays_local.rs`): it spawns the served binary with this
// `Store` wired into the RFC 0014 write path and asserts only Parquet/manifest
// objects reach the store while the WAL `*.wal` segments stay on local disk.
// It can't live in this crate, which has no WAL or server to observe.

/// Scenario RFC0013.7 — an S3-compatible store (`MinIO`) configured via an endpoint
/// override (RFC 0004) reads and writes exactly as AWS S3.
/// See `docs/rfcs/0013-object-storage.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "RFC0013.7 — S3 integration; run via the `s3-integration` CI job"]
async fn rfc0013_7_s3_compatible_endpoint_via_override() {
    // The store is configured purely through `S3Config::endpoint` (the RFC 0004
    // override) pointing at LocalStack; put/get/delete behave like AWS S3.
    let (_node, s3) = localstack_s3("ourios-it-endpoint").await;
    let key = "data/tenant_id=t/probe.bin";
    s3.put(key, b"endpoint-override".to_vec())
        .await
        .expect("put");
    assert_eq!(s3.get(key).await.expect("get"), b"endpoint-override");
    s3.delete(key).await.expect("delete");
    let err = s3.get(key).await.expect_err("object gone after delete");
    assert!(
        err.is_not_found(),
        "post-delete get should be NotFound, got {err:?}",
    );
}

/// Scenario RFC0013.8 — the RFC 0005 §3.9 reader forward-compat contract (absent
/// columns default, unknown columns ignored) holds over the object store.
/// See `docs/rfcs/0013-object-storage.md` §5.
#[test]
#[ignore = "RFC0013.8 — substantively covered by the colocated reader §3.9 tests \
            (rfc0005_2/3/4), which now read through the Store seam; a dedicated \
            variant-schema integration test is deferred (low marginal value)"]
fn rfc0013_8_reader_forward_compat_over_store() {
    // The reader's open path (`open_file`/`open_partition`/`open_bytes`) now
    // resolves bytes through `Store`, so the §3.9 forward-compat contract —
    // absent OPTIONAL columns default, unknown columns ignored, absent REQUIRED
    // columns error — is already exercised over the store by the colocated
    // reader tests `rfc0005_2_missing_optional_column_surfaces_as_none`,
    // `rfc0005_3_unknown_column_is_silently_ignored`, and
    // `rfc0005_4_missing_required_column_returns_hard_error` (which build
    // variant-schema Parquet via test-only Arrow helpers). A duplicate
    // integration test would re-create that machinery for no added coverage.
    todo!("RFC0013.8: dedicated variant-schema integration test (deferred)")
}

/// `Store::list_blocking` enumerates keys under a prefix on the real `AmazonS3`
/// backend (the seam the querier/compactor walk instead of `std::fs`, RFC 0019
/// §3.3) — recursive, prefix-scoped, store-relative keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "RFC 0019 — S3 integration; run via the `s3-integration` CI job (needs Docker + AWS_* env)"]
async fn store_list_enumerates_keys_on_s3() {
    let (_node, s3) = localstack_s3("ourios-it-list").await;
    for key in [
        "data/tenant_id=a/year=2026/h0.parquet",
        "data/tenant_id=a/year=2026/h1.parquet",
        "data/tenant_id=b/year=2026/h0.parquet",
    ] {
        s3.put(key, b"x".to_vec()).await.expect("s3 put");
    }

    // `list_blocking` is sync; run it off the async test thread (it drives its
    // own off-runtime bridge, but `spawn_blocking` keeps the test runtime free).
    let scoped = {
        let s3 = s3.clone();
        tokio::task::spawn_blocking(move || s3.list_blocking(Some("data/tenant_id=a")))
            .await
            .expect("join")
            .expect("list a")
    };
    // Asserted directly (no test-side sort) — `list_blocking` guarantees
    // lexicographic order, so a regression on the real S3 backend fails here.
    assert_eq!(
        scoped,
        vec![
            "data/tenant_id=a/year=2026/h0.parquet".to_string(),
            "data/tenant_id=a/year=2026/h1.parquet".to_string(),
        ],
        "S3 listing is prefix-scoped, recursive, store-relative, and ordered",
    );

    let all = tokio::task::spawn_blocking(move || s3.list_blocking(None))
        .await
        .expect("join")
        .expect("list all");
    assert_eq!(
        all,
        vec![
            "data/tenant_id=a/year=2026/h0.parquet".to_string(),
            "data/tenant_id=a/year=2026/h1.parquet".to_string(),
            "data/tenant_id=b/year=2026/h0.parquet".to_string(),
        ],
        "no prefix lists the whole bucket, ordered",
    );
}
