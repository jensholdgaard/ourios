//! The peer client: `openraft`'s network for one group, over a channel per
//! peer address that every group on the node shares.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use openraft::errors::{
    NetworkError, RPCError, ReplicationClosed, StreamingError, Timeout, Unreachable,
};
use openraft::network::{RPCOption, RPCTypes, RaftNetworkFactory, v2::RaftNetworkV2};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, SnapshotResponse, VoteRequest, VoteResponse,
};
use openraft::type_config::alias::SnapshotOf;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Request, Status};
use tonic_prost::ProstCodec;
use tracing::Instrument as _;

use super::super::types::{NodeId, RaftTypes, Vote};
use super::wire::{
    AppendRequest, AppendResponse, Malformed, Reply, SnapshotRequest, WireSnapshotResponse,
    WireVoteRequest, WireVoteResponse,
};
use super::{APPEND, Call, INSTALL_SNAPSHOT, PRE_VOTE, Side, Traced, VOTE, rpc_method};

type RpcError = RPCError<RaftTypes>;

/// One lazily connected channel per peer address. Clones share the pool.
#[derive(Clone, Default)]
pub struct Peers {
    channels: Arc<Mutex<HashMap<String, Channel>>>,
}

impl Peers {
    /// The pool for plaintext peers.
    #[must_use]
    pub fn plaintext() -> Self {
        Self::default()
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
            target,
            group: self.group,
            addr: node.clone(),
            channel,
        }
    }
}

/// One group's connection to one peer.
pub struct PeerClient {
    id: NodeId,
    target: NodeId,
    group: u32,
    addr: String,
    /// An address that does not parse is a peer that cannot be reached.
    channel: Result<Channel, Unreachable<RaftTypes>>,
}

fn network(e: &Malformed) -> RpcError {
    RPCError::Network(NetworkError::new(e))
}

/// The host and port of a `host:port` peer address.
fn host_port(addr: &str) -> (&str, Option<u16>) {
    match addr.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().ok()),
        None => (addr, None),
    }
}

fn client_span(path: &'static str, host: &str, port: Option<u16>) -> tracing::Span {
    let method = rpc_method(path);
    tracing::info_span!(
        "grpc client",
        otel.name = method,
        otel.kind = "client",
        rpc.system.name = "grpc",
        rpc.method = method,
        server.address = host,
        server.port = port,
        rpc.response.status_code = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
        error.type = tracing::field::Empty,
    )
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
        let ttl = option.hard_ttl();
        let (host, port) = host_port(&self.addr);
        let span = message.traced().then(|| client_span(rpc.path, host, port));
        let trace = Call::new(Side::Client, rpc.path, Some(host), span);
        let call = async move {
            let mut grpc = tonic::client::Grpc::new(channel);
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

    /// Transport failures, a version the peer refuses and a group it does
    /// not host are all an unreachable peer, which `openraft` backs off
    /// from; anything else is a network error it retries sooner.
    fn rpc_error(&self, status: &Status, action: RPCTypes, ttl: Duration) -> RpcError {
        match status.code() {
            Code::DeadlineExceeded => RPCError::Timeout(Timeout {
                action,
                id: self.id,
                target: self.target,
                timeout: ttl,
            }),
            Code::Unavailable | Code::FailedPrecondition | Code::NotFound | Code::Unimplemented => {
                RPCError::Unreachable(Unreachable::new(status))
            }
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

impl RaftNetworkV2<RaftTypes> for PeerClient {
    type SnapshotData = ();

    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<RaftTypes>,
        option: RPCOption,
    ) -> Result<AppendEntriesResponse<RaftTypes>, RpcError> {
        let request = AppendRequest::new(self.group, &rpc);
        self.exchange::<_, AppendResponse>(APPEND_RPC, request, &option)
            .await
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<RaftTypes>,
        option: RPCOption,
    ) -> Result<VoteResponse<RaftTypes>, RpcError> {
        let request = Ok(WireVoteRequest::new(self.group, &rpc));
        self.exchange::<_, WireVoteResponse>(VOTE_RPC, request, &option)
            .await
    }

    async fn pre_vote(
        &mut self,
        rpc: VoteRequest<RaftTypes>,
        option: RPCOption,
    ) -> Result<VoteResponse<RaftTypes>, RpcError> {
        let request = Ok(WireVoteRequest::new(self.group, &rpc));
        self.exchange::<_, WireVoteResponse>(PRE_VOTE_RPC, request, &option)
            .await
    }

    async fn full_snapshot(
        &mut self,
        vote: Vote,
        snapshot: SnapshotOf<RaftTypes, ()>,
        cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        option: RPCOption,
    ) -> Result<SnapshotResponse<RaftTypes>, StreamingError<RaftTypes>> {
        let request = SnapshotRequest::new(self.group, &vote, &snapshot.meta);
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
            target: NodeId::from(2),
            group: 0,
            addr: "127.0.0.1:1".to_owned(),
            channel: Err(Unreachable::new(&Status::unavailable("unused"))),
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
