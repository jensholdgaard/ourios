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
    replicated_with(node_id, (peers, "t=127.0.0.1:1"), tls)
}

/// As [`replicated`], with tail replica `tails`.
fn replicated_with(node_id: &str, (peers, tails): (&str, &str), tls: &str) -> ReplicatedWal {
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
      tail_peers: {tails}
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
        match tokio::time::timeout_at(deadline, client.export(export("replicated line"))).await {
            Ok(Ok(_)) => break true,
            Ok(Err(_)) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Ok(Err(status)) => {
                eprintln!("last refusal: {status:?}");
                break false;
            }
            Err(_) => break false,
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

/// RFC 0026 §3.4: a burst of tenant denials past the audit sink's ceiling
/// flushes at once, not at the next age tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denials_past_the_audit_ceiling_flush_without_waiting_for_the_age() {
    use ourios_core::audit::{AuditEvent, AuditPayload, AuditSink as _};
    use ourios_core::tenant::TenantId;
    use ourios_ingester::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
    use tokio::sync::watch;

    let store = Store::in_memory();
    let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(store.clone(), 2));
    let (stop, stopped) = watch::channel(false);
    let flushing =
        super::serve::spawn_audit_flush(audit.clone(), Duration::from_secs(3_600), stopped);
    let mut sink = audit.clone();
    for _ in 0..3 {
        sink.emit(AuditEvent {
            tenant_id: TenantId::new("acme"),
            timestamp: std::time::SystemTime::now(),
            payload: AuditPayload::IngestDenied {
                token_name: "edge".to_owned(),
            },
        });
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while stored_events(&store) < 3 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the overflow flushes"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    stop.send_replace(true);
    flushing.await.expect("stopped");
}

/// A start that fails to bind its OTLP listeners after the node opened
/// gives back everything it took before returning: its peer listener, and
/// its groups' logs and vote log, so the node starts again at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_that_cannot_bind_otlp_releases_its_peer_listener() {
    let certs = tempfile::tempdir().expect("tempdir");
    let tls = loopback_tls(certs.path());
    let peer = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let peer_addr = peer.local_addr().expect("addr");
    let peers = format!("a={peer_addr},b=127.0.0.1:1,c=127.0.0.1:2");
    let taken = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::in_memory();
    let mut config = receiver(root.path(), &store);
    config.grpc_addr = taken.local_addr().expect("addr");
    let stopping = Arc::new(AtomicBool::new(false));

    let refused = tokio::time::timeout(
        Duration::from_secs(20),
        serve_replicated(&replicated("a", &peers, &tls), config, (peer, stopping)),
    )
    .await
    .expect("returns in time");

    let refusal = refused.err().expect("the gRPC address is taken");
    let taken_addr = taken.local_addr().expect("addr").to_string();
    assert!(refusal.contains(&taken_addr), "{refusal}");
    let rebound = TcpListener::bind(peer_addr)
        .await
        .expect("the peer listener is free");
    let again = serve_replicated(
        &replicated("a", &peers, &tls),
        receiver(root.path(), &store),
        (rebound, Arc::new(AtomicBool::new(false))),
    )
    .await
    .expect("the node starts again at once");
    again.shutdown().await.expect("stops cleanly");
}

/// Voters a, b, c and tail replica t, served on one store; their handles,
/// directories, peer addresses and config strings.
struct Four {
    handles: Vec<ReplicatedHandle>,
    roots: Vec<tempfile::TempDir>,
    addrs: Vec<std::net::SocketAddr>,
    peers: String,
    tails: String,
    tls: String,
    store: Store,
    _certs: tempfile::TempDir,
}

async fn four_served() -> Four {
    let certs = tempfile::tempdir().expect("tempdir");
    let tls = loopback_tls(certs.path());
    let store = Store::in_memory();
    let mut listeners = Vec::new();
    for _ in 0..4 {
        listeners.push(TcpListener::bind("127.0.0.1:0").await.expect("bind"));
    }
    let addrs: Vec<_> = listeners
        .iter()
        .map(|l| l.local_addr().expect("addr"))
        .collect();
    let peers = format!("a={},b={},c={}", addrs[0], addrs[1], addrs[2]);
    let tails = format!("t={}", addrs[3]);
    let (mut handles, mut roots) = (Vec::new(), Vec::new());
    for (name, listener) in ["a", "b", "c", "t"].iter().zip(listeners) {
        let root = tempfile::tempdir().expect("tempdir");
        let wal = replicated_with(name, (&peers, &tails), &tls);
        let config = receiver(root.path(), &store);
        let stopping = Arc::new(AtomicBool::new(false));
        let handle = serve_replicated(&wal, config, (listener, stopping))
            .await
            .expect("served");
        handles.push(handle);
        roots.push(root);
    }
    Four {
        handles,
        roots,
        addrs,
        peers,
        tails,
        tls,
        store,
        _certs: certs,
    }
}

/// Send `count` exports to `handle`, each retried until the group takes it.
async fn exported(handle: &ReplicatedHandle, count: usize) {
    let mut client = LogsServiceClient::connect(format!("http://{}", handle.grpc_addr))
        .await
        .expect("connect");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut sent = 0;
    while sent < count {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the group takes the exports"
        );
        let sending = client.export(export(&format!("line {sent}")));
        let answered = tokio::time::timeout_at(deadline, sending)
            .await
            .expect("the group takes the exports in time");
        match answered {
            Ok(_) => sent += 1,
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// RFC 0065 §6.7: a tail replica restarted with more committed entries past
/// its snapshot than its applied and mined channels hold opens again, its
/// mined output taken while its groups replay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tail_replica_restarted_with_a_backlog_opens_again() {
    let mut four = four_served().await;
    exported(&four.handles[0], 200).await;
    // The tail replica has applied every export before it restarts.
    let leading = four.handles[0].node().host.raft(0).expect("hosted").clone();
    let last = leading
        .wait(Some(Duration::from_secs(10)))
        .metrics(|m| m.last_log_index.is_some(), "the leader's log")
        .await
        .expect("the leader's log")
        .last_log_index
        .expect("entries");
    let tail = four.handles[3].node().host.raft(0).expect("hosted").clone();
    tail.wait(Some(Duration::from_secs(30)))
        .applied_index_at_least(Some(last), "the tail catches up")
        .await
        .expect("the tail catches up");
    drop(tail);

    let restarted = four.handles.pop().expect("the tail");
    restarted.shutdown().await.expect("the tail stops");
    let listener = TcpListener::bind(four.addrs[3]).await.expect("rebind");
    let config = receiver(four.roots[3].path(), &four.store);
    let wal = replicated_with("t", (&four.peers, &four.tails), &four.tls);
    let reopened = tokio::time::timeout(
        Duration::from_secs(30),
        serve_replicated(&wal, config, (listener, Arc::new(AtomicBool::new(false)))),
    )
    .await
    .expect("the tail opens again in time")
    .expect("served");

    reopened.shutdown().await.expect("stops");
    for handle in four.handles {
        handle.shutdown().await.expect("stops");
    }
}

/// How many audit events `store` holds.
fn stored_events(store: &Store) -> usize {
    store
        .list_blocking(None)
        .expect("list")
        .iter()
        .map(|key| {
            let bytes = store.get_blocking(key).expect("get");
            ourios_parquet::AuditReader::open_bytes(bytes.into())
                .expect("open")
                .read_all()
                .expect("read")
                .len()
        })
        .sum()
}
