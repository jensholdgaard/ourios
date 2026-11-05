//! The peer client: `openraft`'s network for one group, over a channel per
//! peer address that every group on the node shares.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::{Stream, StreamExt as _};
use openraft::OptionalSend;
use openraft::base::{BoxFuture, BoxStream};
use openraft::errors::{
    NetworkError, RPCError, ReplicationClosed, StreamingError, Timeout, Unreachable,
};
use openraft::network::{RPCOption, RPCTypes, RaftNetworkFactory, v2::RaftNetworkV2};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, SnapshotResponse, StreamAppendResult, VoteRequest,
    VoteResponse,
};
use openraft::type_config::alias::SnapshotOf;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Request, Status, Streaming};
use tonic_prost::ProstCodec;
use tracing::Instrument as _;

use super::super::types::{NodeId, RaftTypes, Vote};
use super::trace::{Call, Carried, Peer, StreamCall, Traced, Tracing};
use super::wire::{
    AppendRequest, AppendResponse, MAX_MESSAGE_BYTES, Malformed, Pieces, Recipient, Reply,
    SnapshotRequest, StreamAppendResponse, WireSnapshotResponse, WireVoteRequest, WireVoteResponse,
};
use super::{APPEND, INSTALL_SNAPSHOT, PRE_VOTE, STREAM_APPEND, VOTE};

type RpcError = RPCError<RaftTypes>;

/// One lazily connected channel per peer address. Clones share the pool.
#[derive(Clone, Default)]
pub struct Peers {
    channels: Arc<Mutex<HashMap<String, Channel>>>,
    tracing: Tracing,
}

impl Peers {
    /// The pool for plaintext peers.
    #[must_use]
    pub fn plaintext() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(super) fn traced_by(tracing: Tracing) -> Self {
        Self {
            channels: Arc::default(),
            tracing,
        }
    }

    /// The network `openraft` uses for `group` on the node `id`.
    #[must_use]
    pub fn network(&self, id: NodeId, group: u32) -> GroupNetwork {
        GroupNetwork {
            id,
            group,
            peers: self.clone(),
        }
    }

    /// The RFC 0030 client TLS config will be applied here.
    fn endpoint(addr: &str) -> Result<Endpoint, tonic::transport::Error> {
        Endpoint::from_shared(format!("http://{addr}"))
    }

    fn channel(&self, addr: &str) -> Result<Channel, tonic::transport::Error> {
        let mut channels = self.channels.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(channel) = channels.get(addr) {
            return Ok(channel.clone());
        }
        let channel = Self::endpoint(addr)?.connect_lazy();
        channels.insert(addr.to_owned(), channel.clone());
        Ok(channel)
    }
}

/// `openraft`'s network factory for one group.
pub struct GroupNetwork {
    id: NodeId,
    group: u32,
    peers: Peers,
}

impl RaftNetworkFactory<RaftTypes> for GroupNetwork {
    type Network = PeerClient;

    // The trait is async; the channel connects lazily, on first use.
    #[allow(clippy::unused_async_trait_impl)]
    async fn new_client(&mut self, target: NodeId, node: &String) -> PeerClient {
        let channel = self.peers.channel(node).map_err(|e| Unreachable::new(&e));
        PeerClient {
            id: self.id,
            to: Recipient {
                group: self.group,
                node: target,
            },
            addr: node.clone(),
            channel,
            tracing: self.peers.tracing.clone(),
        }
    }
}

/// One group's connection to one peer.
pub struct PeerClient {
    id: NodeId,
    to: Recipient,
    addr: String,
    /// An address that does not parse is a peer that cannot be reached.
    channel: Result<Channel, Unreachable<RaftTypes>>,
    tracing: Tracing,
}

fn network(e: &Malformed) -> RpcError {
    RPCError::Network(NetworkError::new(e))
}

/// One method of the service and the RPC kind `openraft` names it by.
#[derive(Clone, Copy)]
struct Rpc {
    path: &'static str,
    action: RPCTypes,
}

