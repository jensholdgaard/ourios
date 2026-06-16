//! RFC 0013 — object-storage backend acceptance scenarios (§5).
//!
//! **Status: `red`.** These are the failing stubs that drive the `green`
//! implementation: each encodes one RFC0013.§5 scenario and currently
//! `todo!()`s. They are `#[ignore]`d so the default `cargo test` (and CI)
//! stays green while the backend is built — `green` replaces each body with
//! a real assertion and removes the `#[ignore]`. The S3-backed scenarios run
//! against a MinIO/localstack container (`testcontainers`); the local-backend
//! scenarios re-run the RFC 0005 / 0009 contract through `Store`.
//!
//! See `docs/rfcs/0013-object-storage.md` §5/§6.

// Anchor the stubs to the public surface so they fail to compile if the
// `Store` seam is removed out from under them.
use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{
    DEFAULT_ZSTD_LEVEL, PartitionKey, Reader, S3Config, Store, Writer, encode_records_to_parquet,
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

    let mb = container
        .exec(ExecCommand::new([
            "awslocal".to_string(),
            "s3".to_string(),
            "mb".to_string(),
            format!("s3://{bucket}"),
        ]))
        .await
        .expect("exec awslocal s3 mb");
    assert_eq!(
        mb.exit_code().await.expect("mb exit code"),
        Some(0),
        "awslocal s3 mb must succeed",
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
#[test]
#[ignore = "RFC0013.3 — red until conditional-PUT atomic publish lands"]
fn rfc0013_3_atomic_publish_under_contention() {
    todo!("RFC0013.3: exactly-one-wins under concurrent publishers")
}

/// Scenario RFC0013.4 — generation publish uses conditional PUT (`PutMode::Create` /
/// `Update{ETag}`) with no `rename` dependency.
/// See `docs/rfcs/0013-object-storage.md` §5.
#[test]
#[ignore = "RFC0013.4 — red until the manifest swap uses conditional PUT"]
fn rfc0013_4_manifest_swap_via_conditional_put() {
    todo!("RFC0013.4: publish path uses PutMode::Create/Update, never rename")
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

/// Scenario RFC0013.6 — with an object-storage backend, only data/audit/manifest
/// objects reach the store; the WAL stays on local disk (`CLAUDE.md` §3.4).
/// See `docs/rfcs/0013-object-storage.md` §5.
#[test]
#[ignore = "RFC0013.6 — red until the server wires the object-store backend"]
fn rfc0013_6_wal_stays_local() {
    let _ = S3Config::default();
    todo!("RFC0013.6: WAL frames local; only Parquet/manifest in the store")
}

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
    assert!(s3.get(key).await.is_err(), "object gone after delete");
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
