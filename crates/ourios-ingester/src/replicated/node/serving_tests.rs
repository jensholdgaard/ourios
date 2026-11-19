//! RFC 0065 §6.8, B21: started nodes serve replication over mutual TLS and
//! answer each other's forwarded exports, so an export sent to a follower
//! commits on its group's leader.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, any_value::Value};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use ourios_core::audit::{AuditPayload, AuditSink, SharedAuditSink};
use ourios_core::auth::{TokenSpec, build_token_store};
use ourios_core::tenant::TenantId;
use ourios_parquet::Store;
use ourios_serving::AuthResolver;
use ourios_serving::tls::ALPN_GRPC;
use ourios_serving::tls_serve::{LISTENER_GRPC, reloading_acceptor};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tonic::transport::server::TcpIncoming;

use super::super::ingest::Ingested;
use super::super::raft::cluster::write;
use super::super::raft::transport::Peers;
use super::super::raft::transport::tests::mtls::Ca;
use super::super::raft::types::NodeId;
use super::tests::n0_named;
use super::{Node, start};
use crate::receiver::Export;

const NAMES: [&str; 3] = ["n0", "n1", "n2"];

fn export(line: &str) -> Export<'static> {
    let record = LogRecord {
        body: Some(AnyValue {
            value: Some(Value::StringValue(line.to_owned())),
        }),
        ..Default::default()
    };
    Export {
        request: ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                scope_logs: vec![ScopeLogs {
                    log_records: vec![record],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        },
        tenant: TenantId::new("acme"),
        binding: None,
        lenient_json: false,
    }
}

/// Three nodes serving, their shutdown senders, and the directories they
/// keep.
pub(super) struct Served {
    pub(super) nodes: Vec<Node>,
    /// The store they share.
    pub(super) store: Store,
    stops: Vec<oneshot::Sender<()>>,
    /// Kept for the nodes' lifetime: dropping one deletes it.
    dirs: Vec<tempfile::TempDir>,
}

impl Served {
    pub(super) async fn stop(self) {
        for stop in self.stops {
            let _ = stop.send(());
        }
        for node in self.nodes {
            node.host.shutdown().await.expect("shutdown");
        }
    }
}

/// Three nodes started on one store, each serving over mutual TLS with a
/// certificate from one CA, authenticating forwards with `auth` and
/// recording tenant denials on `denials`.
pub(super) async fn three_served(auth: &AuthResolver, denials: &SharedAuditSink) -> Served {
    let ca = Ca::new();
    let store = Store::in_memory();
    let certs = tempfile::tempdir().expect("tempdir");
    let mut listeners = Vec::new();
    for _ in NAMES {
        listeners.push(TcpListener::bind("127.0.0.1:0").await.expect("bind"));
    }
    let addresses: Vec<(String, String)> = NAMES
        .iter()
        .zip(&listeners)
        .map(|(name, l)| {
            (
                (*name).to_owned(),
                l.local_addr().expect("addr").to_string(),
            )
        })
        .collect();
    let mut served = Served {
        nodes: Vec::new(),
        store: store.clone(),
        stops: Vec::new(),
        dirs: Vec::new(),
    };
    for (name, listener) in NAMES.iter().zip(listeners) {
        let tls = ca.node(certs.path(), name);
        let peers = Peers::mutual_tls(&tls)
            .expect("peers")
            .with_addresses(addresses.clone());
        let root = tempfile::tempdir().expect("tempdir");
        let node = start(n0_named(name, root.path(), &store, peers), &mut |_| {})
            .await
            .expect("started");
        let acceptor = reloading_acceptor(&tls, ALPN_GRPC, LISTENER_GRPC).expect("acceptor");
        let (stop, stopped) = oneshot::channel::<()>();
        let shutdown = async {
            let _ = stopped.await;
        };
        let answer = (
            auth.clone(),
            Box::new(denials.clone()) as Box<dyn AuditSink + Send>,
        );
        drop(node.serve(answer, (TcpIncoming::from(listener), acceptor), shutdown));
        served.nodes.push(node);
        served.stops.push(stop);
        served.dirs.push(root);
    }
    served.dirs.push(certs);
    served
}