const APPEND_RPC: Rpc = Rpc {
    path: APPEND,
    action: RPCTypes::AppendEntries,
};
const VOTE_RPC: Rpc = Rpc {
    path: VOTE,
    action: RPCTypes::Vote,
};
const PRE_VOTE_RPC: Rpc = Rpc {
    path: PRE_VOTE,
    action: RPCTypes::Vote,
};
const INSTALL_SNAPSHOT_RPC: Rpc = Rpc {
    path: INSTALL_SNAPSHOT,
    action: RPCTypes::InstallSnapshot,
};

impl PeerClient {
    /// Encode, call, and decode the reply as `openraft`'s type.
    async fn exchange<Req, Resp>(
        &self,
        rpc: Rpc,
        request: Result<Req, Malformed>,
        option: &RPCOption,
    ) -> Result<Resp::Raft, RpcError>
    where
        Req: prost::Message + Traced + Send + 'static,
        Resp: Reply + prost::Message + Default + Send + 'static,
    {
        let request = request.map_err(|e| network(&e))?;
        let response: Resp = self.call(rpc, request, option).await?;
        response.into_raft().map_err(|e| network(&e))
    }

    async fn call<Req, Resp>(
        &self,
        rpc: Rpc,
        message: Req,
        option: &RPCOption,
    ) -> Result<Resp, RpcError>
    where
        Req: prost::Message + Traced + Send + 'static,
        Resp: prost::Message + Default + Send + 'static,
    {
        let channel = self.channel.clone()?;
        let ttl = option.soft_ttl();
        let peer = Peer::parse(&self.addr);
        let trace = Call::client(self.tracing.clone(), rpc.path, peer, message.traced());
        let call = async move {
            let mut grpc = tonic::client::Grpc::new(channel)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES);
            grpc.ready()
                .await
                .map_err(|e| Status::unavailable(e.to_string()))?;
            let mut request = Request::new(message);
            request.set_timeout(ttl);
            let codec = ProstCodec::<Req, Resp>::default();
            grpc.unary(request, PathAndQuery::from_static(rpc.path), codec)
                .await
        };
        let result = tokio::time::timeout(ttl, call)
            .instrument(trace.span())
            .await
            .unwrap_or_else(|_| Err(Status::deadline_exceeded("peer RPC timed out")));
        trace.finish(result.as_ref().map_or_else(Status::code, |_| Code::Ok));
        result
            .map(tonic::Response::into_inner)
            .map_err(|status| self.rpc_error(&status, rpc.action, ttl))
    }

    /// Transport failures, a version the peer refuses, a group or replica
    /// it does not host, and a message over a size limit (a peer whose
    /// limit differs, until it is upgraded) are all an unreachable peer,
    /// which `openraft` backs off from; anything else is a network error
    /// it retries sooner.
    fn rpc_error(&self, status: &Status, action: RPCTypes, ttl: Duration) -> RpcError {
        match status.code() {
            Code::DeadlineExceeded => RPCError::Timeout(Timeout {
                action,
                id: self.id,
                target: self.to.node,
                timeout: ttl,
            }),
            Code::Unavailable
            | Code::FailedPrecondition
            | Code::NotFound
            | Code::Unimplemented
            | Code::OutOfRange
            | Code::ResourceExhausted => RPCError::Unreachable(Unreachable::new(status)),
            _ => RPCError::Network(NetworkError::new(status)),
        }
    }
}

fn streaming(e: RpcError) -> StreamingError<RaftTypes> {
    match e {
        RPCError::Timeout(e) => StreamingError::Timeout(e),
        RPCError::Unreachable(e) => StreamingError::Unreachable(e),
        RPCError::Network(e) => StreamingError::Network(e),
        RPCError::RemoteError(e) => match e.source {},
    }
}

type Answer = Result<StreamAppendResult<RaftTypes>, RpcError>;

