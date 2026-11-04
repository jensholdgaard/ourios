//! Three group members over real loopback gRPC, the server's refusals,
//! message sizes, and the peer-RPC spans.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use openraft::async_runtime::WatchReceiver as _;
use openraft::errors::RPCError;
use openraft::network::{RPCOption, RaftNetworkFactory, v2::RaftNetworkV2};
use openraft::raft::{AppendEntriesRequest, AppendEntriesResponse, VoteRequest};
use openraft::{Raft, RaftMetrics};
use opentelemetry::global::BoxedTracer;
use opentelemetry::trace::{SpanKind, Status as SpanStatus};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
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
use super::super::types::{Entry, NodeId, RaftTypes, TenantBatchBytes, Vote};
use super::super::vote_log::{self, VoteLog};
use super::trace::Tracing;
use super::wire::{
    AppendRequest, AppendResponse, Header, MAX_MESSAGE_BYTES, SnapshotRequest, WireLogId,
    WireSnapshotResponse, WireVote,
};
use super::{APPEND, INSTALL_SNAPSHOT, PeerClient, Peers, ReplicationServer, VERSION};

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
    async fn start(
        n: u64,
        incoming: TcpIncoming,
        peers: &Peers,
        server: ReplicationServer<MemSm>,
    ) -> Self {
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
        nodes.push(Node::start(n, incoming, &peers, ReplicationServer::default()).await);
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

/// A request sent as is, past `PeerClient`'s own encoding.
async fn raw<Req, Resp>(addr: SocketAddr, path: &'static str, request: Req) -> Result<Resp, Status>
where
    Req: prost::Message + Send + 'static,
    Resp: prost::Message + Default + Send + 'static,
{
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let mut grpc = tonic::client::Grpc::new(channel).max_encoding_message_size(MAX_MESSAGE_BYTES);
    grpc.ready().await.expect("ready");
    let codec = ProstCodec::<Req, Resp>::default();
    let response = grpc
        .unary(
            Request::new(request),
            PathAndQuery::from_static(path),
            codec,
        )
        .await?;
    Ok(response.into_inner())
}

async fn raw_append(addr: SocketAddr, request: AppendRequest) -> Status {
    raw::<_, AppendResponse>(addr, APPEND, request)
        .await
        .expect_err("refused")
}

fn header(version: u32) -> Header {
    Header {
        version,
        group: GROUP,
        target: NodeId::from(1).uuid().as_bytes().to_vec(),
    }
}

#[tokio::test]
async fn an_unknown_protocol_version_is_refused() {
    let (incoming, addr) = bind();
    let (_stop, _serving) = serve(ReplicationServer::default(), incoming);
    let request = AppendRequest {
        header: Some(header(VERSION + 1)),
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

fn option() -> RPCOption {
    RPCOption::new(Duration::from_secs(5))
}

async fn client_to(peers: &Peers, target: NodeId, addr: SocketAddr) -> PeerClient {
    let mut network = peers.network(NodeId::from(2), GROUP);
    network.new_client(target, &addr.to_string()).await
}

#[tokio::test]
async fn a_group_the_peer_does_not_host_is_unreachable() {
    let (incoming, addr) = bind();
    let (_stop, _serving) = serve(ReplicationServer::default(), incoming);
    let request = AppendRequest {
        header: Some(header(VERSION)),
        ..AppendRequest::default()
    };
    assert_eq!(raw_append(addr, request).await.code(), Code::NotFound);

    let mut client = client_to(&Peers::plaintext(), NodeId::from(1), addr).await;
    let result = client.vote(vote_request(), option()).await;
    assert!(
        matches!(result, Err(RPCError::Unreachable(_))),
        "{result:?}"
    );
}

/// A replica that replaced another at its address must not answer for
/// it: the reply would be credited to the old voter.
#[tokio::test]
async fn a_request_for_another_replica_at_the_address_is_refused_uncredited() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = Node::start(1, incoming, &peers, ReplicationServer::default()).await;
    let vote_before = node.raft.metrics().borrow_watched().vote;
    let higher = VoteRequest::new(Vote::new(5, NodeId::from(2)), None);

    let mut stranger = client_to(&peers, NodeId::from(9), addr).await;
    let refused = stranger.vote(higher.clone(), option()).await;
    assert!(
        matches!(refused, Err(RPCError::Unreachable(_))),
        "{refused:?}"
    );
    assert_eq!(node.raft.metrics().borrow_watched().vote, vote_before);

    let mut addressed = client_to(&peers, node.id, addr).await;
    let granted = addressed.vote(higher, option()).await.expect("vote");
    assert!(granted.vote_granted);
    node.kill().await;
}

#[tokio::test]
async fn a_peer_with_no_listener_is_unreachable() {
    let (incoming, addr) = bind();
    drop(incoming);
    let mut client = client_to(&Peers::plaintext(), NodeId::from(1), addr).await;
    let result = client.pre_vote(vote_request(), option()).await;
    assert!(
        matches!(result, Err(RPCError::Unreachable(_))),
        "{result:?}"
    );
}

#[test]
fn groups_run_without_leader_restore_and_with_pre_vote() {
    let c = super::super::group_config().expect("config");
    let flags = (
        c.enable_leader_restore,
        c.enable_pre_vote,
        c.allow_log_reversion,
    );
    assert_eq!(flags, (Some(false), Some(true), Some(false)));
    assert_eq!(c.snapshot_policy, openraft::SnapshotPolicy::Never);
}

fn entry(index: u64, bytes: usize) -> Entry {
    let batch = TenantBatch::encode("acme", &vec![7; bytes]).expect("batch");
    Entry {
        log_id: WireLogId { term: 1, index }.into(),
        payload: openraft::EntryPayload::Normal(TenantBatchBytes::new(batch).expect("valid")),
    }
}

fn append(entries: Vec<Entry>) -> AppendEntriesRequest<RaftTypes> {
    AppendEntriesRequest {
        vote: Vote::new_committed(1, NodeId::from(2)),
        prev_log_id: None,
        entries,
        leader_commit: None,
    }
}

const MIB: usize = 1024 * 1024;

/// tonic's default limit is 4 MiB; a WAL frame may be 16.
#[tokio::test]
async fn an_entry_over_four_mib_replicates() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = Node::start(1, incoming, &peers, ReplicationServer::default()).await;
    let mut client = client_to(&peers, node.id, addr).await;
    let response = client
        .append_entries(append(vec![entry(0, 5 * MIB)]), option())
        .await;
    assert_eq!(response.expect("append"), AppendEntriesResponse::Success);
    node.kill().await;
}

#[tokio::test]
async fn a_batch_over_the_byte_budget_is_a_partial_success() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = Node::start(1, incoming, &peers, ReplicationServer::default()).await;
    let mut client = client_to(&peers, node.id, addr).await;
    let rpc = append((0..3).map(|i| entry(i, 7 * MIB)).collect());
    let second = rpc.entries[1].log_id;
    let response = client.append_entries(rpc, option()).await;
    assert_eq!(
        response.expect("append"),
        AppendEntriesResponse::PartialSuccess(Some(second))
    );
    node.kill().await;
}

fn attr(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.as_str().into_owned())
}