/// Form the nodes' group 0 and return the index of a follower.
pub(super) async fn formed(nodes: &[Node]) -> usize {
    let members: BTreeMap<NodeId, String> = nodes
        .iter()
        .zip(NAMES)
        .map(|(node, name)| (node.id, name.to_owned()))
        .collect();
    let first = nodes[0].host.raft(0).expect("hosted");
    first.initialize(members).await.expect("initialize");
    let leader = first
        .wait(Some(Duration::from_secs(10)))
        .metrics(|m| m.current_leader.is_some(), "a leader")
        .await
        .expect("a leader")
        .current_leader
        .expect("a leader");
    // The leader takes writes once its own first entry commits, and a
    // follower routes to it once it knows it leads.
    let leading = nodes.iter().find(|n| n.id == leader).expect("the leader");
    write(leading.host.raft(0).expect("hosted"), 0).await;
    let follower = nodes
        .iter()
        .position(|n| n.id != leader)
        .expect("a follower");
    nodes[follower]
        .host
        .raft(0)
        .expect("hosted")
        .wait(Some(Duration::from_secs(10)))
        .metrics(
            |m| m.current_leader == Some(leader),
            "the follower knows the leader",
        )
        .await
        .expect("the follower knows the leader");
    follower
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_export_to_a_follower_commits_on_the_leader_over_mutual_tls() {
    let served = three_served(&AuthResolver::static_only(None), &SharedAuditSink::new()).await;
    let follower = formed(&served.nodes).await;

    let ingested = served.nodes[follower]
        .ingest()
        .ingest_with(export("served"), None)
        .await;

    assert!(
        matches!(ingested, Ok(Ingested::Committed { .. })),
        "{ingested:?}"
    );
    served.stop().await;
}

/// RFC 0065 §6.7: a served node answers `ReadIndex` over mutual TLS, the
/// leader with a read point and a follower by naming the leader.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_served_node_answers_read_points_over_mutual_tls() {
    use openraft::async_runtime::WatchReceiver as _;

    use crate::replicated::raft::transport::read_index::Outcome;

    let served = three_served(&AuthResolver::static_only(None), &SharedAuditSink::new()).await;
    let follower = formed(&served.nodes).await;
    let asking = &served.nodes[follower].peers;
    let leader = served.nodes[follower]
        .host
        .raft(0)
        .expect("hosted")
        .metrics()
        .borrow_watched()
        .current_leader
        .expect("a leader");
    let leader = served
        .nodes
        .iter()
        .position(|n| n.id == leader)
        .expect("the leader");
    let patience = Duration::from_secs(5);

    let read = asking.read_index(NAMES[leader], 0, patience).await;
    assert!(matches!(read, Outcome::Read(_)), "{read:?}");
    // A follower can know the leader's id before it can give its node: it
    // then names none, which is a correct answer, so it is asked again
    // until it names one (#1379).
    let deadline = tokio::time::Instant::now() + patience;
    let named = loop {
        let named = asking.read_index(NAMES[follower], 0, patience).await;
        let unnamed = matches!(named, Outcome::NotLeader { leader: None });
        if !unnamed || tokio::time::Instant::now() >= deadline {
            break named;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        named,
        Outcome::NotLeader {
            leader: Some(NAMES[leader].to_owned())
        }
    );
    served.stop().await;
}

/// RFC 0026 §3.4: the leader that denies a forwarded export for its tenant
/// binding records the `IngestDenied` audit event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forwarded_export_the_leader_denies_is_audited() {
    let tokens = build_token_store(Some(&[TokenSpec {
        name: Some("edge".to_owned()),
        token: Some("tok-other".to_owned()),
        tenants: vec!["other".to_owned()],
    }]))
    .expect("valid spec")
    .expect("enabled store");
    let auth = AuthResolver::static_only(Some(Arc::new(tokens)));
    let denials = SharedAuditSink::new();
    let served = three_served(&auth, &denials).await;
    let follower = formed(&served.nodes).await;

    let denied = served.nodes[follower]
        .ingest()
        .ingest_with(export("denied"), Some("Bearer tok-other"))
        .await;

    assert!(denied.is_err(), "{denied:?}");
    let events = denials.drain();
    assert!(
        events
            .iter()
            .any(|e| matches!(e.payload, AuditPayload::IngestDenied { .. })),
        "{events:?}"
    );
    served.stop().await;
}