impl PeerClient {
    /// Open `StreamAppend` with `input` as its messages. The soft TTL
    /// bounds the opening and then each wait for an answer; the stream
    /// itself lives as long as `openraft` keeps it.
    ///
    /// The opening waits for `input`'s first append before its deadline
    /// starts: `openraft` holds that append back for as long as it backs
    /// off from the peer, longer than the soft TTL, and the server opens
    /// its answers only on a first message.
    async fn open_stream<S>(
        &self,
        mut input: S,
        option: RPCOption,
    ) -> Result<BoxStream<'_, Answer>, RpcError>
    where
        S: Stream<Item = AppendEntriesRequest<RaftTypes>> + OptionalSend + Unpin + 'static,
    {
        let channel = self.channel.clone()?;
        let Some(first) = input.next().await else {
            return Ok(Box::pin(futures_util::stream::empty()));
        };
        let input = futures_util::stream::iter([first]).chain(input);
        let ttl = option.soft_ttl();
        let peer = Peer::parse(&self.addr);
        let mut call = StreamCall::client(self.tracing.clone(), STREAM_APPEND, peer);
        let sent = Arc::new(Sent {
            unanswered: Mutex::default(),
            refused: Mutex::default(),
            carried: call.carried(),
        });
        let (release, released) = oneshot::channel();
        let requests = outbound(self.to, input, Arc::clone(&sent), released);
        let open = async move {
            let mut grpc = tonic::client::Grpc::new(channel)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES);
            grpc.ready()
                .await
                .map_err(|e| Status::unavailable(e.to_string()))?;
            let codec = ProstCodec::<AppendRequest, StreamAppendResponse>::default();
            let path = PathAndQuery::from_static(STREAM_APPEND);
            grpc.streaming(Request::new(requests), path, codec).await
        };
        let opened = tokio::time::timeout(ttl, open)
            .await
            .unwrap_or_else(|_| Err(Status::deadline_exceeded("peer stream did not open")));
        match opened {
            Ok(response) => {
                let replies = Replies {
                    client: self,
                    incoming: response.into_inner(),
                    sent,
                    call,
                    ttl,
                    progress: Instant::now(),
                    _release: release,
                };
                let answers = futures_util::stream::unfold(Some(replies), |replies| async move {
                    replies?.next().await
                });
                Ok(Box::pin(answers))
            }
            Err(status) => {
                call.end(status.code());
                Err(self.rpc_error(&status, RPCTypes::AppendEntries, ttl))
            }
        }
    }
}

/// What a stream's writer leaves its reader: when each unanswered
/// message went out, oldest first, and why it stopped writing early.
struct Sent {
    unanswered: Mutex<VecDeque<Instant>>,
    refused: Mutex<Option<Malformed>>,
    carried: Carried,
}

impl Sent {
    fn unanswered(&self) -> std::sync::MutexGuard<'_, VecDeque<Instant>> {
        self.unanswered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn refused(&self) -> Option<Malformed> {
        self.refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn refuse(&self, e: Malformed) {
        *self.refused.lock().unwrap_or_else(PoisonError::into_inner) = Some(e);
    }
}

/// `openraft`'s appends as messages, each split to fit
/// [`MAX_MESSAGE_BYTES`]: the follower answers each piece, and each
/// answer stays within the append it came from.
struct Outbound<S> {
    input: S,
    to: Recipient,
    /// The append being sent, encoded a piece at a time as the stream
    /// takes them.
    pieces: Option<Pieces<AppendEntriesRequest<RaftTypes>>>,
    sent: Arc<Sent>,
    /// Resolves when the replies drop: the stream stays open while its
    /// requests live, and `openraft` may hold its input open and idle.
    released: oneshot::Receiver<()>,
}

impl<S> Outbound<S>
where
    S: Stream<Item = AppendEntriesRequest<RaftTypes>> + Unpin,
{
    async fn next(mut self) -> Option<(AppendRequest, Self)> {
        loop {
            match self.pieces.as_mut().and_then(Iterator::next) {
                Some(Ok(piece)) => {
                    self.sent.carried.note(&piece);
                    self.sent.unanswered().push_back(Instant::now());
                    return Some((piece, self));
                }
                Some(Err(e)) => {
                    self.sent.refuse(e);
                    return None;
                }
                // Every piece is sent: free the append's entries before
                // waiting, possibly long, for the next one.
                None => self.pieces = None,
            }
            let rpc = tokio::select! {
                rpc = self.input.next() => rpc?,
                _ = &mut self.released => return None,
            };
            self.pieces = Some(Pieces::new(self.to, rpc));
        }
    }
}

fn outbound<S>(
    to: Recipient,
    input: S,
    sent: Arc<Sent>,
    released: oneshot::Receiver<()>,
) -> impl Stream<Item = AppendRequest> + Send + 'static
where
    S: Stream<Item = AppendEntriesRequest<RaftTypes>> + OptionalSend + Unpin + 'static,
{
    let outbound = Outbound {
        input,
        to,
        pieces: None,
        sent,
        released,
    };
    futures_util::stream::unfold(outbound, Outbound::next)
}

/// The follower's answers to a stream, ending after the first that ends
/// replication or fails.
struct Replies<'s> {
    client: &'s PeerClient,
    incoming: Streaming<StreamAppendResponse>,
    sent: Arc<Sent>,
    call: StreamCall<'s>,
    ttl: Duration,
    /// When the stream last made progress: opened, or answered.
    progress: Instant,
    _release: oneshot::Sender<()>,
}

