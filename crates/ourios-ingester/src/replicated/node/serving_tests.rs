//! RFC 0065 §6.8, B21: started nodes serve replication over mutual TLS and
//! answer each other's forwarded exports, so an export sent to a follower
//! commits on its group's leader.

use std::collections::BTreeMap;
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, any_value::Value};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use ourios_core::tenant::TenantId;
use ourios_parquet::Store;
use ourios_serving::AuthResolver;
use ourios_serving::tls::ALPN_GRPC;
use ourios_serving::tls_serve::{LISTENER_GRPC, reloading_acceptor};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tonic::transport::server::TcpIncoming;

use super::super::ingest::Ingested;
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

/// Three nodes started on one store, each serving over mutual TLS with a
/// certificate from one CA; their shutdown senders.
async fn three_served() -> (Vec<Node>, Vec<oneshot::Sender<()>>, Vec<tempfile::TempDir>) {
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
    let (mut nodes, mut stops, mut roots) = (Vec::new(), Vec::new(), Vec::new());
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
        let incoming = TcpIncoming::from(listener);
        drop(node.serve(
            AuthResolver::static_only(None),
            (incoming, acceptor),
            shutdown,
        ));
        nodes.push(node);
        stops.push(stop);
        roots.push(root);
    }
    roots.push(certs);
    (nodes, stops, roots)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_export_to_a_follower_commits_on_the_leader_over_mutual_tls() {
    let (nodes, stops, _roots) = three_served().await;
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
    let follower = nodes.iter().find(|n| n.id != leader).expect("a follower");

    let ingested = follower.ingest().ingest_with(export("served"), None).await;

    assert!(
        matches!(ingested, Ok(Ingested::Committed { .. })),
        "{ingested:?}"
    );
    for stop in stops {
        let _ = stop.send(());
    }
    for node in nodes {
        node.host.shutdown().await.expect("shutdown");
    }
}