/// Spans for this thread until the guard drops, from the `tracing` layer
/// and from the returned [`Tracing`] alike. The tests run on a
/// current-thread runtime, so every task is covered.
fn capture() -> (
    InMemorySpanExporter,
    Tracing,
    tracing::subscriber::DefaultGuard,
) {
    use opentelemetry::trace::TracerProvider as _;
    use tracing_subscriber::prelude::*;

    let exporter = InMemorySpanExporter::default();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let owned = BoxedTracer::new(Box::new(
        provider.tracer_with_scope(ourios_core::telemetry::scope("peer-rpc-test")),
    ));
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(ourios_telemetry::test_tracer(&provider)));
    let guard = tracing::subscriber::set_default(subscriber);
    (exporter, Tracing::Owned(Arc::new(owned)), guard)
}

/// The peer-RPC spans exported, not `openraft`'s own.
fn peer_spans(exporter: &InMemorySpanExporter) -> Vec<SpanData> {
    let spans = exporter.get_finished_spans().expect("spans");
    spans
        .into_iter()
        .filter(|s| s.name.starts_with("ourios.wal.v1.Replication/"))
        .collect()
}

/// A span's name, `rpc.system.name`, `rpc.method` and
/// `rpc.response.status_code`.
fn rpc_of(span: &SpanData) -> (String, [Option<String>; 3]) {
    let keys = ["rpc.system.name", "rpc.method", "rpc.response.status_code"];
    (span.name.to_string(), keys.map(|key| attr(span, key)))
}

/// What a failed span records: whether its status is an error, its
/// `error.type`, and `server.address`.
fn failure_of(span: &SpanData) -> (bool, Option<String>, Option<String>) {
    let error = matches!(span.status, SpanStatus::Error { .. });
    (
        error,
        attr(span, "error.type"),
        attr(span, "server.address"),
    )
}

fn expected_rpc(method: &str, code: &str) -> (String, [Option<String>; 3]) {
    let name = format!("ourios.wal.v1.Replication/{method}");
    let attrs = [
        Some("grpc".to_owned()),
        Some(name.clone()),
        Some(code.to_owned()),
    ];
    (name, attrs)
}

