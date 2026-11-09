//! Three group members over real loopback gRPC, the server's refusals,
//! message sizes, and the peer-RPC spans.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use openraft::async_runtime::WatchReceiver as _;
use openraft::errors::RPCError;
use openraft::network::{RPCOption, RaftNetworkFactory, v2::RaftNetworkV2};
use openraft::raft::{AppendEntriesRequest, AppendEntriesResponse, StreamAppendError, VoteRequest};
use opentelemetry::global::BoxedTracer;
use opentelemetry::trace::{SpanKind, Status as SpanStatus};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
use ourios_wal::TenantBatch;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::transport::server::TcpIncoming;
use tonic::{Code, Request, Status};
use tonic_prost::ProstCodec;

use super::super::cluster::{Node, Replica, bind, leader, name, serve, write};
use super::super::state_machine::GroupSm;
use super::super::types::{Entry, NodeId, RaftTypes, TenantBatchBytes, Vote};
use super::trace::Tracing;
use super::wire::{
    AppendRequest, AppendResponse, Header, MAX_MESSAGE_BYTES, StreamAppendResponse, WireLogId,
    WireVote,
};
use super::{
    APPEND, ForwardRequest, ForwardTarget, Forwarded, PeerClient, Peers, ReplicationServer,
    STREAM_APPEND, VERSION,
};

mod mtls;
mod pipelining;
mod shutdown;
mod snapshot;
mod withholding;

const GROUP: u32 = 7;

/// A single node hosting [`GROUP`], from a fresh directory.
async fn start(
    n: u64,
    incoming: TcpIncoming,
    peers: &Peers,
    server: ReplicationServer<GroupSm>,
) -> Node {
    Node::open(Replica::new(n, &[GROUP]), incoming, peers, server).await
}

/// `size` listeners, the membership naming each node by its `node_id`,
/// and each `node_id`'s address.
fn listeners(size: u64) -> (Vec<(u64, TcpIncoming)>, Members, Addresses) {
    let mut listeners = Vec::new();
    let mut members = BTreeMap::new();
    let mut addresses = Vec::new();
    for n in 1..=size {
        let (incoming, addr) = bind();
        let id = NodeId::from(n);
        members.insert(id, name(id));
        addresses.push((name(id), addr.to_string()));
        listeners.push((n, incoming));
    }
    (listeners, members, addresses)
}

