//! The `ourios.wal.v1.Replication` server: one per node, handing each
//! request to the `Raft` of the group it names.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::task::{Context, Poll};

use futures_util::{Stream, StreamExt as _};
use openraft::Raft;
use openraft::errors::Fatal;
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, AppendResult, StreamAppendError,
};
use openraft::storage::RaftStateMachine;
use ourios_serving::tls_serve::{ReloadingAcceptor, tls_incoming};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{oneshot, watch};
use tonic::body::Body;
use tonic::codegen::{Body as HttpBody, BoxFuture, StdError, http};
use tonic::server::{NamedService, StreamingService, UnaryService};
use tonic::transport::server::{Connected, TcpIncoming};
use tonic::{Code, Request, Response, Status, Streaming};
use tonic_prost::ProstCodec;
use tracing::Instrument as _;

use super::super::snapshot_record::GroupState;
use super::super::types::RaftTypes;
use super::forward::{self, ForwardTarget};
use super::identify::{self, Identity};
use super::trace::{Call, Carried, StreamCall, Traced, Tracing};
use super::wire::{
    AppendRequest, AppendResponse, Header, MAX_MESSAGE_BYTES, Malformed, SnapshotRequest,
    StreamAppendResponse, WireSnapshotResponse, WireVoteRequest, WireVoteResponse,
};
use super::{
    APPEND, FORWARD, IDENTIFY, INSTALL_SNAPSHOT, PRE_VOTE, SERVICE, STREAM_APPEND, VERSION, VOTE,
};

type Groups<SM> = HashMap<u32, Raft<RaftTypes, SM>>;

/// Serves every group this node hosts. Clones share one registry.
pub struct ReplicationServer<SM>
where
    SM: RaftStateMachine<RaftTypes>,
{
    groups: Arc<RwLock<Groups<SM>>>,
    /// The node's ingest for `Forward`; until it is set, every forwarded
    /// export is refused as not hosted.
    forwards: Arc<RwLock<Option<Arc<dyn ForwardTarget>>>>,
    /// Who this node is, for `Identify`; until it is set the call is
    /// unavailable.
    identity: Arc<RwLock<Option<Identity>>>,
    tracing: Tracing,
    /// Set once `serve`'s shutdown resolves. Graceful shutdown waits for
    /// every running call, so a `StreamAppend` waiting on its peer must
    /// end on it rather than on the peer.
    closing: Arc<watch::Sender<bool>>,
}

