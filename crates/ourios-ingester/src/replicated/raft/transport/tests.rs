//! Three group members over real loopback gRPC, and the server's
//! refusals.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use openraft::errors::RPCError;
use openraft::network::{RPCOption, RaftNetworkFactory, v2::RaftNetworkV2};
use openraft::raft::VoteRequest;
use openraft::{Raft, RaftMetrics};
use ourios_wal::TenantBatch;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::transport::server::TcpIncoming;
use tonic::{Code, Request, Status};
use tonic_prost::ProstCodec;

use super::super::log_store::{GroupLog, group_dir};
use super::super::tests::{MemSm, wal_config};
use super::super::types::{NodeId, RaftTypes, TenantBatchBytes, Vote};
use super::super::vote_log::{self, VoteLog};
use super::wire::{AppendRequest, AppendResponse};
use super::{APPEND, Peers, ReplicationServer, VERSION};

const GROUP: u32 = 7;
const WAIT: Option<Duration> = Some(Duration::from_secs(10));

fn bind() -> (TcpIncoming, SocketAddr) {
    let incoming = TcpIncoming::bind("127.0.0.1:0".parse().expect("addr")).expect("bind");
    let addr = incoming.local_addr().expect("local addr");
    (incoming, addr)
}

/// Serve `server` on `incoming` until the returned sender fires or drops.
fn serve(
    server: ReplicationServer<MemSm>,
    incoming: TcpIncoming,
) -> (oneshot::Sender<()>, JoinHandle<()>) {
    let (stop, stopped) = oneshot::channel::<()>();
    let serving = tokio::spawn(async move {
        let shutdown = async {
            let _ = stopped.await;
        };
        server.serve(incoming, shutdown).await.expect("serve");
    });
    (stop, serving)
}

struct Node {
    id: NodeId,
    raft: Raft<RaftTypes, MemSm>,
    sm: MemSm,
    stop: Option<oneshot::Sender<()>>,
    serving: JoinHandle<()>,
    _dir: TempDir,
}

impl Node {
    async fn start(n: u64, incoming: TcpIncoming, peers: &Peers) -> Self {
        let id = NodeId::from(n);
        let dir = tempfile::tempdir().expect("tempdir");
        vote_log::create(dir.path()).expect("vote log");
        let votes = VoteLog::open(dir.path()).expect("vote log");
        let wal = wal_config(&group_dir(dir.path(), GROUP));
        let log = GroupLog::open(wal, GROUP, votes).expect("group log");
        let sm = MemSm::default();
        let config = Arc::new(super::super::group_config().expect("config"));
        let raft = Raft::new(id, config, peers.network(id, GROUP), log, sm.clone())
            .await
            .expect("raft");
        let server = ReplicationServer::default();
        server.insert(GROUP, raft.clone());
        let (stop, serving) = serve(server, incoming);
        Self {
            id,
            raft,
            sm,
            stop: Some(stop),
            serving,
            _dir: dir,
        }
    }

    async fn kill(&mut self) {
        self.raft.shutdown().await.expect("shutdown");
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        tokio::time::timeout(Duration::from_secs(10), &mut self.serving)
            .await
            .expect("server stops")
            .expect("server task");
    }

    async fn applied_through(&self, index: u64) {
        self.raft
            .wait(WAIT)
            .applied_index_at_least(Some(index), "applied")
            .await
            .expect("applied");
        let applied = self.sm.state().applied.map(|id| id.index);
        assert!(applied >= Some(index), "{} applied {applied:?}", self.id);
    }
}

async fn leader(nodes: &[&Node], not: Option<NodeId>) -> NodeId {
    let metrics = nodes[0]
        .raft
        .wait(WAIT)
        .metrics(
            |m: &RaftMetrics<RaftTypes>| m.current_leader.is_some() && m.current_leader != not,
            "a leader",
        )
        .await
        .expect("leader elected");
    metrics.current_leader.expect("leader")
}

fn batch(n: u64) -> TenantBatchBytes {
    let payload = TenantBatch::encode("acme", format!("record {n}").as_bytes()).expect("batch");
    TenantBatchBytes::new(payload).expect("valid")
}