type Members = BTreeMap<NodeId, String>;
type Addresses = Vec<(String, String)>;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_nodes_replicate_over_grpc_and_fail_over() {
    let (listeners, members, addresses) = listeners(3);
    let peers = Peers::plaintext().with_addresses(addresses);
    let mut nodes = Vec::new();
    for (n, incoming) in listeners {
        nodes.push(start(n, incoming, &peers, ReplicationServer::default()).await);
    }
    nodes[0]
        .raft(GROUP)
        .initialize(members)
        .await
        .expect("initialize");

    let first = leader(GROUP, &nodes.iter().collect::<Vec<_>>(), None).await;
    let at = nodes
        .iter()
        .position(|n| n.id == first)
        .expect("leader node");
    let mut last = 0;
    for n in 0..5 {
        last = write(nodes[at].raft(GROUP), n).await;
    }
    for node in &nodes {
        node.applied_through(GROUP, last).await;
    }

    let mut old = nodes.remove(at);
    old.kill().await;
    let survivors: Vec<&Node> = nodes.iter().collect();
    let second = leader(GROUP, &survivors, Some(first)).await;
    let new = survivors
        .iter()
        .find(|n| n.id == second)
        .expect("new leader is a survivor");
    let written = write(new.raft(GROUP), 99).await;
    assert!(written > last);
    for node in &survivors {
        node.applied_through(GROUP, written).await;
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

/// One node serving on loopback, and a client to it.
async fn one_node() -> (Node, PeerClient) {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let node = start(1, incoming, &peers, ReplicationServer::default()).await;
    let client = client_to(&peers, node.id, addr).await;
    (node, client)
}

/// A client to `target`, reached at `addr`.
async fn client_to(peers: &Peers, target: NodeId, addr: SocketAddr) -> PeerClient {
    let peers = peers
        .clone()
        .with_addresses([(name(target), addr.to_string())]);
    let mut network = peers.network(NodeId::from(2), GROUP);
    network.new_client(target, &name(target)).await
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
    let mut node = start(1, incoming, &peers, ReplicationServer::default()).await;
    let vote_before = node.raft(GROUP).metrics().borrow_watched().vote;
    let higher = VoteRequest::new(Vote::new(5, NodeId::from(2)), None);

    let mut stranger = client_to(&peers, NodeId::from(9), addr).await;
    let refused = stranger.vote(higher.clone(), option()).await;
    assert!(
        matches!(refused, Err(RPCError::Unreachable(_))),
        "{refused:?}"
    );
    assert_eq!(
        node.raft(GROUP).metrics().borrow_watched().vote,
        vote_before
    );

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
        payload: openraft::EntryPayload::Normal(
            TenantBatchBytes::new(batch).expect("valid").into(),
        ),
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
#[ignore = "superseded: append_entries answers an AppendResult since openraft 817899a, \
            see an_entry_over_four_mib_is_matched_through_its_log_id"]
async fn an_entry_over_four_mib_replicates() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = start(1, incoming, &peers, ReplicationServer::default()).await;
    let mut client = client_to(&peers, node.id, addr).await;
    let response = client
        .append_entries(append(vec![entry(0, 5 * MIB)]), option())
        .await;
    assert_eq!(
        AppendEntriesResponse::from(response.expect("append")),
        AppendEntriesResponse::Success
    );
    node.kill().await;
}

/// tonic's default limit is 4 MiB; a WAL frame may be 16.
#[tokio::test]
async fn an_entry_over_four_mib_is_matched_through_its_log_id() {
    let (mut node, mut client) = one_node().await;
    let rpc = append(vec![entry(0, 5 * MIB)]);
    let last = rpc.last_log_id();
    let response = client.append_entries(rpc, option()).await;
    assert_eq!(response.expect("append"), Ok(last));
    node.kill().await;
}

#[tokio::test]
#[ignore = "superseded: append_entries answers an AppendResult since openraft 817899a, \
            see a_batch_over_the_byte_budget_is_matched_through_its_prefix"]
async fn a_batch_over_the_byte_budget_is_a_partial_success() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = start(1, incoming, &peers, ReplicationServer::default()).await;
    let mut client = client_to(&peers, node.id, addr).await;
    let rpc = append((0..3).map(|i| entry(i, 7 * MIB)).collect());
    let second = rpc.entries[1].log_id;
    let response = client.append_entries(rpc, option()).await;
    assert_eq!(
        AppendEntriesResponse::from(response.expect("append")),
        AppendEntriesResponse::PartialSuccess(Some(second))
    );
    node.kill().await;
}

/// The prefix that fits the byte budget is matched, not the whole batch.
#[tokio::test]
async fn a_batch_over_the_byte_budget_is_matched_through_its_prefix() {
    let (mut node, mut client) = one_node().await;
    let rpc = append((0..3).map(|i| entry(i, 7 * MIB)).collect());
    let second = rpc.entries[1].log_id;
    let response = client.append_entries(rpc, option()).await;
    assert_eq!(response.expect("append"), Ok(Some(second)));
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
    let mut node = start(1, incoming, &peers, server).await;
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
    let port = opentelemetry::Value::I64(i64::from(addr.port()));
    for span in spans.iter().filter(|s| s.span_kind == SpanKind::Client) {
        let found = span
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "server.port");
        assert_eq!(found.map(|kv| &kv.value), Some(&port), "{span:?}");
    }
}

