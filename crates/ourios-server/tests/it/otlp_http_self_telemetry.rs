//! The served binary's own telemetry over OTLP/HTTP.
//!
//! `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` must reach an OTLP/HTTP
//! endpoint: the server once ignored it and kept dialling gRPC, so pointing it
//! at a Collector's `:4318` exported nothing and said nothing. The test spawns
//! the binary with the receiver role on ephemeral ports and its exporters aimed
//! at a stub OTLP/HTTP endpoint, lets it start, then SIGTERMs it: the shutdown
//! flush must deliver the startup logs and the metrics as protobuf POSTs. The
//! exporters run inside the server's tokio runtime, so this also proves the
//! blocking HTTP client never blocks a runtime worker (it would panic).
#![cfg(unix)]

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// The request line and headers of one OTLP/HTTP export, lowercased.
type Head = String;

/// Accept connections forever, forwarding each request head and answering
/// `200` so the exporter counts the batch as delivered.
async fn serve_stub(listener: TcpListener, heads: mpsc::UnboundedSender<Head>) {
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(answer(stream, heads.clone()));
    }
}

/// The head is forwarded before the `200` goes out, so once the server has
/// exited every export it saw acknowledged is already in the channel.
async fn answer(stream: TcpStream, heads: mpsc::UnboundedSender<Head>) -> Option<()> {
    let mut reader = BufReader::new(stream);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        head.push_str(&line);
        if line == "\r\n" {
            break;
        }
    }
    let head = head.to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.ok()?;
    heads.send(head).ok()?;
    reader
        .get_mut()
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await
        .ok()
}

/// The child's telemetry env is set from scratch: every inherited `OTEL_*`
/// variable is dropped, and `RUST_LOG` is pinned because it also filters the
/// OTLP log bridge, so a caller's `RUST_LOG=off` would leave nothing to export.
fn spawn_server(bucket: &std::path::Path, endpoint: &str) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ourios-server"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("OTEL_") {
            command.env_remove(name);
        }
    }
    command
        .env("OURIOS_BUCKET_ROOT", bucket)
        .env("OURIOS_RECEIVER_ENABLED", "1")
        .env("OURIOS_RECEIVER_GRPC_ADDR", "127.0.0.1:0")
        .env("OURIOS_RECEIVER_HTTP_ADDR", "127.0.0.1:0")
        .env("OURIOS_WAL_ROOT", bucket.join("wal"))
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint)
        .env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf")
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ourios-server")
}

/// Wait for the receiver's bound-address line: the server is past telemetry
/// init and has logged its startup events.
async fn await_started(child: &mut Child) {
    let stdout = child.stdout.take().expect("server stdout piped");
    let mut lines = BufReader::new(stdout).lines();
    let started = async {
        while let Some(line) = lines.next_line().await.expect("read server stdout") {
            if line.starts_with("receiver HTTP listening on ") {
                return;
            }
        }
        panic!("server stdout closed before it reported its listeners");
    };
    timeout(Duration::from_secs(30), started)
        .await
        .expect("server starts before timeout");
}

async fn sigterm_and_wait(child: &mut Child) {
    let pid = child.id().expect("server pid");
    let killed = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .await
        .expect("run kill -TERM");
    assert!(killed.success(), "kill -TERM failed: {killed:?}");
    let status = timeout(Duration::from_secs(30), child.wait())
        .await
        .expect("server exits before timeout")
        .expect("await server exit");
    assert!(status.success(), "clean shutdown, got {status:?}");
}

fn assert_exported(heads: &[Head], path: &str) {
    let export = heads
        .iter()
        .find(|head| head.starts_with(&format!("post {path} ")))
        .unwrap_or_else(|| panic!("no POST {path} reached the endpoint, got {heads:?}"));
    assert!(
        export.contains("\r\ncontent-type: application/x-protobuf\r\n"),
        "{export}"
    );
}

#[tokio::test]
async fn http_protobuf_self_telemetry_reaches_an_otlp_http_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
    let endpoint = format!("http://{}", listener.local_addr().expect("stub addr"));
    let (sender, mut received) = mpsc::unbounded_channel();
    let stub = tokio::spawn(serve_stub(listener, sender));

    let bucket = tempfile::TempDir::new().expect("temp");
    let mut child = spawn_server(bucket.path(), &endpoint);
    await_started(&mut child).await;
    sigterm_and_wait(&mut child).await;
    stub.abort();

    let mut heads = Vec::new();
    while let Ok(head) = received.try_recv() {
        heads.push(head);
    }
    assert_exported(&heads, "/v1/logs");
    assert_exported(&heads, "/v1/metrics");
}