fn of_kind(spans: &[SpanData], kind: &SpanKind) -> SpanData {
    spans
        .iter()
        .find(|s| &s.span_kind == kind)
        .cloned()
        .unwrap_or_else(|| panic!("a {kind:?} span in {spans:?}"))
}

/// A `NOT_FOUND` fails the client span, which every non-`OK` code does,
/// and leaves the server span unset, as semconv's server list says.
#[tokio::test]
async fn a_peer_rpc_is_traced_as_grpc_client_and_server_spans() {
    let (exporter, _tracing, _default) = capture();
    let (incoming, addr) = bind();
    let (_stop, _serving) = serve(ReplicationServer::default(), incoming);
    let mut client = client_to(&Peers::plaintext(), NodeId::from(1), addr).await;
    assert!(client.vote(vote_request(), option()).await.is_err());

    let spans = peer_spans(&exporter);
    let client = of_kind(&spans, &SpanKind::Client);
    let server = of_kind(&spans, &SpanKind::Server);
    for span in [&client, &server] {
        assert_eq!(rpc_of(span), expected_rpc("Vote", "NOT_FOUND"));
    }
    let not_found = Some("NOT_FOUND".to_owned());
    let local = Some("127.0.0.1".to_owned());
    assert_eq!(failure_of(&client), (true, not_found, local));
    assert_eq!(failure_of(&server), (false, None, None));
}

/// A heartbeat is an append without entries: a successful one leaves no
/// span on either side.
#[tokio::test]
async fn a_successful_empty_append_records_no_span() {
    let (exporter, tracing, _default) = capture();
    let (incoming, addr) = bind();
    let peers = Peers::traced_by(tracing.clone());
    let server = ReplicationServer::traced_by(tracing);
    let mut node = Node::start(1, incoming, &peers, server).await;
    let mut client = client_to(&peers, node.id, addr).await;
    let response = client.append_entries(append(Vec::new()), option()).await;
    assert!(response.is_ok(), "{response:?}");
    node.kill().await;

    let spans = peer_spans(&exporter);
    assert!(spans.is_empty(), "{spans:?}");
}

/// A span built after the call failed, still covering the call.
fn assert_failed(span: &SpanData, kind: &SpanKind, code: &str) {
    assert_eq!(&span.span_kind, kind);
    assert_eq!(rpc_of(span), expected_rpc("Append", code));
    let (error, error_type, _) = failure_of(span);
    assert_eq!((error, error_type.as_deref()), (true, Some(code)));
    assert!(span.start_time < span.end_time, "{span:?}");
}

/// A failing heartbeat still shows, as an error span from the call's
/// start; an append with entries keeps its spans.
#[tokio::test]
async fn a_failing_empty_append_records_an_error_span_and_one_with_entries_is_traced() {
    let (exporter, tracing, _default) = capture();
    let (incoming, addr) = bind();
    let (_stop, _serving) = serve(ReplicationServer::traced_by(tracing.clone()), incoming);
    let mut client = client_to(&Peers::traced_by(tracing), NodeId::from(1), addr).await;
    let started = std::time::SystemTime::now();
    let empty = client.append_entries(append(Vec::new()), option()).await;
    assert!(matches!(empty, Err(RPCError::Unreachable(_))), "{empty:?}");

    let spans = peer_spans(&exporter);
    assert_eq!(
        spans.len(),
        1,
        "only the client fails on NOT_FOUND: {spans:?}"
    );
    assert_failed(&spans[0], &SpanKind::Client, "NOT_FOUND");
    assert!(spans[0].start_time >= started);
    assert_eq!(
        attr(&spans[0], "server.address").as_deref(),
        Some("127.0.0.1")
    );

    let carrying = client
        .append_entries(append(vec![entry(0, 16)]), option())
        .await;
    assert!(carrying.is_err());
    let spans = peer_spans(&exporter);
    assert_eq!(spans.len(), 3, "and a client and a server span: {spans:?}");
}

#[tokio::test]
async fn a_failing_empty_append_records_a_server_error_span_too() {
    let (exporter, tracing, _default) = capture();
    let (incoming, addr) = bind();
    let peers = Peers::traced_by(tracing.clone());
    let mut node = Node::start(1, incoming, &peers, ReplicationServer::traced_by(tracing)).await;
    node.raft.shutdown().await.expect("shutdown");
    let mut client = client_to(&peers, node.id, addr).await;
    let empty = client.append_entries(append(Vec::new()), option()).await;
    assert!(matches!(empty, Err(RPCError::Unreachable(_))), "{empty:?}");
    node.kill().await;

    let spans = peer_spans(&exporter);
    assert_eq!(spans.len(), 2, "{spans:?}");
    assert_failed(
        &of_kind(&spans, &SpanKind::Client),
        &SpanKind::Client,
        "UNAVAILABLE",
    );
    assert_failed(
        &of_kind(&spans, &SpanKind::Server),
        &SpanKind::Server,
        "UNAVAILABLE",
    );
}