#[tokio::test]
async fn a_failing_empty_append_records_a_server_error_span_too() {
    let (exporter, tracing, _default) = capture();
    let (incoming, addr) = bind();
    let peers = Peers::traced_by(tracing.clone());
    let mut node = start(1, incoming, &peers, ReplicationServer::traced_by(tracing)).await;
    node.raft(GROUP).shutdown().await.expect("shutdown");
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

/// The replies to `requests` sent as one `StreamAppend` stream past
/// `PeerClient`'s encoding, up to and including the status it ends on;
/// `Err` when the stream is refused before it opens.
async fn raw_stream(
    addr: SocketAddr,
    requests: Vec<AppendRequest>,
) -> Result<Vec<Result<StreamAppendResponse, Code>>, Code> {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready().await.expect("ready");
    let codec = ProstCodec::<AppendRequest, StreamAppendResponse>::default();
    let request = Request::new(futures_util::stream::iter(requests));
    let path = PathAndQuery::from_static(STREAM_APPEND);
    let mut replies = grpc
        .streaming(request, path, codec)
        .await
        .map_err(|s| s.code())?
        .into_inner();
    let mut seen = Vec::new();
    loop {
        match replies.message().await {
            Ok(Some(reply)) => seen.push(Ok(reply)),
            Ok(None) => return Ok(seen),
            Err(status) => {
                seen.push(Err(status.code()));
                return Ok(seen);
            }
        }
    }
}

fn wire_append(to: NodeId, rpc: &AppendEntriesRequest<RaftTypes>) -> AppendRequest {
    let to = super::wire::Recipient {
        group: GROUP,
        node: to,
    };
    AppendRequest::bounded(to, rpc).expect("encode").0
}

/// Appends of `runs` entries each, every one following the last.
fn consecutive(runs: &[u64]) -> Vec<AppendEntriesRequest<RaftTypes>> {
    let mut next = 0_u64;
    runs.iter()
        .map(|&run| {
            let prev = next.checked_sub(1).map(|i| log(1, i));
            let entries = (next..next + run).map(|i| entry(i, 16)).collect();
            next += run;
            AppendEntriesRequest {
                prev_log_id: prev,
                ..append(entries)
            }
        })
        .collect()
}

type StreamAnswer = Result<openraft::raft::StreamAppendResult<RaftTypes>, RPCError<RaftTypes>>;

/// The answers to `requests` streamed, without conflict hints: the wire
/// carries none.
async fn streamed(
    client: &mut PeerClient,
    requests: Vec<AppendEntriesRequest<RaftTypes>>,
    option: RPCOption,
) -> Result<Vec<StreamAnswer>, RPCError<RaftTypes>> {
    let input = futures_util::stream::iter(requests);
    let answers = client.stream_append(input, option).await?;
    let unhinted =
        answers.map(|answer| answer.map(|result| result.map_err(StreamAppendError::from)));
    Ok(unhinted.collect().await)
}

fn matched(answers: &[StreamAnswer]) -> Vec<Option<u64>> {
    answers
        .iter()
        .map(|a| match a {
            Ok(Ok(id)) => id.map(|id| id.index),
            other => panic!("not a match: {other:?}"),
        })
        .collect()
}

/// Replication runs over the pipelined stream, not unary `Append`: the
/// peer-RPC spans the group records are `StreamAppend`'s. On one thread,
/// so the `tracing` layer would see a unary span too.
#[tokio::test]
async fn three_nodes_replicate_through_stream_append() {
    let (exporter, tracing, _default) = capture();
    let (listeners, members, addresses) = listeners(3);
    let peers = Peers::traced_by(tracing.clone()).with_addresses(addresses);
    let mut nodes = Vec::new();
    for (n, incoming) in listeners {
        let server = ReplicationServer::traced_by(tracing.clone());
        nodes.push(start(n, incoming, &peers, server).await);
    }
    nodes[0]
        .raft(GROUP)
        .initialize(members)
        .await
        .expect("initialize");
    let first = leader(GROUP, &nodes.iter().collect::<Vec<_>>(), None).await;
    let at = nodes.iter().position(|n| n.id == first).expect("leader");
    let mut last = 0;
    for n in 0..5 {
        last = write(nodes[at].raft(GROUP), n).await;
    }
    for node in &nodes {
        node.applied_through(GROUP, last).await;
    }
    for node in &mut nodes {
        node.kill().await;
    }

    let spans = peer_spans(&exporter);
    let unary = spans
        .iter()
        .filter(|s| s.name == "ourios.wal.v1.Replication/Append");
    assert_eq!(unary.count(), 0, "{spans:?}");
    let streamed_ok = |kind: SpanKind| {
        spans.iter().any(|s| {
            s.span_kind == kind
                && s.name == "ourios.wal.v1.Replication/StreamAppend"
                && attr(s, "rpc.response.status_code").as_deref() == Some("OK")
        })
    };
    assert!(streamed_ok(SpanKind::Client), "{spans:?}");
    assert!(streamed_ok(SpanKind::Server), "{spans:?}");
}

/// Every message is answered in order with the log id it matched
/// through; the unary path still serves alongside.
#[tokio::test]
#[ignore = "superseded: append_entries answers an AppendResult since openraft 817899a, \
            see a_stream_answers_each_append_in_order_beside_the_unary_path"]
async fn a_stream_answers_each_append_in_order() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = start(1, incoming, &peers, ReplicationServer::default()).await;
    let mut client = client_to(&peers, node.id, addr).await;
    let answers = streamed(&mut client, consecutive(&[3, 2, 1, 0]), option())
        .await
        .expect("stream");
    assert_eq!(matched(&answers), [Some(2), Some(4), Some(5), Some(5)]);

    let next = consecutive(&[3, 2, 1, 1]).remove(3);
    let unary = client.append_entries(next, option()).await;
    assert_eq!(
        AppendEntriesResponse::from(unary.expect("unary")),
        AppendEntriesResponse::Success
    );
    node.kill().await;
}

