//! RFC 0019 — storage-backend selection, the server-level §5 scenarios.
//!
//! The config-resolution scenarios (`.1` backend selection, `.6` config/secret
//! hygiene, `.7` local-backend regression) are unit tests of the private
//! `build_store_config` / `build_config` in `src/main.rs`; see those.
//!
//! The remaining scenarios are server-level and exercise the resolved `Store`
//! end to end against a real S3 backend (LocalStack): the WAL staying local
//! under an S3 backend (`.2`), an end-to-end ingest→query on S3 (`.3`),
//! compaction on S3 (`.4`), and tenant isolation on S3 (`.5`).
//!
//! These four are `#[ignore]`d and run only in the `s3 integration
//! (localstack)` CI job (it needs a Docker-API runtime, which GitHub Actions
//! has but local containerd does not); the job invokes them by name via
//! `--ignored --exact`. They reuse the `LocalStack` harness pattern from
//! `crates/ourios-parquet/tests/rfc0013_object_store.rs`: a container, a bucket
//! created with the image's own `awslocal`, the endpoint override passed both to
//! the spawned `ourios-server` binary (as `OURIOS_S3_ENDPOINT`) and to a
//! `Store::s3` the test uses to assert what reached the bucket. Credentials come
//! from the `AWS_*` env the CI job sets (`LocalStack` accepts any), inherited by
//! the spawned child — never set here (RFC 0019 §3.4).
//!
//! Unix-only: graceful shutdown is driven by `kill -TERM`, like the colocated
//! RFC0013.6 / RFC0016.5/.7 harnesses these mirror.
//!
//! See `docs/rfcs/0019-storage-backend-selection.md` §5 / §6.
#![cfg(unix)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{Manifest, PartitionKey, S3Config, Store, Writer};
use prost::Message;
use testcontainers_modules::localstack::LocalStack;
use testcontainers_modules::testcontainers::core::ExecCommand;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::timeout;

// INFO (RFC 0018 / OTel `SeverityNumber`), the floor of `severity >= info`. The
// e2e fixtures carry this so the query predicate matches without depending on
// the miner's template-id assignment.
const SEVERITY_INFO: i32 = 9;

/// The current wall clock in unix nanos — the e2e fixture timestamp, always
/// inside the querier's default look-back window `[now - W, now]` by the time a
/// query runs a few seconds later.
fn now_ns() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    u64::try_from(nanos).unwrap_or(0)
}

fn string_value(s: &str) -> AnyValue {
    AnyValue {
        value: Some(Value::StringValue(s.to_owned())),
    }
}

