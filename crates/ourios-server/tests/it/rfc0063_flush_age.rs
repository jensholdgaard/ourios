//! RFC 0063 — configurable receiver flush age.
//! See `docs/rfcs/0063-configurable-flush-age.md` §5.
//!
//! RFC0063.1 resolves the field through both front-ends, and RFC0063.5
//! spawns the served binary with a short age. RFC0063.2–.4 test private
//! receiver and compactor builders, so they live beside them in
//! `src/receiver.rs` and `src/main.rs`; the Helm arm of RFC0063.1 is in
//! `deploy/helm/render-tests.sh`.
#![cfg(unix)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_server::config::file::parse;
use ourios_server::config::resolve::{
    ReceiverInputs, build_receiver_config, server_config_from_file,
};
use prost::Message;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

const KEY: &str = "receiver.max_buffer_age_secs";

fn from_env(raw: Option<&str>) -> Result<Duration, String> {
    build_receiver_config(ReceiverInputs {
        enabled: Some("1"),
        wal_root: Some(PathBuf::from("/wal")),
        max_buffer_age_secs: raw,
        ..ReceiverInputs::default()
    })
    .map(|params| params.expect("enabled").max_buffer_age)
}

const CHART_LINE: &str =
    "  max_buffer_age_secs: \"${env:OURIOS_RECEIVER_MAX_BUFFER_AGE_SECS:-}\"\n";

fn from_file(field: &str) -> Result<Duration, String> {
    from_file_with_env(field, None)
}

fn from_file_with_env(field: &str, age_env: Option<&str>) -> Result<Duration, String> {
    let yaml = format!(
        "storage:\n  local:\n    bucket_root: /tmp/ourios\n\
         receiver:\n  enabled: true\n  wal_root: /wal\n{field}"
    );
    let lookup = |name: &str| {
        (name == "OURIOS_RECEIVER_MAX_BUFFER_AGE_SECS")
            .then(|| age_env.map(str::to_owned))
            .flatten()
    };
    let file = parse(&yaml, &lookup).map_err(|e| e.to_string())?;
    let config = server_config_from_file(&file)?;
    Ok(config.receiver.expect("receiver enabled").max_buffer_age)
}

/// Scenario RFC0063.1 — the age is configurable and validated.
/// See `docs/rfcs/0063-configurable-flush-age.md` §5.
#[test]
fn rfc0063_1_the_age_is_configurable_and_validated() {
    let default = Duration::from_secs(300);
    assert_eq!(from_env(None), Ok(default));
    assert_eq!(from_env(Some("")), Ok(default));
    assert_eq!(from_env(Some("10")), Ok(Duration::from_secs(10)));
    assert_eq!(from_file(""), Ok(default));
    assert_eq!(from_file("  max_buffer_age_secs: \"\"\n"), Ok(default));
    assert_eq!(
        from_file("  max_buffer_age_secs: 10\n"),
        Ok(Duration::from_secs(10))
    );
    // The Helm chart's rendered reference (deploy/helm/render-tests.sh
    // pins the line): the extraEnv value when set, the default when not.
    assert_eq!(
        from_file_with_env(CHART_LINE, Some("10")),
        Ok(Duration::from_secs(10))
    );
    assert_eq!(from_file_with_env(CHART_LINE, None), Ok(default));
    for bad in ["0", "301", "ten"] {
        for err in [
            from_env(Some(bad)).expect_err(bad),
            from_file(&format!("  max_buffer_age_secs: {bad}\n")).expect_err(bad),
        ] {
            assert!(err.contains(KEY), "the error names the field: {err}");
            assert!(err.contains("1..=300"), "the error names the range: {err}");
        }
    }
}

fn string_value(s: &str) -> AnyValue {
    AnyValue {
        value: Some(Value::StringValue(s.to_owned())),
    }
}

fn now_ns() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    u64::try_from(nanos).unwrap_or(0)
}

fn export_request(lines: usize) -> ExportLogsServiceRequest {
    let log_records = (0..lines)
        .map(|i| LogRecord {
            time_unix_nano: now_ns(),
            body: Some(string_value(&format!("order {i} placed"))),
            ..Default::default()
        })
        .collect();
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![KeyValue {
                    key: "service.name".to_owned(),
                    value: Some(string_value("checkout")),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records,
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

async fn post(addr: SocketAddr, path: &str, content_type: &str, body: &[u8]) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nX-Ourios-Tenant: checkout\r\n\
         Content-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len(),
    );
    stream.write_all(head.as_bytes()).await.expect("write head");
    stream.write_all(body).await.expect("write body");
    stream.flush().await.ok();
    crate::raw_http::read_response(&mut stream).await
}

fn rows(response: &str) -> u64 {
    let body = response
        .split("\r\n\r\n")
        .nth(1)
        .expect("response has a body");
    let json: serde_json::Value = serde_json::from_str(body).expect("body is JSON");
    json["rows"].as_u64().unwrap_or(0)
}

/// Scenario RFC0063.5 — a lowered age makes records queryable sooner.
/// See `docs/rfcs/0063-configurable-flush-age.md` §5.
#[tokio::test]
async fn rfc0063_5_a_lowered_age_makes_records_queryable_sooner() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let mut child = crate::server_bin::command()
        .env("OURIOS_BUCKET_ROOT", tmp.path().join("store"))
        .env("OURIOS_RECEIVER_ENABLED", "1")
        .env("OURIOS_RECEIVER_GRPC_ADDR", "127.0.0.1:0")
        .env("OURIOS_RECEIVER_HTTP_ADDR", "127.0.0.1:0")
        .env("OURIOS_WAL_ROOT", tmp.path().join("wal"))
        .env("OURIOS_RECEIVER_MAX_BUFFER_AGE_SECS", "2")
        .env("OURIOS_QUERIER_ENABLED", "1")
        .env("OURIOS_QUERIER_HTTP_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ourios-server");

    let stdout = child.stdout.take().expect("server stdout piped");
    let mut lines = BufReader::new(stdout).lines();
    let mut receiver: Option<SocketAddr> = None;
    let mut querier: Option<SocketAddr> = None;
    let read_addrs = async {
        while receiver.is_none() || querier.is_none() {
            let line = lines
                .next_line()
                .await
                .expect("read server stdout")
                .expect("server stdout closed before reporting addresses");
            if let Some(rest) = line.strip_prefix("receiver HTTP listening on ") {
                receiver = Some(rest.trim().parse().expect("parse receiver addr"));
            } else if let Some(rest) = line.strip_prefix("querier HTTP listening on ") {
                querier = Some(rest.trim().parse().expect("parse querier addr"));
            }
        }
    };
    timeout(Duration::from_secs(15), read_addrs)
        .await
        .expect("server reports its bound addresses before timeout");
    let (receiver, querier) = (receiver.expect("receiver"), querier.expect("querier"));

    let ack = post(
        receiver,
        "/v1/logs",
        "application/x-protobuf",
        &export_request(3).encode_to_vec(),
    )
    .await;
    assert!(ack.starts_with("HTTP/1.1 200"), "export is acked: {ack:?}");
    let acked = Instant::now();

    let dsl = b"service == \"checkout\"";
    let poll = async {
        loop {
            if rows(&post(querier, "/v1/query", "text/plain", dsl).await) == 3 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    timeout(
        Duration::from_secs(15).saturating_sub(acked.elapsed()),
        poll,
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the acked records are queryable within 15 s, not ~330 s ({:?} elapsed)",
            acked.elapsed()
        )
    });
}