/// Every message is answered in order with the log id it matched
/// through; the unary path still serves alongside.
#[tokio::test]
async fn a_stream_answers_each_append_in_order_beside_the_unary_path() {
    let (mut node, mut client) = one_node().await;
    let answers = streamed(&mut client, consecutive(&[3, 2, 1, 0]), option())
        .await
        .expect("stream");
    assert_eq!(matched(&answers), [Some(2), Some(4), Some(5), Some(5)]);

    let next = consecutive(&[3, 2, 1, 1]).remove(3);
    let last = next.last_log_id();
    let unary = client.append_entries(next, option()).await;
    assert_eq!(unary.expect("unary"), Ok(last));
    node.kill().await;
}

/// An append over the byte budget goes out as follow-on pieces, each
/// answered within the append's range, rather than as a partial success
/// that would end the stream.
#[tokio::test]
async fn a_streamed_append_over_the_byte_budget_is_split_and_fully_matched() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = start(1, incoming, &peers, ReplicationServer::default()).await;
    let mut client = client_to(&peers, node.id, addr).await;
    let rpc = append((0..3).map(|i| entry(i, 7 * MIB)).collect());
    let answers = streamed(&mut client, vec![rpc], option())
        .await
        .expect("stream");
    assert_eq!(matched(&answers), [Some(1), Some(2)]);
    node.kill().await;
}

#[tokio::test]
async fn a_conflict_ends_the_stream() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = start(1, incoming, &peers, ReplicationServer::default()).await;
    let mut client = client_to(&peers, node.id, addr).await;
    let ahead = AppendEntriesRequest {
        prev_log_id: Some(log(1, 5)),
        ..append(vec![entry(6, 16)])
    };
    let mut requests = vec![ahead];
    requests.extend(consecutive(&[1]));
    let answers = streamed(&mut client, requests, option())
        .await
        .expect("stream");
    assert!(
        matches!(
            answers.as_slice(),
            [Ok(Err(StreamAppendError::Conflict(id)))] if *id == log(1, 5)
        ),
        "{answers:?}"
    );
    node.kill().await;
}

