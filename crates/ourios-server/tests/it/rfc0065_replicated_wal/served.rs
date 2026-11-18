//! Serving the `ourios-server` binary with the replicated WAL backend.

use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value::Value};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;

/// The attribute naming each [`numbered`] record.
pub(super) const SEQ: &str = "seq";

/// A served replicated receiver.
pub(super) struct Server {
    child: Child,
    pub(super) grpc: SocketAddr,
    /// Its stderr, echoed as it comes.
    stderr: tokio::task::JoinHandle<Vec<String>>,
}

impl Server {
    /// SIGKILL it: nothing it holds unsynced survives.
    pub(super) async fn kill(mut self) {
        self.child.start_kill().expect("SIGKILL");
        let _ = self.child.wait().await;
    }

    /// SIGTERM it and require a clean exit.
    pub(super) async fn terminate_cleanly(mut self) {
        let pid = self.child.id().expect("pid");
        let killed = tokio::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .await
            .expect("kill");
        assert!(killed.success(), "kill -TERM: {killed:?}");
        let status = tokio::time::timeout(Duration::from_secs(30), self.child.wait())
            .await
            .expect("exits in time")
            .expect("exit status");
        assert!(status.success(), "a clean exit: {status:?}");
        let logged = self.stderr.await.expect("stderr read");
        let failed: Vec<_> = logged
            .iter()
            .filter(|line| line.contains("receiver shutdown error"))
            .collect();
        assert!(failed.is_empty(), "its shutdown reported {failed:?}");
    }
}

/// A loopback address free when asked.
pub(super) fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr")
}

/// One CA, and one leaf it signed for 127.0.0.1 that every peer
/// presents, as `receiver.wal.replicated.tls`'s leaves.
pub(super) fn peer_tls(dir: &Path) -> String {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};

    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate().expect("key");
    let ca = params.self_signed(&ca_key).expect("ca");
    let issuer = Issuer::new(params, ca_key);
    let key = KeyPair::generate().expect("key");
    let leaf = CertificateParams::new(vec!["127.0.0.1".to_owned()])
        .expect("params")
        .signed_by(&key, &issuer)
        .expect("leaf");
    let write = |name: &str, pem: String| {
        let path = dir.join(name);
        std::fs::write(&path, pem).expect("write");
        path.display().to_string()
    };
    let (cert, key, ca) = (
        write("peer.crt", leaf.pem()),
        write("peer.key", key.serialize_pem()),
        write("ca.crt", ca.pem()),
    );
    format!(
        "      tls:\n        cert_file: {cert}\n        key_file: {key}\n        \
         client_ca_file: {ca}\n"
    )
}

/// Start node `name` on the S3 store `(endpoint, bucket)`, its
/// `receiver.wal.replicated` leaves `leaves`; its own `wal_root` and
/// config file go under `dir`. It flushes and compacts every second.
pub(super) async fn start(
    dir: &Path,
    name: &str,
    (endpoint, bucket): (&str, &str),
    leaves: &str,
) -> Server {
    let wal_root = dir.join(format!("wal-{name}"));
    let config = format!(
        "storage:\n  backend: s3\n  s3:\n    bucket: {bucket}\n    endpoint: {endpoint}\n    \
         region: us-east-1\ncompaction:\n  interval_secs: 1\nreceiver:\n  enabled: true\n  \
         grpc_addr: 127.0.0.1:0\n  http_addr: 127.0.0.1:0\n  max_buffer_age_secs: 1\n  \
         wal_root: {}\n  wal:\n    backend: replicated\n    replicated:\n{leaves}",
        wal_root.display()
    );
    std::fs::write(config_path(dir, name), config).expect("config");
    restart(dir, name).await
}

fn config_path(dir: &Path, name: &str) -> std::path::PathBuf {
    dir.join(format!("{name}.yaml"))
}

/// Start node `name` again, on the configuration and `wal_root` [`start`]
/// gave it under `dir`.
pub(super) async fn restart(dir: &Path, name: &str) -> Server {
    let mut child = crate::server_bin::command()
        .arg("--config")
        .arg(config_path(dir, name))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn");
    let stderr = echoed(&mut child);
    let grpc = announced(&mut child, "receiver gRPC listening on ").await;
    Server {
        child,
        grpc,
        stderr,
    }
}

/// `child`'s stderr lines, each echoed to the test's own as it comes.
fn echoed(child: &mut Child) -> tokio::task::JoinHandle<Vec<String>> {
    let mut lines = BufReader::new(child.stderr.take().expect("piped")).lines();
    tokio::spawn(async move {
        let mut logged = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("{line}");
            logged.push(line);
        }
        logged
    })
}

/// The address the server announces after `prefix`. Its stdout is read
/// to the end in the background, so no later line meets a closed pipe.
async fn announced(child: &mut Child, prefix: &str) -> SocketAddr {
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
    let read = async {
        loop {
            let line = lines
                .next_line()
                .await
                .expect("read stdout")
                .expect("stdout open until the address");
            if let Some(rest) = line.strip_prefix(prefix) {
                return rest.trim().parse().expect("an address");
            }
        }
    };
    let addr = tokio::time::timeout(Duration::from_secs(60), read)
        .await
        .expect("announced in time");
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    addr
}

/// Whether an export to `grpc` is acknowledged within a minute,
/// retried while the group forms.
pub(super) async fn exported(grpc: SocketAddr) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let Ok(mut client) = LogsServiceClient::connect(format!("http://{grpc}")).await else {
        return false;
    };
    while tokio::time::Instant::now() < deadline {
        let sending = client.export(numbered(0, 0));
        match tokio::time::timeout_at(deadline, sending).await {
            Ok(Ok(_)) => return true,
            Ok(Err(_)) => tokio::time::sleep(Duration::from_millis(200)).await,
            Err(_) => return false,
        }
    }
    false
}

/// An export of one record of tenant `acme` at `time_unix_nano`, its
/// `seq` attribute `seq`.
pub(super) fn numbered(seq: u64, time_unix_nano: u64) -> tonic::Request<ExportLogsServiceRequest> {
    let text = |s: String| {
        Some(AnyValue {
            value: Some(Value::StringValue(s)),
        })
    };
    let record = LogRecord {
        time_unix_nano,
        body: text("replicated line".to_owned()),
        attributes: vec![KeyValue {
            key: SEQ.to_owned(),
            value: text(seq.to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut request = tonic::Request::new(ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            scope_logs: vec![ScopeLogs {
                log_records: vec![record],
                ..Default::default()
            }],
            ..Default::default()
        }],
    });
    request
        .metadata_mut()
        .insert("x-ourios-tenant", "acme".parse().expect("ascii"));
    request
}
