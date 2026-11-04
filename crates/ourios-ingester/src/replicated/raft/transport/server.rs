//! The `ourios.wal.v1.Replication` server: one per node, handing each
//! request to the `Raft` of the group it names.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::sync::{Arc, PoisonError, RwLock};
use std::task::{Context, Poll};

use openraft::Raft;
use openraft::errors::Fatal;
use openraft::storage::{RaftStateMachine, Snapshot};
use tonic::body::Body;
use tonic::codegen::{Body as HttpBody, BoxFuture, StdError, http};
use tonic::server::{NamedService, UnaryService};
use tonic::transport::server::TcpIncoming;
use tonic::{Code, Request, Response, Status};
use tonic_prost::ProstCodec;
use tracing::Instrument as _;

use super::super::types::RaftTypes;
use super::trace::{Call, Traced, Tracing};
use super::wire::{
    AppendRequest, AppendResponse, Header, MAX_MESSAGE_BYTES, Malformed, SnapshotRequest,
    WireSnapshotResponse, WireVoteRequest, WireVoteResponse,
};
use super::{APPEND, INSTALL_SNAPSHOT, PRE_VOTE, SERVICE, VERSION, VOTE};

type Groups<SM> = HashMap<u32, Raft<RaftTypes, SM>>;

/// Serves every group this node hosts. Clones share one registry.
pub struct ReplicationServer<SM>
where
    SM: RaftStateMachine<RaftTypes>,
{
    groups: Arc<RwLock<Groups<SM>>>,
    tracing: Tracing,
}

impl<SM> Clone for ReplicationServer<SM>
where
    SM: RaftStateMachine<RaftTypes>,
{
    fn clone(&self) -> Self {
        Self {
            groups: Arc::clone(&self.groups),
            tracing: self.tracing.clone(),
        }
    }
}

impl<SM> Default for ReplicationServer<SM>
where
    SM: RaftStateMachine<RaftTypes>,
{
    fn default() -> Self {
        Self {
            groups: Arc::default(),
            tracing: Tracing::default(),
        }
    }
}

impl From<Malformed> for Status {
    fn from(e: Malformed) -> Self {
        Self::invalid_argument(e.to_string())
    }
}

/// A stopped `Raft` is a peer that is not there: `UNAVAILABLE`, which the
/// client reports as unreachable.
fn stopped(e: &impl std::fmt::Display) -> Status {
    Status::unavailable(e.to_string())
}

