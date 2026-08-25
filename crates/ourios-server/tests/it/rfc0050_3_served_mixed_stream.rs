//! Scenario RFC0050.3 — a mixed stream works, through the served
//! binary.
//!
//! The server is spawned with a **config file** selecting
//! `miner.upstream_templates: adopt` (the RFC 0050 dial, slice 4);
//! one gRPC export carries an annotated record (a valid
//! `log.record.template`) and a bare one. After a graceful shutdown
//! drains the sinks, a second server process (querier role, same
//! bucket) answers `POST /v1/query`: both records return in one
//! tenant, the annotated one under its adopted template string, the
//! bare one under its mined template, with distinct `template_id`s
//! — and the attributes arrays stay exactly what the producers sent
//! (RFC 0018 fidelity / RFC0050.8 end to end).
//!
//! See `docs/rfcs/0050-upstream-derived-templates.md` §5.
#![cfg(unix)]

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::timeout;

/// 2026-04-02T10:58:00Z — inside the query's `-365d` look-back.
const TS: u64 = 1_775_127_480_000_000_000;

fn string_value(s: &str) -> AnyValue {
    AnyValue {
        value: Some(Value::StringValue(s.to_owned())),
    }
}

/// One two-record `acme` batch: `annotated_body` carrying the
/// `log.record.template` claim, `bare_body` carrying nothing.
fn mixed_request(
    annotated_body: &str,
    template: &str,
    bare_body: &str,
) -> ExportLogsServiceRequest {
    let annotated = LogRecord {
        body: Some(string_value(annotated_body)),
        severity_number: 9,
        time_unix_nano: TS,
        attributes: vec![KeyValue {
            key: "log.record.template".to_owned(),
            value: Some(string_value(template)),
            ..Default::default()
        }],
        ..Default::default()
    };
    let bare = LogRecord {
        body: Some(string_value(bare_body)),
        severity_number: 9,
        time_unix_nano: TS + 1,
        ..Default::default()
    };
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![KeyValue {
                    key: "service.name".to_owned(),
                    value: Some(string_value("acme")),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records: vec![annotated, bare],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

/// Spawn `ourios-server --config <path>` and collect the "listening
/// on" lines until every prefix in `expected` has an address.
async fn spawn_with_config(
    config: &std::path::Path,
    expected: &[&str],
) -> (Child, Vec<SocketAddr>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .arg("--config")
        .arg(config)
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ourios-server");
    let stdout = child.stdout.take().expect("server stdout piped");
    let mut lines = BufReader::new(stdout).lines();
    let mut addrs: Vec<Option<SocketAddr>> = vec![None; expected.len()];
    let read = async {
        while addrs.iter().any(Option::is_none) {
            let line = lines
                .next_line()
                .await
                .expect("read server stdout")
                .expect("server stdout closed before reporting addresses");
            for (i, prefix) in expected.iter().enumerate() {
                if let Some(rest) = line.strip_prefix(prefix) {
                    addrs[i] = Some(rest.trim().parse().expect("parse addr"));
                }
            }
        }
    };
    timeout(Duration::from_secs(15), read)
        .await
        .expect("server reports its bound addresses before timeout");
    (child, addrs.into_iter().map(|a| a.expect("addr")).collect())
}

async fn sigterm_and_wait(mut child: Child) {
    let pid = child.id().expect("server pid");
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .await
        .expect("run kill -TERM");
    assert!(status.success(), "kill -TERM succeeded");
    let exit = timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("server exits before timeout")
        .expect("await server exit");
    assert!(exit.success(), "graceful shutdown exits cleanly: {exit:?}");
}

/// Hand-rolled `POST /v1/query` (text DSL), returning the JSON body.
async fn post_query(addr: SocketAddr, dsl: &str) -> serde_json::Value {
    let mut stream = TcpStream::connect(addr).await.expect("connect querier");
    let head = format!(
        "POST /v1/query HTTP/1.1\r\nHost: {addr}\r\nContent-Type: text/plain\r\n\
         X-Ourios-Tenant: acme\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        dsl.len(),
    );
    stream.write_all(head.as_bytes()).await.expect("write head");
    stream.write_all(dsl.as_bytes()).await.expect("write body");
    stream.flush().await.ok();
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    let text = String::from_utf8_lossy(&response);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .expect("response has a header/body split");
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "query returns 200, got {:?}",
        head.lines().next(),
    );
    serde_json::from_str(body.trim()).expect("JSON body")
}

#[tokio::test]
async fn rfc0050_3_mixed_stream_through_the_served_binary() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let bucket = tmp.path().join("bucket");
    std::fs::create_dir(&bucket).expect("bucket dir");
    let config_path = tmp.path().join("ourios.yaml");
    std::fs::write(
        &config_path,
        format!(
            "\
storage:
  local:
    bucket_root: {bucket}
receiver:
  enabled: true
  grpc_addr: 127.0.0.1:0
  http_addr: 127.0.0.1:0
  wal_root: {wal}
querier:
  enabled: true
  http_addr: 127.0.0.1:0
miner:
  upstream_templates: adopt
",
            bucket = bucket.display(),
            wal = tmp.path().join("wal").display(),
        ),
    )
    .expect("write config");

    // First process: receiver + querier under the adopt dial.
    let (child, addrs) = spawn_with_config(
        &config_path,
        &["receiver gRPC listening on ", "querier HTTP listening on "],
    )
    .await;
    let grpc_addr = addrs[0];

    let mut grpc = LogsServiceClient::connect(format!("http://{grpc_addr}"))
        .await
        .expect("connect gRPC");
    let mut export = tonic::Request::new(mixed_request(
        "job 7 finished",
        "job <*> finished",
        "cache warmed fully",
    ));
    export
        .metadata_mut()
        .insert("x-ourios-tenant", "acme".parse().expect("ascii"));
    grpc.export(export).await.expect("mixed export acks");

    // Graceful shutdown drains the data + audit sinks to the store.
    sigterm_and_wait(child).await;

    // Second process over the same bucket answers the query.
    let (child, addrs) = spawn_with_config(&config_path, &["querier HTTP listening on "]).await;
    let json = post_query(addrs[0], "true | range(-365d, now) | limit 100").await;
    sigterm_and_wait(child).await;

    let records = json["records"].as_array().expect("records array");
    assert_eq!(
        records.len(),
        2,
        "both records return in one tenant: {json}"
    );
    let by_line = |line: &str| {
        records
            .iter()
            .find(|r| r["body"]["line"] == line)
            .unwrap_or_else(|| panic!("record {line:?} present: {json}"))
    };
    let adopted = by_line("job 7 finished");
    let mined = by_line("cache warmed fully");

    // The annotated record adopted the upstream string; the bare one
    // was mined; distinct ids, both queryable in one response.
    assert_eq!(adopted["template"], "job <*> finished");
    assert_eq!(mined["template"], "cache warmed fully");
    assert_ne!(adopted["template_id"], mined["template_id"]);

    // RFC 0018 fidelity end to end: the producer-sent attribute
    // survives verbatim; the bare record gained nothing.
    let adopted_attrs = adopted["attributes"].to_string();
    assert!(
        adopted_attrs.contains("log.record.template"),
        "producer-sent attribute survives: {adopted_attrs}",
    );
    assert_eq!(
        mined["attributes"].as_array().map(Vec::len),
        Some(0),
        "no derived attribute is injected: {}",
        mined["attributes"],
    );

    // Both rows reconstruct faithfully (adoption is alignment-gated).
    assert_eq!(adopted["body"]["reconstruction"], "faithful");
    assert_eq!(mined["body"]["reconstruction"], "faithful");
}