#[tokio::test]
async fn a_higher_vote_ends_the_stream() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = start(1, incoming, &peers, ReplicationServer::default()).await;
    let mut client = client_to(&peers, node.id, addr).await;
    let higher = VoteRequest::new(Vote::new(5, NodeId::from(3)), None);
    let granted = client.vote(higher, option()).await.expect("vote");
    assert!(granted.vote_granted);
    let answers = streamed(&mut client, consecutive(&[1, 1]), option())
        .await
        .expect("stream");
    assert!(
        matches!(
            answers.as_slice(),
            [Ok(Err(StreamAppendError::HigherVote(v)))] if v.leader_id.term == 5
        ),
        "{answers:?}"
    );
    node.kill().await;
}

/// The first message routes the stream, so its refusals are the unary
/// ones, before the stream opens. A malformed message, or one addressed
/// elsewhere than the first, ends an open stream after the answers to the
/// messages before it, and the group keeps serving.
#[tokio::test]
async fn a_stream_is_refused_as_unary_append_is() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = start(1, incoming, &peers, ReplicationServer::default()).await;
    let valid = wire_append(node.id, &consecutive(&[1])[0]);
    let headed = |header: Header| AppendRequest {
        header: Some(header),
        ..valid.clone()
    };
    let unknown_group = Header {
        group: GROUP + 1,
        ..header(VERSION)
    };
    let stranger = wire_append(NodeId::from(9), &consecutive(&[1])[0]);
    let refused = [
        (headed(header(VERSION + 1)), Code::FailedPrecondition),
        (headed(unknown_group.clone()), Code::NotFound),
        (stranger, Code::NotFound),
    ];
    for (request, code) in refused {
        assert_eq!(raw_stream(addr, vec![request]).await, Err(code));
    }

    let uncommitted = AppendRequest {
        vote: Some(WireVote::from(&Vote::new(1, NodeId::from(2)))),
        ..valid.clone()
    };
    let gap = wire_append(node.id, &append(vec![entry(3, 16)]));
    let elsewhere = AppendRequest {
        header: Some(unknown_group),
        ..wire_append(node.id, &consecutive(&[1, 1])[1])
    };
    for bad in [uncommitted, gap, elsewhere] {
        let replies = raw_stream(addr, vec![valid.clone(), bad])
            .await
            .expect("open");
        assert_eq!(replies.len(), 2, "{replies:?}");
        assert!(replies[0].is_ok(), "{replies:?}");
        assert_eq!(replies[1], Err(Code::InvalidArgument));
    }

    let mut client = client_to(&peers, node.id, addr).await;
    let answers = streamed(&mut client, consecutive(&[1, 1]), option())
        .await
        .expect("still serving");
    assert_eq!(matched(&answers), [Some(0), Some(1)]);
    node.kill().await;
}

#[tokio::test]
async fn a_stream_to_a_group_the_peer_does_not_host_is_unreachable() {
    let (incoming, addr) = bind();
    let (_stop, _serving) = serve(ReplicationServer::default(), incoming);
    let mut client = client_to(&Peers::plaintext(), NodeId::from(1), addr).await;
    let result = streamed(&mut client, consecutive(&[1]), option()).await;
    assert!(
        matches!(result, Err(RPCError::Unreachable(_))),
        "{result:?}"
    );
}

/// A stopped `Raft` leaves admitted appends unanswered: the stream ends
/// `UNAVAILABLE`, an unreachable peer, not a clean end.
#[tokio::test]
async fn a_stream_to_a_stopped_group_is_unreachable() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = start(1, incoming, &peers, ReplicationServer::default()).await;
    node.raft(GROUP).shutdown().await.expect("shutdown");
    let mut client = client_to(&peers, node.id, addr).await;
    let answers = streamed(&mut client, consecutive(&[1]), option())
        .await
        .expect("opens");
    assert!(
        matches!(answers.as_slice(), [Err(RPCError::Unreachable(_))]),
        "{answers:?}"
    );
    node.kill().await;
}