/// Build an OTLP export with one `ResourceLogs` group per `(service, bodies)`
/// pair — distinct `service.name`s route to distinct tenants (RFC 0003 §6.3).
/// Each record carries `severity_number = INFO` and a fresh `time_unix_nano`
/// so `severity >= info` over the default window matches every ingested row.
fn export_request(groups: &[(&str, &[&str])]) -> ExportLogsServiceRequest {
    let resource_logs = groups
        .iter()
        .map(|(service, bodies)| ResourceLogs {
            resource: Some(Resource {
                attributes: vec![KeyValue {
                    key: "service.name".to_owned(),
                    value: Some(string_value(service)),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records: bodies
                    .iter()
                    .map(|b| LogRecord {
                        body: Some(string_value(b)),
                        severity_number: SEVERITY_INFO,
                        time_unix_nano: now_ns(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .collect();
    ExportLogsServiceRequest { resource_logs }
}

/// A clean-round-trip seed record for `tenant` at a fixed in-hour offset `i`
/// (partition `year=2026/month=04/day=02/hour=10`). Mirrors the
/// `rfc0013_object_store` fixture so the compaction seed lands in a known
/// partition prefix.
fn seed_record(tenant: &str, i: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new(tenant),
        template_id: 1,
        template_version: 1,
        // `SEVERITY_INFO` is the OTLP/proto severity (`i32`); `MinedRecord`'s is
        // `u8`, so convert rather than re-spelling the magic number.
        severity_number: SEVERITY_INFO.try_into().expect("INFO severity fits u8"),
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

/// Start a `LocalStack` S3 container, create `bucket` via the image's own
/// `awslocal`, and return the running container (the caller keeps it alive —
/// dropping it stops `LocalStack`), the endpoint URL (passed to the spawned
/// server as `OURIOS_S3_ENDPOINT` and used to build the assertion `Store`), and
/// a `Store::s3` pointed at it.
async fn localstack_s3(bucket: &str) -> (ContainerAsync<LocalStack>, String, Store) {
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
            .with_endpoint(endpoint.clone())
            .with_region("us-east-1"),
    )
    .expect("build s3 store");
    (container, endpoint, store)
}

/// A `Command` for the `ourios-server` binary configured for the S3 backend at
/// `endpoint`/`bucket`. Credentials are *not* set here — the child inherits the
/// `AWS_*` env the CI job provides (RFC 0019 §3.4).
fn s3_server(endpoint: &str, bucket: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ourios-server"));
    cmd.env("OURIOS_STORAGE_BACKEND", "s3")
        .env("OURIOS_S3_BUCKET", bucket)
        .env("OURIOS_S3_ENDPOINT", endpoint)
        .env("OURIOS_S3_REGION", "us-east-1")
        .kill_on_drop(true);
    cmd
}

/// A `Command` like [`s3_server`] but supplying credentials via the **explicit
/// S3-named** keys (`OURIOS_S3_ACCESS_KEY_ID` / `OURIOS_S3_SECRET_ACCESS_KEY` /
/// `OURIOS_S3_SESSION_TOKEN`, RFC 0019 §9) — and **removing** the `AWS_*` static
/// keys from the child's environment, so only the explicit path can
/// authenticate (RFC0019.8). Values come from the `AWS_*` the CI job sets
/// (`LocalStack` accepts any), defaulting to `LocalStack`'s conventional `test`.
fn s3_server_explicit_creds(endpoint: &str, bucket: &str) -> Command {
    let access = std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_else(|_| "test".to_string());
    let secret = std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_else(|_| "test".to_string());
    let session = std::env::var("AWS_SESSION_TOKEN").ok();
    let mut cmd = s3_server(endpoint, bucket);
    cmd.env("OURIOS_S3_ACCESS_KEY_ID", access)
        .env("OURIOS_S3_SECRET_ACCESS_KEY", secret)
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN");
    if let Some(token) = session {
        cmd.env("OURIOS_S3_SESSION_TOKEN", token);
    }
    cmd
}

/// Read the `{prefix}{addr}` line the server prints on startup, with a timeout.
async fn read_listen_addr(child: &mut Child, prefix: &str) -> SocketAddr {
    let stdout = child.stdout.take().expect("server stdout piped");
    let mut lines = BufReader::new(stdout).lines();
    let read = async {
        loop {
            let line = lines
                .next_line()
                .await
                .expect("read server stdout")
                .expect("server stdout closed before reporting the address");
            if let Some(rest) = line.strip_prefix(prefix) {
                return rest.trim().parse().expect("parse listen addr");
            }
        }
    };
    timeout(Duration::from_secs(30), read)
        .await
        .expect("server reports its bound address before timeout")
}

/// Hand-rolled OTLP/HTTP POST (no HTTP-client dependency).
async fn http_post_logs(addr: SocketAddr, body: &[u8]) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect HTTP");
    let head = format!(
        "POST /v1/logs HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/x-protobuf\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len(),
    );
    stream.write_all(head.as_bytes()).await.expect("write head");
    stream.write_all(body).await.expect("write body");
    stream.flush().await.expect("flush HTTP request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read HTTP response");
    String::from_utf8_lossy(&response).into_owned()
}

/// Hand-rolled `POST /v1/query` (no HTTP-client dependency): a `text/plain` DSL
/// body with the tenant header.
async fn http_post_query(addr: SocketAddr, tenant: &str, dsl: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect querier");
    let head = format!(
        "POST /v1/query HTTP/1.1\r\nHost: {addr}\r\nX-Ourios-Tenant: {tenant}\r\n\
         Content-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        dsl.len(),
    );
    stream.write_all(head.as_bytes()).await.expect("write head");
    stream.write_all(dsl.as_bytes()).await.expect("write body");
    stream.flush().await.expect("flush querier request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read HTTP response");
    String::from_utf8_lossy(&response).into_owned()
}

/// The JSON body of a `200` query response, parsed.
fn query_json(response: &str) -> serde_json::Value {
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "querier returns 200, got status line {:?}",
        response.lines().next(),
    );
    let body = response
        .split("\r\n\r\n")
        .nth(1)
        .expect("response has a body");
    serde_json::from_str(body).expect("body is JSON")
}

/// SIGTERM `child` (what k8s / `nerdctl stop` send) and assert a clean exit —
/// the graceful-shutdown drain flushes the RFC 0014 sink to the store.
async fn terminate_and_assert_clean(mut child: Child) {
    let pid = child.id().expect("server pid");
    let kill_status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .await
        .expect("run kill -TERM");
    assert!(kill_status.success(), "kill -TERM, got {kill_status:?}");
    let status = timeout(Duration::from_secs(30), child.wait())
        .await
        .expect("server exits before timeout")
        .expect("await server exit");
    assert!(
        status.success(),
        "graceful shutdown exits cleanly, got {status:?}"
    );
}

/// `Store::list_blocking` off the async test thread (it drives its own
/// off-runtime bridge; `spawn_blocking` keeps the test runtime free).
async fn list_keys(store: &Store, prefix: Option<&str>) -> Vec<String> {
    let store = store.clone();
    let prefix = prefix.map(str::to_owned);
    tokio::task::spawn_blocking(move || store.list_blocking(prefix.as_deref()))
        .await
        .expect("join list")
        .expect("list keys")
}

/// Every file (not directory) under `root`, recursively.
fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read test directory") {
            let path = entry.expect("read directory entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

fn has_extension(path: &Path, ext: &str) -> bool {
    path.extension().is_some_and(|e| e == ext)
}

/// Scenario RFC0019.2 — the WAL stays local under the S3 backend (`CLAUDE.md`
/// §3.6, extends RFC0013.6). With `OURIOS_STORAGE_BACKEND=s3` and a local
/// `OURIOS_WAL_ROOT`, ingesting one batch lands the flushed Parquet in the S3
/// bucket while the `*.wal` segments stay on local disk and never reach an
/// object key.
/// See `docs/rfcs/0019-storage-backend-selection.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "RFC0019.2 — S3 integration; run via the `s3-integration` CI job (needs Docker + AWS_* env)"]
async fn rfc0019_2_wal_stays_local_under_s3() {
    let (_node, endpoint, s3) = localstack_s3("ourios-it-srv-wal").await;
    let tmp = tempfile::TempDir::new().expect("temp");
    let wal_root = tmp.path().join("wal");

    let batch = export_request(&[("checkout", &["user 1 logged in", "user 2 logged in"])]);

    // Receiver on the S3 data backend; WAL on local disk.
    let mut child = s3_server(&endpoint, "ourios-it-srv-wal")
        .env("OURIOS_RECEIVER_ENABLED", "1")
        .env("OURIOS_RECEIVER_GRPC_ADDR", "127.0.0.1:0")
        .env("OURIOS_RECEIVER_HTTP_ADDR", "127.0.0.1:0")
        .env("OURIOS_WAL_ROOT", &wal_root)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ourios-server");

    let http_addr = read_listen_addr(&mut child, "receiver HTTP listening on ").await;
    let response = http_post_logs(http_addr, &batch.encode_to_vec()).await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "export returns 200, got status line {:?}",
        response.lines().next(),
    );

    // SIGTERM so the graceful-shutdown drain flushes the sink to S3.
    terminate_and_assert_clean(child).await;

    // The data Parquet reached the bucket; nothing WAL-ish did.
    let keys = list_keys(&s3, None).await;
    assert!(
        keys.iter()
            .any(|k| k.starts_with("data/") && k.ends_with(".parquet")),
        "the shutdown drain landed at least one data Parquet object on S3; saw {keys:?}",
    );
    for key in &keys {
        // The store only ever holds data/audit objects (manifests live under
        // `data/<partition>/manifest.json`); a WAL segment would surface as a
        // `wal/...` key, which this prefix check positively excludes.
        assert!(
            key.starts_with("data/") || key.starts_with("audit/"),
            "only data/audit objects reach the object store, found {key:?}",
        );
    }

    // The WAL segments stayed under the local WAL root.
    let wal_files = files_under(&wal_root);
    assert!(
        wal_files.iter().any(|p| has_extension(p, "wal")),
        "the WAL segments stay on local disk under the WAL root; saw {wal_files:?}",
    );
    assert!(
        !wal_files.iter().any(|p| has_extension(p, "parquet")),
        "no Parquet object may live under the WAL root; saw {wal_files:?}",
    );
}

/// Scenario RFC0019.3 — end-to-end ingest→query on S3.
/// See `docs/rfcs/0019-storage-backend-selection.md` §5.
///
/// Visibility is made deterministic with a two-phase shape (mirroring how the
/// local `rfc0016` served tests make data visible): the receiver write path
/// flushes the sink to S3 only on the flush cadence / graceful shutdown, so we
/// ingest, then SIGTERM the receiver (its `handle.shutdown().await` awaits the
/// drain — the Parquet is durable on S3 once `wait()` returns), then spawn a
/// fresh querier-only server over the *same* bucket and query. The querier
/// reads the flushed objects (glob fallback: a receiver-flushed partition has no
/// manifest yet — the compactor writes those).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "RFC0019.3 — S3 integration; run via the `s3-integration` CI job (needs Docker + AWS_* env)"]
async fn rfc0019_3_ingest_query_end_to_end_on_s3() {
    let (_node, endpoint, s3) = localstack_s3("ourios-it-srv-e2e").await;
    let tmp = tempfile::TempDir::new().expect("temp");
    let bodies = ["user 1 logged in", "user 2 logged in", "user 3 logged in"];
    let batch = export_request(&[("storefront", &bodies)]);

    // Phase 1 — ingest, then drain to S3 on SIGTERM.
    let mut receiver = s3_server(&endpoint, "ourios-it-srv-e2e")
        .env("OURIOS_RECEIVER_ENABLED", "1")
        .env("OURIOS_RECEIVER_GRPC_ADDR", "127.0.0.1:0")
        .env("OURIOS_RECEIVER_HTTP_ADDR", "127.0.0.1:0")
        .env("OURIOS_WAL_ROOT", tmp.path().join("wal"))
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn receiver");
    let http_addr = read_listen_addr(&mut receiver, "receiver HTTP listening on ").await;
    let response = http_post_logs(http_addr, &batch.encode_to_vec()).await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "export returns 200, got status line {:?}",
        response.lines().next(),
    );
    terminate_and_assert_clean(receiver).await;

    // The drain is durable on S3 before the query phase (clearer failure than a
    // 0-row query if the flush regressed).
    let data = list_keys(&s3, Some("data/tenant_id=storefront")).await;
    assert!(
        data.iter().any(|k| k.ends_with(".parquet")),
        "the receiver flushed Parquet under the tenant prefix on S3; saw {data:?}",
    );

    // Phase 2 — query the same bucket through a fresh querier-only server.
    let mut querier = s3_server(&endpoint, "ourios-it-srv-e2e")
        .env("OURIOS_QUERIER_ENABLED", "1")
        .env("OURIOS_QUERIER_HTTP_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn querier");
    let querier_addr = read_listen_addr(&mut querier, "querier HTTP listening on ").await;

    let response = http_post_query(querier_addr, "storefront", "severity >= info").await;
    let json = query_json(&response);
    // RFC0019.3 acceptance: the query returns the rows with pruning stats — "the
    // same result the local backend produces." We assert the row count + stats,
    // not the body *text*: reconstructing a clean row's body needs the read-time
    // template registry, derived from the miner's `template_created` audit events
    // — which the receiver does not yet persist (issue #302), so a clean row's
    // body renders empty on *both* backends. That gap is orthogonal to RFC 0019's
    // local-vs-S3 scope; when #302 lands these tests can also assert body text.
    assert_eq!(
        json["rows"], 3,
        "the three ingested rows round-trip out of S3: {json}",
    );
    let scanned = json["stats"]["row_groups_scanned"]
        .as_u64()
        .expect("pruning stats present in the query response");
    assert!(
        scanned >= 1,
        "the S3 scan reports row-group pruning stats: {json}"
    );

    terminate_and_assert_clean(querier).await;
}

/// Scenario RFC0019.4 — compaction operates on S3.
/// See `docs/rfcs/0019-storage-backend-selection.md` §5.
///
/// Seeds three small sealed Parquet files for one partition through the
/// Store-backed `Writer::open_in` (the `compact_partition_consolidates_on_s3`
/// seeding pattern), then runs the server's background compactor on the S3
/// backend with a 1 s sweep cadence. Polls the partition manifest (the
/// authoritative live-file set) until it collapses to a single file at
/// generation 2 — the `publish_cas` conditional-PUT commit. Polling (not a fixed
/// sleep) keeps it timing-robust.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "RFC0019.4 — S3 integration; run via the `s3-integration` CI job (needs Docker + AWS_* env)"]
async fn rfc0019_4_compaction_operates_on_s3() {
    const BUCKET: &str = "ourios-it-srv-compact";
    const MANIFEST_KEY: &str =
        "data/tenant_id=tenant-d/year=2026/month=04/day=02/hour=10/manifest.json";
    let (_node, endpoint, s3) = localstack_s3(BUCKET).await;

    // Three one-record files in one partition (the sealed-but-uncompacted
    // backlog), each below the small-file threshold so the default policy's
    // small-file arm makes the partition a candidate.
    let records: Vec<MinedRecord> = (0..3).map(|i| seed_record("tenant-d", i)).collect();
    let partition = PartitionKey::derive(&records[0]).expect("derive partition");
    {
        let s3 = s3.clone();
        let partition = partition.clone();
        tokio::task::spawn_blocking(move || {
            for record in &records {
                let mut writer = Writer::open_in(&s3, partition.clone()).expect("open_in");
                writer
                    .append_records(std::slice::from_ref(record))
                    .expect("append");
                writer.close().expect("close");
            }
        })
        .await
        .expect("join seed writes");
    }

    // Run the compactor (no receiver/querier — compaction always runs) with a
    // fast cadence.
    let child = s3_server(&endpoint, BUCKET)
        .env("OURIOS_COMPACTION_INTERVAL_SECS", "1")
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn compactor");

    // Poll the manifest until the live set is the single consolidated file.
    let consolidated = timeout(Duration::from_secs(30), async {
        loop {
            let s3 = s3.clone();
            let manifest =
                tokio::task::spawn_blocking(move || Manifest::read_with_etag(&s3, MANIFEST_KEY))
                    .await
                    .expect("join manifest read")
                    .expect("manifest read");
            if let Some((manifest, _)) = manifest {
                if manifest.files.len() == 1 {
                    return manifest;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("partition consolidates to one live file within the timeout");

    assert_eq!(
        consolidated.files.len(),
        1,
        "compaction consolidated the partition to a single live file",
    );
    assert!(
        consolidated.generation >= 2,
        "the manifest was swapped via the conditional-PUT CAS (bootstrap gen 1 → commit gen ≥ 2), got {}",
        consolidated.generation,
    );

    terminate_and_assert_clean(child).await;
}

/// Scenario RFC0019.5 — tenant isolation on S3 (`CLAUDE.md` §3.7).
/// See `docs/rfcs/0019-storage-backend-selection.md` §5.
///
/// Two tenants ingest distinct rows on the same bucket; querying one tenant
/// returns only its rows and never the other's. Reuses the `.3` two-phase
/// visibility shape (ingest+drain on SIGTERM, then a fresh querier).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "RFC0019.5 — S3 integration; run via the `s3-integration` CI job (needs Docker + AWS_* env)"]
async fn rfc0019_5_tenant_isolation_on_s3() {
    let (_node, endpoint, _s3) = localstack_s3("ourios-it-srv-tenants").await;
    let tmp = tempfile::TempDir::new().expect("temp");
    let batch = export_request(&[
        ("alpha", &["alpha one apple", "alpha two apple"]),
        ("bravo", &["bravo one cherry", "bravo two cherry"]),
    ]);

    // Phase 1 — both tenants ingest on the one bucket, drained on SIGTERM.
    let mut receiver = s3_server(&endpoint, "ourios-it-srv-tenants")
        .env("OURIOS_RECEIVER_ENABLED", "1")
        .env("OURIOS_RECEIVER_GRPC_ADDR", "127.0.0.1:0")
        .env("OURIOS_RECEIVER_HTTP_ADDR", "127.0.0.1:0")
        .env("OURIOS_WAL_ROOT", tmp.path().join("wal"))
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn receiver");
    let http_addr = read_listen_addr(&mut receiver, "receiver HTTP listening on ").await;
    let response = http_post_logs(http_addr, &batch.encode_to_vec()).await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "export returns 200, got status line {:?}",
        response.lines().next(),
    );
    terminate_and_assert_clean(receiver).await;

    // Phase 2 — query `alpha`; it must see only its own rows.
    let mut querier = s3_server(&endpoint, "ourios-it-srv-tenants")
        .env("OURIOS_QUERIER_ENABLED", "1")
        .env("OURIOS_QUERIER_HTTP_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn querier");
    let querier_addr = read_listen_addr(&mut querier, "querier HTTP listening on ").await;

    let response = http_post_query(querier_addr, "alpha", "severity >= info").await;
    let json = query_json(&response);
    // RFC0019.5 acceptance: alpha reads only its own prefix; bravo's objects are
    // never returned. Assert the row count + that bravo's `service.name` (carried
    // in each row's resource attributes) never appears — the cross-tenant leak
    // check. (Body *text* isn't asserted: it renders empty for clean rows until
    // issue #302; isolation is about which rows are returned, not their text.)
    assert_eq!(
        json["rows"], 2,
        "alpha sees exactly its own two rows, bravo's excluded: {json}",
    );
    assert!(
        !response.contains("bravo"),
        "no bravo data leaks into an alpha query: {response}",
    );

    terminate_and_assert_clean(querier).await;
}

/// Scenario RFC0019.8 — explicit S3 credentials, S3-named (RFC 0019 §9).
/// See `docs/rfcs/0019-storage-backend-selection.md` §9.5/§9.6.
///
/// Mirrors RFC0019.3's ingest→query round-trip, but the server authenticates via
/// the explicit `OURIOS_S3_*` credential keys with the `AWS_*` static keys
/// removed from its environment ([`s3_server_explicit_creds`]). A successful
/// round-trip confirms Ourios applies the explicit keys to the S3 builder.
/// (`LocalStack` does not enforce auth, so "the values reach the builder" and the
/// partial-set fail-fast / `Debug`-redaction assertions live in the
/// `ourios-parquet` + `ourios-server` unit tests; this proves the end-to-end
/// path is functional with only the S3-named keys set.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "RFC0019.8 — S3 integration; run via the `s3-integration` CI job (needs Docker + AWS_* env)"]
async fn rfc0019_8_explicit_s3_credentials_authenticate() {
    const BUCKET: &str = "ourios-it-srv-explicit-creds";
    let (_node, endpoint, s3) = localstack_s3(BUCKET).await;
    let tmp = tempfile::TempDir::new().expect("temp");
    let bodies = ["user 1 logged in", "user 2 logged in"];
    let batch = export_request(&[("storefront", &bodies)]);

    // Phase 1 — ingest with explicit OURIOS_S3_* creds (no AWS_*), drain on SIGTERM.
    let mut receiver = s3_server_explicit_creds(&endpoint, BUCKET)
        .env("OURIOS_RECEIVER_ENABLED", "1")
        .env("OURIOS_RECEIVER_GRPC_ADDR", "127.0.0.1:0")
        .env("OURIOS_RECEIVER_HTTP_ADDR", "127.0.0.1:0")
        .env("OURIOS_WAL_ROOT", tmp.path().join("wal"))
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn receiver");
    let http_addr = read_listen_addr(&mut receiver, "receiver HTTP listening on ").await;
    let response = http_post_logs(http_addr, &batch.encode_to_vec()).await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "export returns 200 under explicit creds, got status line {:?}",
        response.lines().next(),
    );
    terminate_and_assert_clean(receiver).await;

    // The data is durable on S3 — proving the explicit-credential write path
    // authenticated against the bucket.
    let data = list_keys(&s3, Some("data/tenant_id=storefront")).await;
    assert!(
        data.iter().any(|k| k.ends_with(".parquet")),
        "the receiver flushed Parquet to S3 authenticating via OURIOS_S3_* creds; saw {data:?}",
    );

    // Phase 2 — query the same bucket through a fresh querier, also explicit-creds.
    let mut querier = s3_server_explicit_creds(&endpoint, BUCKET)
        .env("OURIOS_QUERIER_ENABLED", "1")
        .env("OURIOS_QUERIER_HTTP_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn querier");
    let querier_addr = read_listen_addr(&mut querier, "querier HTTP listening on ").await;
    let response = http_post_query(querier_addr, "storefront", "severity >= info").await;
    let json = query_json(&response);
    assert_eq!(
        json["rows"], 2,
        "the rows round-trip out of S3 under explicit OURIOS_S3_* credentials: {json}",
    );

    terminate_and_assert_clean(querier).await;
}