/// A new leader refuses writes until its blank entry commits.
async fn write(raft: &Raft<RaftTypes, MemSm>, n: u64) -> u64 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match raft.client_write(batch(n)).await {
            Ok(response) => return response.log_id.index,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("write {n}: {e}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_nodes_replicate_over_grpc_and_fail_over() {
    let peers = Peers::plaintext();
    let mut listeners = Vec::new();
    let mut members = BTreeMap::new();
    for n in 1..=3 {
        let (incoming, addr) = bind();
        members.insert(NodeId::from(n), addr.to_string());
        listeners.push((n, incoming));
    }
    let mut nodes = Vec::new();
    for (n, incoming) in listeners {
        nodes.push(Node::start(n, incoming, &peers).await);
    }
    nodes[0].raft.initialize(members).await.expect("initialize");

    let first = leader(&nodes.iter().collect::<Vec<_>>(), None).await;
    let at = nodes
        .iter()
        .position(|n| n.id == first)
        .expect("leader node");
    let mut last = 0;
    for n in 0..5 {
        last = write(&nodes[at].raft, n).await;
    }
    for node in &nodes {
        node.applied_through(last).await;
    }

    let mut old = nodes.remove(at);
    old.kill().await;
    let survivors: Vec<&Node> = nodes.iter().collect();
    let second = leader(&survivors, Some(first)).await;
    let new = survivors
        .iter()
        .find(|n| n.id == second)
        .expect("new leader is a survivor");
    let written = write(&new.raft, 99).await;
    assert!(written > last);
    for node in &survivors {
        node.applied_through(written).await;
    }
    for node in &mut nodes {
        node.kill().await;
    }
}

async fn raw_append(addr: SocketAddr, request: AppendRequest) -> Status {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready().await.expect("ready");
    grpc.unary::<_, AppendResponse, _>(
        Request::new(request),
        PathAndQuery::from_static(APPEND),
        ProstCodec::default(),
    )
    .await
    .expect_err("refused")
}

#[tokio::test]
async fn an_unknown_protocol_version_is_refused() {
    let (incoming, addr) = bind();
    let (_stop, _serving) = serve(ReplicationServer::default(), incoming);
    let request = AppendRequest {
        version: VERSION + 1,
        group: GROUP,
        ..AppendRequest::default()
    };
    assert_eq!(
        raw_append(addr, request).await.code(),
        Code::FailedPrecondition
    );
}

fn vote_request() -> VoteRequest<RaftTypes> {
    VoteRequest::new(Vote::new(1, NodeId::from(1)), None)
}

#[tokio::test]
async fn a_group_the_peer_does_not_host_is_unreachable() {
    let (incoming, addr) = bind();
    let (_stop, _serving) = serve(ReplicationServer::default(), incoming);
    let request = AppendRequest {
        version: VERSION,
        group: GROUP,
        ..AppendRequest::default()
    };
    assert_eq!(raw_append(addr, request).await.code(), Code::NotFound);

    let mut network = Peers::plaintext().network(NodeId::from(1), GROUP);
    let mut client = network.new_client(NodeId::from(2), &addr.to_string()).await;
    let option = RPCOption::new(Duration::from_secs(5));
    let result = client.vote(vote_request(), option).await;
    assert!(
        matches!(result, Err(RPCError::Unreachable(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_peer_with_no_listener_is_unreachable() {
    let (incoming, addr) = bind();
    drop(incoming);
    let mut network = Peers::plaintext().network(NodeId::from(1), GROUP);
    let mut client = network.new_client(NodeId::from(2), &addr.to_string()).await;
    let option = RPCOption::new(Duration::from_secs(5));
    let result = client.pre_vote(vote_request(), option).await;
    assert!(
        matches!(result, Err(RPCError::Unreachable(_))),
        "{result:?}"
    );
}

#[test]
fn groups_run_without_leader_restore_and_with_pre_vote() {
    let config = super::super::group_config().expect("config");
    assert_eq!(config.enable_leader_restore, Some(false));
    assert_eq!(config.enable_pre_vote, Some(true));
    assert_eq!(config.allow_log_reversion, Some(false));
    assert_eq!(config.snapshot_policy, openraft::SnapshotPolicy::Never);
}

fn attr(span: &opentelemetry_sdk::trace::SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.as_str().into_owned())
}

/// A `NOT_FOUND` fails the client span, which every non-`OK` code does,
/// and leaves the server span unset, as semconv's server list says.
#[tokio::test]
async fn a_peer_rpc_is_traced_as_grpc_client_and_server_spans() {
    use opentelemetry::trace::{SpanKind, Status as SpanStatus};
    use tracing_subscriber::prelude::*;

    let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(ourios_telemetry::test_tracer(&provider)));
    let _default = tracing::subscriber::set_default(subscriber);

    let (incoming, addr) = bind();
    let (_stop, _serving) = serve(ReplicationServer::default(), incoming);
    let mut network = Peers::plaintext().network(NodeId::from(1), GROUP);
    let mut client = network.new_client(NodeId::from(2), &addr.to_string()).await;
    let option = RPCOption::new(Duration::from_secs(5));
    assert!(client.vote(vote_request(), option).await.is_err());

    let spans = exporter.get_finished_spans().expect("spans");
    let of_kind = |kind: SpanKind| {
        spans
            .iter()
            .find(|s| s.span_kind == kind)
            .unwrap_or_else(|| panic!("a {kind:?} span in {spans:?}"))
    };
    let (client, server) = (of_kind(SpanKind::Client), of_kind(SpanKind::Server));
    for span in [client, server] {
        assert_eq!(span.name, "ourios.wal.v1.Replication/Vote");
        assert_eq!(attr(span, "rpc.system.name").as_deref(), Some("grpc"));
        assert_eq!(
            attr(span, "rpc.method").as_deref(),
            Some("ourios.wal.v1.Replication/Vote")
        );
        assert_eq!(
            attr(span, "rpc.response.status_code").as_deref(),
            Some("NOT_FOUND")
        );
    }
    assert_eq!(attr(client, "server.address").as_deref(), Some("127.0.0.1"));
    assert!(
        matches!(client.status, SpanStatus::Error { .. }),
        "{:?}",
        client.status
    );
    assert_eq!(attr(client, "error.type").as_deref(), Some("NOT_FOUND"));
    assert_eq!(server.status, SpanStatus::Unset);
    assert_eq!(attr(server, "error.type"), None);
}
