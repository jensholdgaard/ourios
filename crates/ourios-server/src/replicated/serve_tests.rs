//! RFC0065.1 in process: three replicated receivers on one store form
//! their group, and an OTLP export to any of them is acknowledged once the
//! group commits it.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;
use opentelemetry_proto::tonic::common::v1::{AnyValue, any_value::Value};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use ourios_parquet::Store;
use ourios_server::config::wal::{ReplicatedWal, WalBackend};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use tokio::net::TcpListener;

use super::serve::{ReplicatedHandle, serve_replicated};
use super::tests::receiver;

const NAMES: [&str; 3] = ["a", "b", "c"];

/// A CA and a loopback certificate it signed, under `dir`, as a `tls:` block.
fn loopback_tls(dir: &Path) -> String {
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
        "      tls:\n        cert_file: {cert}\n        key_file: {key}\n        client_ca_file: {ca}\n"
    )
}

/// Node `node_id`'s replicated config, its peers at `peers`, over one group.
fn replicated(node_id: &str, peers: &str, tls: &str) -> ReplicatedWal {
    let yaml = format!(
        "\
storage:
  local:
    bucket_root: /data
receiver:
  enabled: true
  wal_root: /wal
  wal:
    backend: replicated
    replicated:
      node_id: {node_id}
      groups: 1
      peers: {peers}
      tail_peers: t=127.0.0.1:1
      tail_clients: querier
{tls}"
    );
    let file = ourios_server::config::file::parse(&yaml, &|_| None).expect("parses");
    let config = ourios_server::config::resolve::server_config_from_file(&file).expect("resolves");
    match config.receiver.expect("enabled").wal {
        WalBackend::Replicated(replicated) => *replicated,
        WalBackend::Local => panic!("a replicated backend"),
    }
}

fn export(line: &str) -> tonic::Request<ExportLogsServiceRequest> {
    let record = LogRecord {
        body: Some(AnyValue {
            value: Some(Value::StringValue(line.to_owned())),
        }),
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

/// RFC0065.1: three receivers serving the replicated backend form their
/// group, take an export over OTLP gRPC once it has, and stop cleanly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_replicated_receivers_take_an_export_and_stop() {
    let certs = tempfile::tempdir().expect("tempdir");
    let tls = loopback_tls(certs.path());
    let store = Store::in_memory();
    let mut listeners = Vec::new();
    for _ in NAMES {
        listeners.push(TcpListener::bind("127.0.0.1:0").await.expect("bind"));
    }
    let peers = NAMES
        .iter()
        .zip(&listeners)
        .map(|(name, l)| format!("{name}={}", l.local_addr().expect("addr")))
        .collect::<Vec<_>>()
        .join(",");
    let mut handles: Vec<ReplicatedHandle> = Vec::new();
    let mut roots = Vec::new();
    for (name, listener) in NAMES.iter().zip(listeners) {
        let root = tempfile::tempdir().expect("tempdir");
        let config = receiver(root.path(), &store);
        let stopping = Arc::new(AtomicBool::new(false));
        let handle = serve_replicated(
            &replicated(name, &peers, &tls),
            config,
            (listener, stopping),
        )
        .await
        .expect("served");
        handles.push(handle);
        roots.push(root);
    }

    for handle in &handles {
        assert_ne!(
            handle.http_addr.port(),
            0,
            "the OTLP/HTTP listener is bound"
        );
    }
    let mut client = LogsServiceClient::connect(format!("http://{}", handles[0].grpc_addr))
        .await
        .expect("connect");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let accepted = loop {
        match client.export(export("replicated line")).await {
            Ok(_) => break true,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(status) => {
                eprintln!("last refusal: {status:?}");
                break false;
            }
        }
    };
    assert!(accepted, "an export is acknowledged once the group forms");

    for handle in handles {
        tokio::time::timeout(Duration::from_secs(20), handle.shutdown())
            .await
            .expect("stops in time")
            .expect("stops cleanly");
    }
}