impl Replies<'_> {
    async fn next(mut self) -> Option<(Answer, Option<Self>)> {
        let answer = match self.reply().await {
            Ok(Some(reply)) => reply
                .into_raft()
                .map_err(|e| Status::internal(e.to_string())),
            Ok(None) => Err(self.unfinished()?),
            Err(status) => Err(status),
        };
        match answer {
            Ok(Ok(matched)) => Some((Ok(Ok(matched)), Some(self))),
            Ok(ended) => {
                self.call.end(Code::Ok);
                Some((Ok(ended), None))
            }
            Err(status) => {
                self.call.end(status.code());
                let action = RPCTypes::AppendEntries;
                Some((Err(self.client.rpc_error(&status, action, self.ttl)), None))
            }
        }
    }

    /// Why a stream the follower closed is unfinished, if it is: a
    /// message this end could not encode, or messages left unanswered.
    fn unfinished(&mut self) -> Option<Status> {
        if let Some(e) = self.sent.refused() {
            return Some(Status::internal(e.to_string()));
        }
        if self.sent.unanswered().is_empty() {
            self.call.end(Code::Ok);
            return None;
        }
        Some(Status::internal("the peer closed the stream unanswered"))
    }

    /// The next reply, due `ttl` after the oldest unanswered message went
    /// out or the last progress, whichever is later. An idle stream, with
    /// nothing unanswered, waits as long as it stays idle.
    async fn reply(&mut self) -> Result<Option<StreamAppendResponse>, Status> {
        loop {
            let oldest = self.sent.unanswered().front().copied();
            let due = oldest.map(|sent| sent.max(self.progress) + self.ttl);
            let wake = due.unwrap_or_else(|| Instant::now() + self.ttl);
            match tokio::time::timeout_at(wake, self.incoming.message()).await {
                Ok(reply) => {
                    if matches!(reply, Ok(Some(_))) {
                        self.sent.unanswered().pop_front();
                        self.progress = Instant::now();
                    }
                    return reply;
                }
                Err(_) if due.is_some() => {
                    return Err(Status::deadline_exceeded("peer stopped answering"));
                }
                Err(_) => {}
            }
        }
    }
}

impl RaftNetworkV2<RaftTypes> for PeerClient {
    type SnapshotData = ();