impl<SM> Clone for ReplicationServer<SM>
where
    SM: RaftStateMachine<RaftTypes>,
{
    fn clone(&self) -> Self {
        Self {
            groups: Arc::clone(&self.groups),
            forwards: Arc::clone(&self.forwards),
            identity: Arc::clone(&self.identity),
            tracing: self.tracing.clone(),
            closing: Arc::clone(&self.closing),
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
            forwards: Arc::default(),
            identity: Arc::default(),
            tracing: Tracing::default(),
            closing: Arc::new(watch::Sender::new(false)),
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
    SM: RaftStateMachine<RaftTypes, SnapshotData = GroupState>,
{
    /// Route `group`'s requests to `raft`, replacing any earlier handle.
    pub fn insert(&self, group: u32, raft: Raft<RaftTypes, SM>) {
        self.write().insert(group, raft);
    }

    /// Commit forwarded exports with `target`, replacing any earlier one.
    pub fn serve_forwards(&self, target: Arc<dyn ForwardTarget>) {
        *self
            .forwards
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(target);
    }

    /// Answer `Identify` as `identity`, replacing any earlier one.
    pub fn identify_as(&self, identity: Identity) {
        *self
            .identity
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(identity);
    }

    fn identity(&self) -> Option<Identity> {
        let identity = self.identity.read().unwrap_or_else(PoisonError::into_inner);
        identity.clone()
    }

    fn forward_target(&self) -> Option<Arc<dyn ForwardTarget>> {
        let target = self.forwards.read().unwrap_or_else(PoisonError::into_inner);
        target.clone()
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Groups<SM>> {
        self.groups.write().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(super) fn traced_by(tracing: Tracing) -> Self {
        Self {
            groups: Arc::default(),
            forwards: Arc::default(),
            identity: Arc::default(),
            tracing,
            closing: Arc::new(watch::Sender::new(false)),
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
        Ok(AppendResponse::from(&AppendEntriesResponse::from(response)))
    }

    /// Open a `StreamAppend` stream: its first message routes it, and the
    /// group's `Raft` answers each message in order.
    async fn stream_append(
        &self,
        mut incoming: Streaming<AppendRequest>,
    ) -> Result<AnswerStream, Status> {
        let mut call = StreamCall::server(self.tracing.clone(), STREAM_APPEND);
        let mut closing = self.closing.subscribe();
        let first = tokio::select! {
            first = incoming.message() => first,
            _ = closing.wait_for(|closing| *closing) => {
                return Err(refuse(&mut call, Status::unavailable("the server is shutting down")));
            }
        };
        let first = match first {
            Ok(Some(first)) => first,
            Ok(None) => {
                call.end(Code::Ok);
                return Ok(Box::pin(futures_util::stream::empty()));
            }
            Err(status) => return Err(refuse(&mut call, status)),
        };
        let raft = self
            .route(first.header.clone())
            .map_err(|status| refuse(&mut call, status))?;
        let opening = Opening {
            first,
            rest: incoming,
        };
        Ok(Box::pin(self.answers(&raft, opening, call)))
    }

    /// `raft`'s answers to the stream `opening` begins.
    fn answers<'r>(
        &self,
        raft: &'r Raft<RaftTypes, SM>,
        Opening { first, rest }: Opening,
        call: StreamCall<'static>,
    ) -> impl Stream<Item = Result<StreamAppendResponse, Status>> + Send + 'static + use<'r, SM>
    {
        let mut closing = self.closing.subscribe();
        let reader = Arc::new(Reader {
            refused: Mutex::default(),
            admitted: AtomicU64::default(),
            carried: call.carried(),
        });
        let (release, released) = oneshot::channel();
        let inbound = Inbound {
            header: first.header.clone(),
            messages: futures_util::stream::once(std::future::ready(Ok(first))).chain(rest),
            reader: Arc::clone(&reader),
            released,
        };
        let requests = futures_util::stream::unfold(inbound, Inbound::next);
        let watched = raft.wait(None);
        let stopped = async move {
            tokio::select! {
                _ = watched.metrics(|m| m.running_state.is_err(), "the group stops") => {}
                _ = closing.wait_for(|closing| *closing) => {}
            }
        };
        let answers = Answers {
            results: Box::pin(raft.stream_append(requests)),
            stopped: Box::pin(stopped),
            reader,
            answered: 0,
            call,
            _release: release,
        };
        futures_util::stream::unfold(
            Some(answers),
            |answers| async move { answers?.next().await },
        )
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
        let (vote, snapshot) = request.into_raft()?;
        let response = raft
            .install_full_snapshot(vote, snapshot)
            .await
            .map_err(|e: Fatal<RaftTypes>| stopped(&e))?;
        Ok(WireSnapshotResponse::from(&response))
    }

    /// Serve over mutual TLS until `shutdown` resolves (RFC 0065 §6.8):
    /// `acceptor` comes from the peer listener's settings, whose client CA
    /// every caller's certificate must chain to, and reloads with them.
    ///
    /// # Errors
    ///
    /// The transport's error when the listener fails.
    pub async fn serve(
        self,
        incoming: TcpIncoming,
        acceptor: ReloadingAcceptor,
        shutdown: impl Future<Output = ()>,
    ) -> Result<(), tonic::transport::Error> {
        self.run(tls_incoming(incoming, acceptor), shutdown).await
    }

    #[cfg(test)]
    pub(in crate::replicated::raft) async fn serve_plaintext(
        self,
        incoming: TcpIncoming,
        shutdown: impl Future<Output = ()>,
    ) -> Result<(), tonic::transport::Error> {
        self.run(incoming, shutdown).await
    }

    async fn run<IO>(
        self,
        incoming: impl Stream<Item = std::io::Result<IO>>,
        shutdown: impl Future<Output = ()>,
    ) -> Result<(), tonic::transport::Error>
    where
        IO: AsyncRead + AsyncWrite + Connected + Unpin + Send + 'static,
    {
        let closing = Arc::clone(&self.closing);
        let shutdown = async move {
            shutdown.await;
            closing.send_replace(true);
        };
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

type AnswerStream = Pin<Box<dyn Stream<Item = Result<StreamAppendResponse, Status>> + Send>>;

fn refuse(call: &mut StreamCall<'_>, status: Status) -> Status {
    call.end(status.code());
    status
}

/// What a stream's reader leaves its writer: why it stopped reading
/// early, and how many appends it handed the `Raft`.
struct Reader {
    refused: Mutex<Option<Status>>,
    admitted: AtomicU64,
    carried: Carried,
}

impl Reader {
    fn refuse(&self, status: Status) {
        *self.refused.lock().unwrap_or_else(PoisonError::into_inner) = Some(status);
    }

    fn refused(&self) -> Option<Status> {
        self.refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// A stream's messages as `openraft` appends, ending at the first one
/// that is refused, which it leaves in the [`Reader`], or once the
/// answers end.
struct Inbound<M> {
    messages: M,
    header: Option<Header>,
    reader: Arc<Reader>,
    /// Resolves when the answers drop. `openraft` holds the requests
    /// until its next read, and the stream stays open while they live.
    released: oneshot::Receiver<()>,
}

impl<M> Inbound<M>
where
    M: Stream<Item = Result<AppendRequest, Status>> + Unpin,
{
    async fn next(mut self) -> Option<(AppendEntriesRequest<RaftTypes>, Self)> {
        let message = tokio::select! {
            message = self.messages.next() => message?,
            _ = &mut self.released => return None,
        };
        match self.admit(message) {
            Ok(rpc) => Some((rpc, self)),
            Err(status) => {
                self.reader.refuse(status);
                None
            }
        }
    }

    /// Validated as a unary append is, under the header that routed the
    /// stream.
    fn admit(
        &self,
        message: Result<AppendRequest, Status>,
    ) -> Result<AppendEntriesRequest<RaftTypes>, Status> {
        let request = message?;
        if request.header != self.header {
            return Err(Status::invalid_argument(
                "every message of a stream carries the header of its first",
            ));
        }
        self.reader.carried.note(&request);
        let rpc = request.into_raft()?;
        self.reader.admitted.fetch_add(1, Ordering::Relaxed);
        Ok(rpc)
    }
}

/// The `Raft`'s answers to a stream, in order, ending after the first
/// that ends replication, after the reader's refusal, or when the `Raft`
/// stops.
/// A `StreamAppend` stream: the message that routed it, and the rest.
struct Opening {
    first: AppendRequest,
    rest: Streaming<AppendRequest>,
}

struct Answers<O> {
    results: O,
    /// Resolves once the `Raft` or the server stops: a stream the leader
    /// keeps open but idle would otherwise never end, and a server's
    /// graceful shutdown waits on every open stream.
    stopped: Pin<Box<dyn Future<Output = ()> + Send>>,
    reader: Arc<Reader>,
    answered: u64,
    call: StreamCall<'static>,
    _release: oneshot::Sender<()>,
}

impl<O> Answers<O>
where
    O: Stream<Item = Result<AppendResult<RaftTypes>, Fatal<RaftTypes>>> + Unpin,
{
    async fn next(mut self) -> Option<(Result<StreamAppendResponse, Status>, Option<Self>)> {
        let next = tokio::select! {
            biased;
            next = self.results.next() => next,
            () = &mut self.stopped => {
                let status = Status::unavailable("the group or the server stopped");
                return Some((Err(refuse(&mut self.call, status)), None));
            }
        };
        match next {
            Some(Ok(result)) => {
                self.answered += 1;
                let result = result.map_err(StreamAppendError::from);
                let response = Ok(StreamAppendResponse::from(&result));
                if result.is_ok() {
                    Some((response, Some(self)))
                } else {
                    self.call.end(Code::Ok);
                    Some((response, None))
                }
            }
            Some(Err(fatal)) => Some((Err(refuse(&mut self.call, stopped(&fatal))), None)),
            None => self.close().map(|status| (Err(status), None)),
        }
    }

    /// The status the stream ends on once the `Raft` stops answering:
    /// the reader's refusal, or `UNAVAILABLE` for appends it admitted that
    /// went unanswered, which is a `Raft` that stopped.
    fn close(mut self) -> Option<Status> {
        let unanswered = self.reader.admitted.load(Ordering::Relaxed) > self.answered;
        let status = self.reader.refused().or_else(|| {
            unanswered.then(|| Status::unavailable("the group stopped before answering"))
        });
        self.call
            .end(status.as_ref().map_or(Code::Ok, Status::code));
        status
    }
}

/// The `StreamAppend` method.
struct AppendStreams<SM>
where
    SM: RaftStateMachine<RaftTypes>,
{
    server: ReplicationServer<SM>,
}

impl<SM> StreamingService<AppendRequest> for AppendStreams<SM>
where
    SM: RaftStateMachine<RaftTypes, SnapshotData = GroupState>,
{
    type Response = StreamAppendResponse;
    type ResponseStream = AnswerStream;
    type Future = BoxFuture<Response<AnswerStream>, Status>;

    fn call(&mut self, request: Request<Streaming<AppendRequest>>) -> Self::Future {
        let server = self.server.clone();
        Box::pin(async move {
            let answers = server.stream_append(request.into_inner()).await?;
            Ok(Response::new(answers))
        })
    }
}

fn streaming<SM, B>(
    server: ReplicationServer<SM>,
    request: http::Request<B>,
) -> BoxFuture<http::Response<Body>, Infallible>
where
    SM: RaftStateMachine<RaftTypes, SnapshotData = GroupState>,
    B: HttpBody + Send + 'static,
    B::Error: Into<StdError> + Send + 'static,
{
    Box::pin(async move {
        let codec = ProstCodec::<StreamAppendResponse, AppendRequest>::default();
        let mut grpc = tonic::server::Grpc::new(codec)
            .apply_max_message_size_config(Some(MAX_MESSAGE_BYTES), Some(MAX_MESSAGE_BYTES));
        Ok(grpc.streaming(AppendStreams { server }, request).await)
    })
}

fn unimplemented() -> BoxFuture<http::Response<Body>, Infallible> {
    Box::pin(async { Ok(Status::unimplemented("no such method").into_http()) })
}

impl<SM, B> tonic::codegen::Service<http::Request<B>> for ReplicationServer<SM>
where
    SM: RaftStateMachine<RaftTypes, SnapshotData = GroupState>,
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
            FORWARD => unary(FORWARD, tracing, request, move |r| {
                let target = server.forward_target();
                forward::serve(target, r)
            }),
            IDENTIFY => unary(IDENTIFY, tracing, request, move |r| {
                let identity = server.identity();
                async move { identify::serve(identity, &r) }
            }),
            STREAM_APPEND => streaming(server, request),
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