fn log(term: u64, index: u64) -> openraft::type_config::alias::LogIdOf<RaftTypes> {
    WireLogId { term, index }.into()
}

/// A peer that accepts the connection and never answers.
fn silent_peer() -> (std::net::TcpListener, SocketAddr) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    (listener, addr)
}

fn snapshot_of(
    last: openraft::type_config::alias::LogIdOf<RaftTypes>,
) -> openraft::type_config::alias::SnapshotOf<RaftTypes, ()> {
    let voters = std::collections::BTreeSet::from([NodeId::from(1), NodeId::from(2)]);
    let nodes = BTreeMap::from([
        (NodeId::from(1), "127.0.0.1:1".to_owned()),
        (NodeId::from(2), "127.0.0.1:2".to_owned()),
    ]);
    let membership = super::super::types::Membership::new(vec![voters], nodes).expect("membership");
    openraft::type_config::alias::SnapshotOf::<RaftTypes, ()> {
        meta: openraft::type_config::alias::SnapshotMetaOf::<RaftTypes> {
            last_log_id: Some(last),
            last_membership: openraft::StoredMembership::new(Some(last), membership),
        },
        snapshot: (),
    }
}

#[tokio::test]
async fn a_full_snapshot_installs_over_grpc() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = Node::start(1, incoming, &peers, ReplicationServer::default()).await;
    let mut client = client_to(&peers, node.id, addr).await;
    let vote = Vote::new_committed(2, NodeId::from(2));
    let snapshot = snapshot_of(log(2, 5));
    let meta = snapshot.meta.clone();
    let never = std::future::pending::<openraft::errors::ReplicationClosed>();
    let response = client.full_snapshot(vote, snapshot, never, option()).await;
    assert_eq!(response.expect("installed").vote, vote);
    node.applied_through(5).await;
    let installed = {
        let state = node.sm.state();
        (state.applied, state.membership.clone())
    };
    assert_eq!(installed, (meta.last_log_id, meta.last_membership));
    node.kill().await;
}

#[tokio::test]
async fn a_pending_snapshot_install_ends_when_replication_closes() {
    let (_listener, addr) = silent_peer();
    let mut client = client_to(&Peers::plaintext(), NodeId::from(1), addr).await;
    let closed = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        openraft::errors::ReplicationClosed::new("replication stopped")
    };
    let vote = Vote::new_committed(2, NodeId::from(2));
    let response = client
        .full_snapshot(vote, snapshot_of(log(2, 5)), closed, option())
        .await;
    assert!(
        matches!(response, Err(openraft::errors::StreamingError::Closed(_))),
        "{response:?}"
    );
}

/// Requests a leader never sends, which `openraft` would assert on or
/// the group log fail on, are refused before dispatch and leave the group
/// serving.
#[tokio::test]
async fn requests_no_leader_sends_are_refused_and_the_group_keeps_serving() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = Node::start(1, incoming, &peers, ReplicationServer::default()).await;
    let vote = Vote::new_committed(1, NodeId::from(2));
    let to = super::wire::Recipient {
        group: GROUP,
        node: node.id,
    };
    let snapshot = SnapshotRequest::new(to, &vote, &snapshot_of(log(9, 5)).meta).expect("encode");
    let refused = raw::<_, WireSnapshotResponse>(addr, INSTALL_SNAPSHOT, snapshot).await;
    assert_eq!(
        refused.map(drop).map_err(|s| s.code()),
        Err(Code::InvalidArgument)
    );

    let oversized = AppendRequest {
        header: Some(header(VERSION)),
        vote: Some(WireVote::from(&vote)),
        entries: vec![vec![0; ourios_wal::MAX_FRAME_BYTES + 1]],
        ..AppendRequest::default()
    };
    assert_eq!(
        raw_append(addr, oversized).await.code(),
        Code::InvalidArgument
    );

    let mut client = client_to(&peers, node.id, addr).await;
    let gap = append(vec![entry(3, 16)]);
    assert!(client.append_entries(gap, option()).await.is_err());

    let higher = VoteRequest::new(Vote::new(5, NodeId::from(2)), None);
    let granted = client.vote(higher, option()).await.expect("still serving");
    assert!(granted.vote_granted);
    node.kill().await;
}