/// A leader holds its stream open while it has nothing to send; the
/// follower stopping still ends it, and its server can shut down.
#[tokio::test]
async fn an_idle_stream_ends_when_the_group_stops() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = start(1, incoming, &peers, ReplicationServer::default()).await;
    let mut client = client_to(&peers, node.id, addr).await;
    let input =
        futures_util::stream::iter(consecutive(&[1])).chain(futures_util::stream::pending());
    let mut answers = client.stream_append(input, option()).await.expect("open");
    let first = answers.next().await.expect("an answer");
    assert!(matches!(first, Ok(Ok(Some(_)))), "{first:?}");
    let idle = tokio::time::timeout(Duration::from_millis(200), answers.next()).await;
    assert!(idle.is_err(), "an idle stream is not a timeout: {idle:?}");

    node.raft(GROUP).shutdown().await.expect("shutdown");
    let ended = answers.next().await.expect("an end");
    assert!(matches!(ended, Err(RPCError::Unreachable(_))), "{ended:?}");
    drop(answers);
    node.kill().await;
}

#[tokio::test]
async fn a_stream_to_a_silent_peer_times_out() {
    let (_listener, addr) = silent_peer();
    let mut client = client_to(&Peers::plaintext(), NodeId::from(1), addr).await;
    let option = RPCOption::new(Duration::from_millis(200));
    let result = streamed(&mut client, consecutive(&[1]), option).await;
    assert!(matches!(result, Err(RPCError::Timeout(_))), "{result:?}");
}

/// A heartbeat is a stream of one entry-less append: a successful one
/// leaves no span, and a stream that carries entries leaves one span on
/// each side.
#[tokio::test]
async fn a_stream_is_traced_once_and_only_when_it_carries_entries() {
    let (exporter, tracing, _default) = capture();
    let (incoming, addr) = bind();
    let peers = Peers::traced_by(tracing.clone());
    let server = ReplicationServer::traced_by(tracing);
    let mut node = start(1, incoming, &peers, server).await;
    let mut client = client_to(&peers, node.id, addr).await;
    let heartbeat = streamed(&mut client, consecutive(&[0]), option()).await;
    assert_eq!(matched(&heartbeat.expect("heartbeat")), [None]);

    let carrying = streamed(&mut client, consecutive(&[2, 1]), option()).await;
    assert_eq!(matched(&carrying.expect("stream")), [Some(1), Some(2)]);
    node.kill().await;

    let spans = peer_spans(&exporter);
    assert_eq!(spans.len(), 2, "{spans:?}");
    for kind in [SpanKind::Client, SpanKind::Server] {
        let span = of_kind(&spans, &kind);
        assert_eq!(rpc_of(&span), expected_rpc("StreamAppend", "OK"));
        assert!(!failure_of(&span).0, "{span:?}");
    }
}

/// A follower that went away comes back to a leader backing off from
/// it: `openraft` then holds a session's first append for the backoff,
/// longer than the soft TTL, and the stream must still open and catch the
/// follower up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_follower_that_rejoins_catches_up_over_the_stream() {
    let (listeners, members, addresses) = listeners(3);
    let peers = Peers::plaintext().with_addresses(addresses);
    let mut addrs = BTreeMap::new();
    let mut nodes = Vec::new();
    for (n, incoming) in listeners {
        addrs.insert(NodeId::from(n), incoming.local_addr().expect("local addr"));
        nodes.push(start(n, incoming, &peers, ReplicationServer::default()).await);
    }
    nodes[0]
        .raft(GROUP)
        .initialize(members)
        .await
        .expect("initialize");
    let first = leader(GROUP, &nodes.iter().collect::<Vec<_>>(), None).await;
    let at = nodes.iter().position(|n| n.id == first).expect("leader");
    let gone = (at + 1) % nodes.len();
    write(nodes[at].raft(GROUP), 0).await;

    let mut follower = nodes.remove(gone);
    follower.kill().await;
    let at = nodes.iter().position(|n| n.id == first).expect("leader");
    let mut last = 0;
    for n in 1..20 {
        last = write(nodes[at].raft(GROUP), n).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let incoming = TcpIncoming::bind(addrs[&follower.id]).expect("rebind");
    let replica = follower.into_replica();
    let back = Node::open(replica, incoming, &peers, ReplicationServer::default()).await;
    back.applied_through(GROUP, last).await;
    nodes.push(back);
    for node in &mut nodes {
        node.kill().await;
    }
}

