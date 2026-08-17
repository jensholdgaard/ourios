//! RFC 0046 — out-of-band tenancy at the process boundary: RFC0046.1
//! (selector required, both transports), .2 (binding check), .3 (one
//! export = one tenant; `service.name` is just an attribute), .7 (selector
//! hygiene, incl. round-trip of reserved characters), and .10 (the querier
//! is unchanged — the reads below go through it).
//!
//! Two server lifetimes over one store + WAL root, each driven through
//! `--config`, OTLP/HTTP + OTLP/gRPC export, SIGTERM (which flushes to
//! Parquet), and the querier: open mode first, then a static token bound to
//! `[acme]`.
//!
//! Unix-only: shutdown is driven with `kill -TERM` (as in
//! `rfc0003_16_served_binary`).
#![cfg(unix)]

use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use prost::Message;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::timeout;

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

/// One-record export whose `Resource` carries `attrs`, stamped now so the
/// querier's default look-back window includes it.
fn export(attrs: &[(&str, &str)], body: &str) -> Vec<u8> {
    export_request(attrs, body).encode_to_vec()
}

fn export_request(attrs: &[(&str, &str)], body: &str) -> ExportLogsServiceRequest {
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: attrs
                    .iter()
                    .map(|(key, value)| KeyValue {
                        key: (*key).to_owned(),
                        value: Some(string_value(value)),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records: vec![LogRecord {
                    time_unix_nano: now_ns(),
                    body: Some(string_value(body)),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

struct Server {
    child: Child,
    grpc: SocketAddr,
    http: SocketAddr,
    querier: SocketAddr,
    /// Drains stdout/stderr for the process lifetime — dropping the pipe
    /// readers would make the server's later `println!` hit a closed pipe.
    drain: tokio::task::JoinHandle<String>,
}

/// Spawn `ourios-server --config` with receiver + querier on ephemeral
/// ports over `tmp` (`store/`, `wal/`), returning the announced addresses.
async fn start(tmp: &Path, auth_yaml: &str) -> Server {
    let config = format!(
        "storage:\n  local:\n    bucket_root: {store}\n\
         receiver:\n  enabled: true\n  grpc_addr: 127.0.0.1:0\n  http_addr: 127.0.0.1:0\n\
         \x20\x20wal_root: {wal}\n\
         querier:\n  enabled: true\n  http_addr: 127.0.0.1:0\n{auth_yaml}",
        store = tmp.join("store").display(),
        wal = tmp.join("wal").display(),
    );
    let path = tmp.join(format!("config-{}.yaml", now_ns()));
    std::fs::write(&path, config).expect("write config");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .arg("--config")
        .arg(&path)
        .env("RFC0046_TOKEN", "tok-acme")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ourios-server");
    let stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let stderr_drain =
        std::sync::Arc::new(tokio::sync::Mutex::new(Some(tokio::spawn(async move {
            let mut err = String::new();
            stderr.read_to_string(&mut err).await.ok();
            err
        }))));
    let stderr_for_panic = std::sync::Arc::clone(&stderr_drain);
    let mut lines = BufReader::new(stdout).lines();
    let mut grpc = None;
    let mut http = None;
    let mut querier = None;
    let read = async {
        while grpc.is_none() || http.is_none() || querier.is_none() {
            let Some(line) = lines.next_line().await.expect("read stdout") else {
                let handle = stderr_for_panic.lock().await.take();
                let err = match handle {
                    Some(h) => h.await.unwrap_or_default(),
                    None => String::new(),
                };
                panic!("server exited before announcing its addresses; stderr:\n{err}");
            };
            if let Some(rest) = line.strip_prefix("receiver gRPC listening on ") {
                grpc = Some(rest.trim().parse().expect("grpc addr"));
            } else if let Some(rest) = line.strip_prefix("receiver HTTP listening on ") {
                http = Some(rest.trim().parse().expect("http addr"));
            } else if let Some(rest) = line.strip_prefix("querier HTTP listening on ") {
                querier = Some(rest.trim().parse().expect("querier addr"));
            }
        }
    };
    timeout(Duration::from_secs(20), read)
        .await
        .expect("server announces its addresses");
    let drain = tokio::spawn(async move {
        while let Ok(Some(_)) = lines.next_line().await {}
        match stderr_drain.lock().await.take() {
            Some(h) => h.await.unwrap_or_default(),
            None => String::new(),
        }
    });
    Server {
        child,
        grpc: grpc.expect("grpc"),
        http: http.expect("http"),
        querier: querier.expect("querier"),
        drain,
    }
}

async fn stop(mut server: Server) {
    let pid = server.child.id().expect("pid");
    Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .await
        .expect("kill -TERM");
    let status = timeout(Duration::from_secs(20), server.child.wait())
        .await
        .expect("exit before timeout")
        .expect("await exit");
    let stderr = server.drain.await.expect("drain");
    assert!(
        status.success(),
        "clean shutdown, got {status:?}; stderr:\n{stderr}"
    );
}

async fn raw_post(addr: SocketAddr, head: String, body: &[u8]) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.write_all(head.as_bytes()).await.expect("write head");
    stream.write_all(body).await.expect("write body");
    stream.flush().await.ok();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("read");
    String::from_utf8_lossy(&response).into_owned()
}

/// OTLP/HTTP export with an optional tenant selector (`None` = header
/// absent; `Some(v)` for each entry in `selectors` adds one header, so a
/// repeated header is expressible) and an optional bearer; returns the
/// status code and body.
async fn post_logs(
    addr: SocketAddr,
    body: &[u8],
    selectors: &[&str],
    bearer: Option<&str>,
) -> (u16, String) {
    let auth = bearer.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let mut tenant = String::new();
    for s in selectors {
        tenant.push_str("X-Ourios-Tenant: ");
        tenant.push_str(s);
        tenant.push_str("\r\n");
    }
    let head = format!(
        "POST /v1/logs HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/x-protobuf\r\n\
         {tenant}{auth}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len(),
    );
    let response = raw_post(addr, head, body).await;
    let status = status_of(&response);
    let body = response
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    (status, body)
}

/// OTLP/gRPC export with an optional selector; returns the tonic code.
async fn grpc_export(
    addr: SocketAddr,
    request: ExportLogsServiceRequest,
    selectors: &[&str],
) -> Result<(), tonic::Status> {
    use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;
    let mut client = LogsServiceClient::connect(format!("http://{addr}"))
        .await
        .expect("connect gRPC");
    let mut req = tonic::Request::new(request);
    for s in selectors {
        req.metadata_mut()
            .append("x-ourios-tenant", s.parse().expect("ascii"));
    }
    client.export(req).await.map(|_| ())
}

fn status_of(response: &str) -> u16 {
    response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {response:?}"))
}

/// `true` over `tenant` — the row count the querier reports.
async fn rows_for(addr: SocketAddr, tenant: &str) -> u64 {
    rows_for_dsl(addr, tenant, "true").await
}

/// Row count for `dsl` over `tenant`.
async fn rows_for_dsl(addr: SocketAddr, tenant: &str, dsl: &str) -> u64 {
    let head = format!(
        "POST /v1/query HTTP/1.1\r\nHost: {addr}\r\nX-Ourios-Tenant: {tenant}\r\n\
         Content-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        dsl.len(),
    );
    let response = raw_post(addr, head, dsl.as_bytes()).await;
    assert_eq!(status_of(&response), 200, "query {tenant}: {response}");
    let body = response.split("\r\n\r\n").nth(1).expect("body");
    let json: serde_json::Value = serde_json::from_str(body).expect("json");
    json["rows"].as_u64().expect("rows")
}

/// Scenarios RFC0046.1 / .2 / .3 / .7 / .10.
/// See `docs/rfcs/0046-out-of-band-tenancy.md` §5.
#[tokio::test]
async fn rfc0046_out_of_band_tenancy_end_to_end() {
    let tmp = tempfile::TempDir::new().expect("temp");
    std::fs::create_dir_all(tmp.path().join("store")).expect("store root");

    // ---- Phase 1: open mode -------------------------------------------
    let s1 = start(tmp.path(), "").await;
    let plain = || export(&[("service.name", "fluxcd")], "line");

    // RFC0046.1 — no selector: 400 naming the header (HTTP), INVALID_ARGUMENT
    // (gRPC); nothing lands.
    let (status, reason) = post_logs(s1.http, &plain(), &[], None).await;
    assert_eq!(status, 400, "{reason}");
    assert!(reason.contains("x-ourios-tenant"), "{reason}");
    let err = grpc_export(
        s1.grpc,
        export_request(&[("service.name", "fluxcd")], "line"),
        &[],
    )
    .await
    .expect_err("no selector over gRPC is rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("x-ourios-tenant"),
        "{}",
        err.message()
    );

    // RFC0046.7 — hygiene: whitespace trimmed; empty, oversize, control
    // char, repeated → 400 / INVALID_ARGUMENT.
    let (status, _) = post_logs(s1.http, &plain(), &[" acme "], None).await;
    assert_eq!(status, 200, "surrounding whitespace is trimmed");
    // (A control character inside a header value never reaches the handler
    // — hyper rejects the request at the parser — so that arm is the
    // selector unit test's; here: empty, oversize, repeated.)
    let long = "x".repeat(257);
    let bad_cases: [&[&str]; 3] = [&[""], &[long.as_str()], &["acme", "acme"]];
    for bad in bad_cases {
        let (status, reason) = post_logs(s1.http, &plain(), bad, None).await;
        assert_eq!(status, 400, "{bad:?}: {reason}");
    }
    let repeated = grpc_export(
        s1.grpc,
        export_request(&[("service.name", "fluxcd")], "line"),
        &["acme", "acme"],
    )
    .await
    .expect_err("repeated metadata is rejected");
    assert_eq!(repeated.code(), tonic::Code::InvalidArgument);

    // RFC0046.3 — three groups (fluxcd, checkout, no service.name at all)
    // in ONE export under selector `acme`; and gRPC lands in the same
    // tenant when it says so.
    let three = ExportLogsServiceRequest {
        resource_logs: [
            export_request(&[("service.name", "fluxcd")], "flux line"),
            export_request(&[("service.name", "checkout")], "checkout line"),
            export_request(&[], "anonymous line"),
        ]
        .into_iter()
        .flat_map(|r| r.resource_logs)
        .collect(),
    };
    let (status, reason) = post_logs(s1.http, &three.encode_to_vec(), &["acme"], None).await;
    assert_eq!(status, 200, "{reason}");
    grpc_export(
        s1.grpc,
        export_request(&[("service.name", "fluxcd")], "grpc line"),
        &["acme"],
    )
    .await
    .expect("gRPC export with selector acks");
    // RFC0046.7 — a selector with reserved characters round-trips.
    let (status, reason) = post_logs(s1.http, &plain(), &["team/eu %1 x"], None).await;
    assert_eq!(status, 200, "{reason}");
    stop(s1).await;

    // ---- Phase 2: auth on, token bound to [acme] -----------------------
    let auth = "auth:\n  tokens:\n    - name: acme-collector\n      token: ${env:RFC0046_TOKEN}\n\
                \x20\x20\x20\x20\x20\x20tenants: [acme]\n";
    let s2 = start(tmp.path(), auth).await;
    // RFC0046.2 — in-set selector acks; out-of-set → 403 / PERMISSION_DENIED
    // (whole export, nothing lands); no bearer → 401.
    let (status, _) = post_logs(s2.http, &plain(), &["acme"], Some("tok-acme")).await;
    assert_eq!(status, 200);
    let (status, _) = post_logs(s2.http, &plain(), &["globex"], Some("tok-acme")).await;
    assert_eq!(status, 403);
    let (status, _) = post_logs(s2.http, &plain(), &["acme"], None).await;
    assert_eq!(status, 401);
    // (gRPC binding is exercised in the ingester suite; the querier below
    // is unchanged — RFC0046.10 — and needs no bearer in this arm because
    // it reads via the same static store.)
    stop(s2).await;

    // ---- Queries (RFC0046.3/.7/.10) ------------------------------------
    let s3 = start(tmp.path(), "").await;
    let q = s3.querier;
    // acme: 3 (three) + 1 (gRPC) + 1 (phase-2 in-set) = 5 rows; the
    // whitespace-trimmed export landed there too → 6.
    assert_eq!(
        rows_for(q, "acme").await,
        6,
        "every acme export, all groups"
    );
    assert_eq!(
        rows_for_dsl(q, "acme", "service == \"fluxcd\"").await,
        4,
        "service.name is a plain attribute inside the tenant"
    );
    assert_eq!(rows_for_dsl(q, "acme", "service == \"checkout\"").await, 1);
    assert_eq!(rows_for(q, "fluxcd").await, 0, "no tenant was ever derived");
    assert_eq!(rows_for(q, "checkout").await, 0);
    assert_eq!(
        rows_for(q, "globex").await,
        0,
        "the denied export never landed"
    );
    assert_eq!(
        rows_for(q, "team/eu %1 x").await,
        1,
        "reserved chars round-trip"
    );
    stop(s3).await;
}