    /// Pipelined: messages go out as `openraft` yields them, without
    /// waiting for answers, and the follower answers them in order.
    fn stream_append<S>(
        &mut self,
        input: S,
        option: RPCOption,
    ) -> BoxFuture<'_, Result<BoxStream<'_, Answer>, RpcError>>
    where
        S: Stream<Item = AppendEntriesRequest<RaftTypes>> + OptionalSend + Unpin + 'static,
    {
        Box::pin(self.open_stream(input, option))
    }

    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<RaftTypes>,
        option: RPCOption,
    ) -> Result<AppendEntriesResponse<RaftTypes>, RpcError> {
        let (request, cut) = AppendRequest::bounded(self.to, &rpc).map_err(|e| network(&e))?;
        let response = self
            .exchange::<_, AppendResponse>(APPEND_RPC, Ok(request), &option)
            .await?;
        Ok(match (response, cut) {
            (AppendEntriesResponse::Success, Some(last)) => {
                AppendEntriesResponse::PartialSuccess(Some(last))
            }
            (response, _) => response,
        })
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<RaftTypes>,
        option: RPCOption,
    ) -> Result<VoteResponse<RaftTypes>, RpcError> {
        let request = Ok(WireVoteRequest::new(self.to, &rpc));
        self.exchange::<_, WireVoteResponse>(VOTE_RPC, request, &option)
            .await
    }

    async fn pre_vote(
        &mut self,
        rpc: VoteRequest<RaftTypes>,
        option: RPCOption,
    ) -> Result<VoteResponse<RaftTypes>, RpcError> {
        let request = Ok(WireVoteRequest::new(self.to, &rpc));
        self.exchange::<_, WireVoteResponse>(PRE_VOTE_RPC, request, &option)
            .await
    }

    // @codescene(disable:"Excess Number of Function Arguments")
    // The arguments are `RaftNetworkV2::full_snapshot`'s signature.
    async fn full_snapshot(
        &mut self,
        vote: Vote,
        snapshot: SnapshotOf<RaftTypes, ()>,
        cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        option: RPCOption,
    ) -> Result<SnapshotResponse<RaftTypes>, StreamingError<RaftTypes>> {
        let request = SnapshotRequest::new(self.to, &vote, &snapshot.meta);
        let install =
            self.exchange::<_, WireSnapshotResponse>(INSTALL_SNAPSHOT_RPC, request, &option);
        tokio::select! {
            closed = cancel => Err(StreamingError::Closed(closed)),
            response = install => response.map_err(streaming),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> PeerClient {
        PeerClient {
            id: NodeId::from(1),
            to: Recipient {
                group: 0,
                node: NodeId::from(2),
            },
            addr: "127.0.0.1:1".to_owned(),
            channel: Err(Unreachable::new(&Status::unavailable("unused"))),
            tracing: Tracing::default(),
        }
    }

    fn mapped(code: Code) -> RpcError {
        let ttl = Duration::from_millis(50);
        client().rpc_error(&Status::new(code, "refused"), RPCTypes::Vote, ttl)
    }

    #[test]
    fn a_refused_version_or_group_is_an_unreachable_peer() {
        for code in [
            Code::FailedPrecondition,
            Code::NotFound,
            Code::Unavailable,
            Code::Unimplemented,
            Code::OutOfRange,
            Code::ResourceExhausted,
        ] {
            assert!(matches!(mapped(code), RPCError::Unreachable(_)), "{code:?}");
        }
    }

    #[test]
    fn a_deadline_is_a_timeout_and_other_failures_are_network_errors() {
        assert!(matches!(
            mapped(Code::DeadlineExceeded),
            RPCError::Timeout(Timeout { timeout, .. }) if timeout == Duration::from_millis(50)
        ));
        for code in [Code::Internal, Code::InvalidArgument, Code::Unknown] {
            assert!(matches!(mapped(code), RPCError::Network(_)), "{code:?}");
        }
    }

    #[tokio::test]
    async fn an_unparsable_peer_address_is_unreachable() {
        let mut network = Peers::plaintext().network(NodeId::from(1), 0);
        let mut client = network
            .new_client(NodeId::from(2), &"not an address\n".to_owned())
            .await;
        let rpc = VoteRequest::new(Vote::new(1, NodeId::from(1)), None);
        let result = client
            .vote(rpc, RPCOption::new(Duration::from_secs(1)))
            .await;
        assert!(
            matches!(result, Err(RPCError::Unreachable(_))),
            "{result:?}"
        );
    }
}