impl<SM> ReplicationServer<SM>
where
    SM: RaftStateMachine<RaftTypes, SnapshotData = ()>,
{
    /// Route `group`'s requests to `raft`, replacing any earlier handle.
    pub fn insert(&self, group: u32, raft: Raft<RaftTypes, SM>) {
        self.write().insert(group, raft);
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Groups<SM>> {
        self.groups.write().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(super) fn traced_by(tracing: Tracing) -> Self {
        Self {
            groups: Arc::default(),
            tracing,
        }
    }

    /// The `Raft` a request is for. A replica other than the one this node
    /// hosts for the group is `NOT_FOUND` like an unknown group: a reply
    /// from it would be credited to a voter that never received the
    /// request.
    fn route(&self, header: Option<Header>) -> Result<Raft<RaftTypes, SM>, Status> {
        let Header {
            version,
            group,
            target,
        } = header.unwrap_or_default();
        if version != VERSION {
            return Err(Status::failed_precondition(format!(
                "peer protocol version {version} is not {VERSION}"
            )));
        }
        let groups = self.groups.read().unwrap_or_else(PoisonError::into_inner);
        match groups.get(&group) {
            Some(raft) if target == raft.node_id().uuid().as_bytes() => Ok(raft.clone()),
            Some(raft) => Err(Status::not_found(format!(
                "group {group} has replica {} here, not the one addressed",
                raft.node_id()
            ))),
            None => Err(Status::not_found(format!(
                "group {group} is not hosted here"
            ))),
        }
    }

    async fn append(&self, mut request: AppendRequest) -> Result<AppendResponse, Status> {
        let raft = self.route(request.header.take())?;
        let rpc = request.into_raft()?;
        let response = raft.append_entries(rpc).await.map_err(|e| stopped(&e))?;
        Ok(AppendResponse::from(&response))
    }

    async fn vote(
        &self,
        mut request: WireVoteRequest,
        pre: bool,
    ) -> Result<WireVoteResponse, Status> {
        let raft = self.route(request.header.take())?;
        let rpc = request.into_raft()?;
        let response = if pre {
            raft.pre_vote(rpc).await
        } else {
            raft.vote(rpc).await
        };
        let response = response.map_err(|e| stopped(&e))?;
        Ok(WireVoteResponse::from(&response))
    }

    async fn install_snapshot(
        &self,
        mut request: SnapshotRequest,
    ) -> Result<WireSnapshotResponse, Status> {
        let raft = self.route(request.header.take())?;
        let (vote, meta) = request.into_raft()?;
        let snapshot = Snapshot { meta, snapshot: () };
        let response = raft
            .install_full_snapshot(vote, snapshot)
            .await
            .map_err(|e: Fatal<RaftTypes>| stopped(&e))?;
        Ok(WireSnapshotResponse::from(&response))
    }

    /// Serve until `shutdown` resolves. The incoming stream is where the
    /// RFC 0030 TLS acceptor will sit.
    ///
    /// # Errors
    ///
    /// The transport's error when the listener fails.
    pub async fn serve(
        self,
        incoming: TcpIncoming,
        shutdown: impl Future<Output = ()>,
    ) -> Result<(), tonic::transport::Error> {
        tonic::transport::Server::builder()
            .serve_with_incoming_shutdown(self, incoming, shutdown)
            .await
    }
}

/// One unary method: run the decoded request through the handler under
/// its gRPC server span.
struct Method<F> {
    path: &'static str,
    tracing: Tracing,
    handler: F,
}

impl<Req, Resp, F, Fut> UnaryService<Req> for Method<F>
where
    Req: Traced,
    F: FnMut(Req) -> Fut,
    Fut: Future<Output = Result<Resp, Status>> + Send + 'static,
    Resp: Send + 'static,
{
    type Response = Resp;
    type Future = BoxFuture<Response<Resp>, Status>;

    fn call(&mut self, request: Request<Req>) -> Self::Future {
        let request = request.into_inner();
        let call = Call::server(self.tracing.clone(), self.path, request.traced());
        let handled = (self.handler)(request).instrument(call.span());
        Box::pin(async move {
            let result = handled.await;
            call.finish(result.as_ref().map_or_else(Status::code, |_| Code::Ok));
            result.map(Response::new)
        })
    }
}

fn unary<B, Req, Resp, F, Fut>(
    path: &'static str,
    tracing: Tracing,
    request: http::Request<B>,
    handler: F,
) -> BoxFuture<http::Response<Body>, Infallible>
where
    B: HttpBody + Send + 'static,
    B::Error: Into<StdError> + Send + 'static,
    Req: prost::Message + Traced + Default + Send + 'static,
    Resp: prost::Message + Send + 'static,
    F: FnMut(Req) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Resp, Status>> + Send + 'static,
{
    Box::pin(async move {
        let mut grpc = tonic::server::Grpc::new(ProstCodec::<Resp, Req>::default())
            .apply_max_message_size_config(Some(MAX_MESSAGE_BYTES), Some(MAX_MESSAGE_BYTES));
        let method = Method {
            path,
            tracing,
            handler,
        };
        Ok(grpc.unary(method, request).await)
    })
}

fn unimplemented() -> BoxFuture<http::Response<Body>, Infallible> {
    Box::pin(async { Ok(Status::unimplemented("no such method").into_http()) })
}

impl<SM, B> tonic::codegen::Service<http::Request<B>> for ReplicationServer<SM>
where
    SM: RaftStateMachine<RaftTypes, SnapshotData = ()>,
    B: HttpBody + Send + 'static,
    B::Error: Into<StdError> + Send + 'static,
{
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let server = self.clone();
        let tracing = self.tracing.clone();
        match request.uri().path() {
            APPEND => unary(APPEND, tracing, request, move |r| {
                let server = server.clone();
                async move { server.append(r).await }
            }),
            VOTE => unary(VOTE, tracing, request, move |r| {
                let server = server.clone();
                async move { server.vote(r, false).await }
            }),
            PRE_VOTE => unary(PRE_VOTE, tracing, request, move |r| {
                let server = server.clone();
                async move { server.vote(r, true).await }
            }),
            INSTALL_SNAPSHOT => unary(INSTALL_SNAPSHOT, tracing, request, move |r| {
                let server = server.clone();
                async move { server.install_snapshot(r).await }
            }),
            _ => unimplemented(),
        }
    }
}

impl<SM> NamedService for ReplicationServer<SM>
where
    SM: RaftStateMachine<RaftTypes>,
{
    const NAME: &'static str = SERVICE;
}