/// An answered stream left idle past the soft TTL is not timed out, and
/// carries the next append when `openraft` yields one.
#[tokio::test]
async fn a_stream_idle_past_the_soft_ttl_carries_the_next_append() {
    let (incoming, addr) = bind();
    let peers = Peers::plaintext();
    let mut node = start(1, incoming, &peers, ReplicationServer::default()).await;
    let mut client = client_to(&peers, node.id, addr).await;
    let option = RPCOption::new(Duration::from_millis(200));
    let soft_ttl = option.soft_ttl();
    let (send, input) = futures::channel::mpsc::unbounded();
    let mut appends = consecutive(&[1, 1]).into_iter();
    send.unbounded_send(appends.next().expect("first"))
        .expect("send");
    let mut answers = client.stream_append(input, option).await.expect("open");
    let first = answers.next().await.expect("an answer");
    assert!(
        matches!(first, Ok(Ok(Some(id))) if id.index == 0),
        "{first:?}"
    );

    let idle = tokio::time::timeout(soft_ttl * 4, answers.next()).await;
    assert!(idle.is_err(), "an idle stream is not a timeout: {idle:?}");

    send.unbounded_send(appends.next().expect("second"))
        .expect("send");
    let second = answers.next().await.expect("an answer");
    assert!(
        matches!(second, Ok(Ok(Some(id))) if id.index == 1),
        "{second:?}"
    );
    drop(answers);
    node.kill().await;
}

/// A node whose forwarded exports never commit.
struct Hanging;

impl ForwardTarget for Hanging {
    fn commit(
        &self,
        _: Forwarded,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<u64, super::Refusal>> + Send>> {
        Box::pin(std::future::pending())
    }
}

/// A forward's client span records the call's own status, not the
/// outcome the sender reads it as: a refused version is
/// `FAILED_PRECONDITION`, not "not hosted", and a call that outlives its
/// deadline is `DEADLINE_EXCEEDED`.
#[tokio::test]
async fn a_forward_span_records_the_calls_own_status() {
    let (exporter, tracing, _guard) = capture();
    let server = ReplicationServer::<GroupSm>::traced_by(tracing.clone());
    server.serve_forwards(Arc::new(Hanging));
    let (incoming, addr) = bind();
    let (_stop, _serving) = serve(server, incoming);
    let peers = Peers::traced_by(tracing).with_addresses([("n1".to_owned(), addr.to_string())]);
    let request = |version| ForwardRequest {
        version,
        group: GROUP,
        payload: TenantBatch::encode("acme", b"record").expect("batch"),
        credential: None,
    };
    let commit = Duration::from_millis(100);
    peers.forward("n1", request(VERSION + 1), commit).await;
    peers.forward("n1", request(VERSION), commit).await;

    let codes: Vec<_> = peer_spans(&exporter)
        .iter()
        .filter(|span| span.span_kind == SpanKind::Client)
        .map(|span| attr(span, "rpc.response.status_code"))
        .collect();
    let expected = ["FAILED_PRECONDITION", "DEADLINE_EXCEEDED"].map(|c| Some(c.to_owned()));
    assert_eq!(codes, expected);
}
