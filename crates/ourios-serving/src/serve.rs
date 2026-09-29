//! The HTTP serve loop shared by the OTLP/HTTP and querier listeners,
//! plaintext and TLS alike, and the connection deadlines it enforces.
//!
//! `axum::serve` builds its hyper connection with no timer and exposes no
//! deadlines, so a peer that stalls mid-request, or parks an idle
//! keep-alive connection, holds its socket for as long as it likes.
//! [`serve_http`] keeps `axum::serve`'s accept → spawn → graceful-drain
//! shape and adds:
//!
//! - [`HEADER_READ_TIMEOUT`]: a request head must be complete this long
//!   after its first byte (after the accept, for a connection's first
//!   request), or the connection is dropped;
//! - [`KEEP_ALIVE_IDLE_TIMEOUT`]: an HTTP/1 connection with no request in
//!   flight is closed after this long;
//! - [`HTTP2_KEEPALIVE_INTERVAL`] / [`HTTP2_KEEPALIVE_TIMEOUT`]: HTTP/2
//!   PINGs that reap a peer which vanished without a FIN;
//! - [`TCP_KEEPALIVE`] on every accepted socket.
//!
//! None of these bounds a request once its head has arrived: a request
//! deadline is client-visible and belongs to the ingest and query
//! admission contracts, not to transport hygiene.
//!
//! The two HTTP/1 clocks are kept here rather than on hyper's
//! `header_read_timeout`, because hyper starts that timer the moment a
//! keep-alive connection goes idle: it would also close idle connections
//! after the header-read timeout, and the idle timeout could never be the
//! longer of the two.

use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::Request;
use axum::serve::Listener;
use futures_core::Stream;
use http_body::{Body as HttpBody, Frame, SizeHint};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};
use tokio::time::{Instant, Sleep};
use tower_service::Service as _;

/// How long a client may take to send a complete HTTP/1 request head,
/// counted from its first byte. hyper's own default.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an HTTP/1 connection may sit with no request in flight before
/// the server closes it. At least as long as common client pool idle
/// timeouts, so the client is usually the side that closes first.
pub const KEEP_ALIVE_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Interval between the server's HTTP/2 keepalive PINGs, on the HTTP
/// listeners and on the gRPC listener.
pub const HTTP2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(60);

/// How long an HTTP/2 keepalive PING may go unanswered before the
/// connection is closed.
pub const HTTP2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Idle time before the kernel starts TCP keepalive probes on an accepted
/// socket.
pub const TCP_KEEPALIVE: Duration = Duration::from_secs(60);

/// Pause after an accept fails for want of file descriptors. Retrying at
/// once would spin: the condition clears only as other connections close.
const FD_EXHAUSTED_BACKOFF: Duration = Duration::from_millis(500);

/// Pause after any other accept error.
const ACCEPT_RETRY: Duration = Duration::from_millis(1);

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

#[derive(Clone, Copy, Debug)]
struct Deadlines {
    header_read: Duration,
    idle: Duration,
}

const DEADLINES: Deadlines = Deadlines {
    header_read: HEADER_READ_TIMEOUT,
    idle: KEEP_ALIVE_IDLE_TIMEOUT,
};

/// Serve `router` on `listener` until `shutdown` resolves, then stop
/// accepting, let every open connection finish its in-flight requests,
/// and return once all of them have closed.
///
/// `listener` is a [`PlainListener`] or a
/// [`TlsListener`](crate::tls_serve::TlsListener); both set
/// [`TCP_KEEPALIVE`] on the sockets they accept.
pub async fn serve_http<L>(listener: L, router: Router, shutdown: impl Future<Output = ()>)
where
    L: Listener,
{
    serve_with(listener, router, shutdown, DEADLINES).await;
}

async fn serve_with<L>(
    mut listener: L,
    router: Router,
    shutdown: impl Future<Output = ()>,
    deadlines: Deadlines,
) where
    L: Listener,
{
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http2()
        .timer(TokioTimer::new())
        .keep_alive_interval(HTTP2_KEEPALIVE_INTERVAL)
        .keep_alive_timeout(HTTP2_KEEPALIVE_TIMEOUT);

    let (stop_tx, stop_rx) = watch::channel(false);
    let (open_tx, open_rx) = watch::channel(());
    let mut shutdown = pin!(shutdown);
    loop {
        let (io, _) = tokio::select! {
            accepted = listener.accept() => accepted,
            () = &mut shutdown => break,
        };
        tokio::spawn(serve_connection(
            io,
            router.clone(),
            builder.clone(),
            stop_rx.clone(),
            open_rx.clone(),
            deadlines,
        ));
    }
    drop(listener);
    drop(open_rx);
    stop_tx.send_replace(true);
    open_tx.closed().await;
}

async fn serve_connection<I>(
    io: I,
    router: Router,
    builder: auto::Builder<TokioExecutor>,
    mut stop: watch::Receiver<bool>,
    _open: watch::Receiver<()>,
    deadlines: Deadlines,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let activity = Arc::new(Activity::new());
    let io = TokioIo::new(Tracked::new(io, Arc::clone(&activity)));
    let service = service_fn({
        let activity = Arc::clone(&activity);
        move |request: Request<Incoming>| {
            let guard = activity.begin_request();
            let mut router = router.clone();
            async move {
                let response = router.call(request.map(Body::new)).await?;
                Ok::<_, Infallible>(response.map(|body| GuardedBody {
                    body,
                    _guard: guard,
                }))
            }
        }
    });
    let mut conn = pin!(builder.serve_connection_with_upgrades(io, service));
    let mut stopping = false;
    loop {
        let deadline = activity.next_deadline(deadlines);
        tokio::select! {
            served = conn.as_mut() => {
                if let Err(e) = served {
                    tracing::debug!(error = %e, "HTTP connection ended with an error");
                }
                return;
            }
            _ = stop.wait_for(|stop| *stop), if !stopping => {
                stopping = true;
                activity.close();
                conn.as_mut().graceful_shutdown();
            }
            () = activity.changed.notified() => {}
            () = sleep_until(deadline) => {
                // Re-read: a request may have begun since the deadline was
                // taken, and a stale header deadline must not drop it.
                match activity.next_deadline(deadlines) {
                    Some((at, Expiry::HeaderRead)) if at <= Instant::now() => {
                        tracing::debug!("request head not received in time; dropping the connection");
                        return;
                    }
                    Some((at, Expiry::Idle)) if at <= Instant::now() => {
                        activity.close();
                        conn.as_mut().graceful_shutdown();
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn sleep_until(deadline: Option<(Instant, Expiry)>) {
    match deadline {
        Some((at, _)) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expiry {
    HeaderRead,
    Idle,
}

/// Where a connection is between requests. HTTP/1 only: an HTTP/2
/// connection is detected from its preface and left to the PING
/// keepalive.
#[derive(Clone, Copy, Debug)]
enum Phase {
    /// Waiting for, or part-way through, a request head.
    Head {
        since: Instant,
    },
    /// No request in flight and no byte of the next one yet.
    Idle {
        since: Instant,
    },
    /// A request is being handled or its response is being written.
    Busy {
        requests: usize,
    },
    Http2,
}

#[derive(Debug)]
struct ConnState {
    phase: Phase,
    /// Set once the connection has been told to close; an idle deadline
    /// is then moot, a header deadline is not.
    closing: bool,
}

#[derive(Debug)]
struct Activity {
    state: Mutex<ConnState>,
    changed: Notify,
}

impl Activity {
    fn new() -> Self {
        Self {
            state: Mutex::new(ConnState {
                phase: Phase::Head {
                    since: Instant::now(),
                },
                closing: false,
            }),
            changed: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ConnState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn next_deadline(&self, deadlines: Deadlines) -> Option<(Instant, Expiry)> {
        let state = self.lock();
        match state.phase {
            Phase::Head { since } => Some((since + deadlines.header_read, Expiry::HeaderRead)),
            Phase::Idle { since } if !state.closing => Some((since + deadlines.idle, Expiry::Idle)),
            Phase::Idle { .. } | Phase::Busy { .. } | Phase::Http2 => None,
        }
    }

    fn bytes_read(&self) {
        let mut state = self.lock();
        if let Phase::Idle { .. } = state.phase {
            state.phase = Phase::Head {
                since: Instant::now(),
            };
            drop(state);
            self.changed.notify_one();
        }
    }

    fn detected_http2(&self) {
        self.lock().phase = Phase::Http2;
    }

    fn close(&self) {
        self.lock().closing = true;
    }

    fn begin_request(self: &Arc<Self>) -> RequestGuard {
        let mut state = self.lock();
        state.phase = match state.phase {
            Phase::Head { .. } | Phase::Idle { .. } => Phase::Busy { requests: 1 },
            Phase::Busy { requests } => Phase::Busy {
                requests: requests + 1,
            },
            Phase::Http2 => Phase::Http2,
        };
        RequestGuard(Arc::clone(self))
    }
}

/// Holds its connection busy from the moment hyper hands the request to
/// the router until the response body has been written out.
#[derive(Debug)]
struct RequestGuard(Arc<Activity>);

impl Drop for RequestGuard {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        match state.phase {
            Phase::Busy { requests: 1 } => {
                state.phase = Phase::Idle {
                    since: Instant::now(),
                };
                drop(state);
                self.0.changed.notify_one();
            }
            Phase::Busy { requests } => {
                state.phase = Phase::Busy {
                    requests: requests.saturating_sub(1),
                };
            }
            Phase::Head { .. } | Phase::Idle { .. } | Phase::Http2 => {}
        }
    }
}

/// The router's response body, carrying the request's [`RequestGuard`]
/// so the connection counts as busy until hyper drops the body.
struct GuardedBody {
    body: Body,
    _guard: RequestGuard,
}

impl HttpBody for GuardedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

#[derive(Clone, Copy, Debug)]
enum Sniff {
    /// This many bytes of the HTTP/2 preface have matched so far.
    Matching(usize),
    Http1,
    Http2,
}

/// The connection's byte stream, reporting reads to its [`Activity`].
struct Tracked<I> {
    io: I,
    activity: Arc<Activity>,
    sniff: Sniff,
}

impl<I> Tracked<I> {
    fn new(io: I, activity: Arc<Activity>) -> Self {
        Self {
            io,
            activity,
            sniff: Sniff::Matching(0),
        }
    }

    fn observe(&mut self, read: &[u8]) {
        if let Sniff::Matching(seen) = self.sniff {
            let n = read.len().min(H2_PREFACE.len() - seen);
            let matched = read[..n] == H2_PREFACE[seen..seen + n];
            self.sniff = match seen + n {
                _ if !matched => Sniff::Http1,
                total if total == H2_PREFACE.len() => {
                    self.activity.detected_http2();
                    Sniff::Http2
                }
                total => Sniff::Matching(total),
            };
        }
        if !matches!(self.sniff, Sniff::Http2) {
            self.activity.bytes_read();
        }
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for Tracked<I> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        ready!(Pin::new(&mut this.io).poll_read(cx, buf))?;
        let read = &buf.filled()[before..];
        if !read.is_empty() {
            this.observe(read);
        }
        Poll::Ready(Ok(()))
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for Tracked<I> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

/// A plaintext `axum::serve::Listener` over a bound `TcpListener`: sets
/// [`TCP_KEEPALIVE`] on every accepted socket and backs off when the
/// process runs out of file descriptors.
#[derive(Debug)]
pub struct PlainListener {
    inner: TcpListener,
    listener: &'static str,
}

impl PlainListener {
    /// Wrap a bound `TcpListener`. `listener` names it in accept-error
    /// logs (`LISTENER_HTTP`, `LISTENER_QUERIER`).
    #[must_use]
    pub fn new(inner: TcpListener, listener: &'static str) -> Self {
        Self { inner, listener }
    }
}

impl Listener for PlainListener {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((tcp, addr)) => {
                    set_tcp_keepalive(&tcp);
                    return (tcp, addr);
                }
                Err(e) => accept_failed(&e, self.listener).await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

pub(crate) fn set_tcp_keepalive(tcp: &TcpStream) {
    let keepalive = socket2::TcpKeepalive::new().with_time(TCP_KEEPALIVE);
    if let Err(e) = socket2::SockRef::from(tcp).set_tcp_keepalive(&keepalive) {
        tracing::debug!(error = %e, "could not enable TCP keepalive on an accepted socket");
    }
}

pub(crate) async fn accept_failed(e: &io::Error, listener: &'static str) {
    if fd_exhausted(e) {
        tracing::warn!(
            error = %e,
            listener,
            "accept failed: out of file descriptors; backing off before retrying",
        );
        tokio::time::sleep(FD_EXHAUSTED_BACKOFF).await;
    } else {
        tracing::debug!(error = %e, listener, "TCP accept failed");
        tokio::time::sleep(ACCEPT_RETRY).await;
    }
}

#[cfg(unix)]
fn fd_exhausted(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EMFILE | libc::ENFILE))
}

#[cfg(not(unix))]
fn fd_exhausted(_: &io::Error) -> bool {
    false
}

/// Wrap a stream of accepted sockets (tonic's `TcpIncoming`) so that an
/// accept failing for want of file descriptors is logged at WARN and the
/// next accept waits [`FD_EXHAUSTED_BACKOFF`], instead of tonic retrying
/// in a tight loop. Every item passes through unchanged.
pub fn accept_backoff<S>(incoming: S, listener: &'static str) -> AcceptBackoff<S> {
    AcceptBackoff {
        incoming,
        listener,
        pause: None,
    }
}

/// The stream [`accept_backoff`] returns.
pub struct AcceptBackoff<S> {
    incoming: S,
    listener: &'static str,
    pause: Option<Pin<Box<Sleep>>>,
}

impl<S, T> Stream for AcceptBackoff<S>
where
    S: Stream<Item = io::Result<T>> + Unpin,
{
    type Item = io::Result<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(pause) = &mut this.pause {
            ready!(pause.as_mut().poll(cx));
            this.pause = None;
        }
        let next = ready!(Pin::new(&mut this.incoming).poll_next(cx));
        if let Some(Err(e)) = &next
            && fd_exhausted(e)
        {
            tracing::warn!(
                error = %e,
                listener = this.listener,
                "accept failed: out of file descriptors; backing off before retrying",
            );
            this.pause = Some(Box::pin(tokio::time::sleep(FD_EXHAUSTED_BACKOFF)));
        }
        Poll::Ready(next)
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use axum::serve::Listener as _;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use rustls_pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::client::TlsStream;
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    use super::{Deadlines, PlainListener, fd_exhausted, serve_with};
    use crate::tls_serve::{LISTENER_HTTP, ReloadingAcceptor, TlsListener};

    const SHORT: Duration = Duration::from_millis(300);
    const LONG: Duration = Duration::from_secs(30);
    const SLOW_HANDLER: Duration = Duration::from_millis(900);
    const PATIENCE: Duration = Duration::from_secs(5);
    const PARTIAL_HEAD: &[u8] = b"GET / HTTP/1.1\r\nhost: localhost\r\n";

    const HEADER_ONLY: Deadlines = Deadlines {
        header_read: SHORT,
        idle: LONG,
    };
    const IDLE_ONLY: Deadlines = Deadlines {
        header_read: LONG,
        idle: SHORT,
    };
    const BOTH: Deadlines = Deadlines {
        header_read: SHORT,
        idle: SHORT,
    };

    fn app() -> Router {
        Router::new().route("/", get(|| async { "ok" })).route(
            "/slow",
            get(|| async {
                tokio::time::sleep(SLOW_HANDLER).await;
                "slow"
            }),
        )
    }

    async fn bind() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        (listener, addr)
    }

    async fn serve_plain(deadlines: Deadlines) -> SocketAddr {
        let (listener, addr) = bind().await;
        tokio::spawn(serve_with(
            PlainListener::new(listener, LISTENER_HTTP),
            app(),
            std::future::pending(),
            deadlines,
        ));
        addr
    }

    /// A fixed acceptor for a cert minted now, and a connector trusting it.
    fn tls_pair() -> (ReloadingAcceptor, TlsConnector) {
        let minted =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("mint cert");
        let cert: CertificateDer<'static> = minted.cert.der().clone();
        let key = PrivatePkcs8KeyDer::from(minted.signing_key.serialize_der());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut server = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key.into())
            .expect("server config");
        server.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).expect("trust the minted cert");
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        (
            ReloadingAcceptor::fixed(TlsAcceptor::from(Arc::new(server))),
            TlsConnector::from(Arc::new(client)),
        )
    }

    async fn serve_tls(deadlines: Deadlines) -> (SocketAddr, TlsConnector) {
        let (listener, addr) = bind().await;
        let (acceptor, connector) = tls_pair();
        tokio::spawn(serve_with(
            TlsListener::new(listener, acceptor, LISTENER_HTTP),
            app(),
            std::future::pending(),
            deadlines,
        ));
        (addr, connector)
    }

    async fn tls_connect(addr: SocketAddr, connector: &TlsConnector) -> TlsStream<TcpStream> {
        let tcp = TcpStream::connect(addr).await.expect("connect");
        let name = ServerName::try_from("localhost").expect("server name");
        connector.connect(name, tcp).await.expect("TLS handshake")
    }

    /// Send `GET path` and read the whole 200 response, ending in `body`.
    async fn get_ok<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, path: &str, body: &str) {
        let request = format!("GET {path} HTTP/1.1\r\nhost: localhost\r\n\r\n");
        stream.write_all(request.as_bytes()).await.expect("write");
        let mut response = Vec::new();
        let mut chunk = [0u8; 512];
        tokio::time::timeout(PATIENCE, async {
            while !response.ends_with(body.as_bytes()) {
                let n = stream.read(&mut chunk).await.expect("read");
                assert_ne!(n, 0, "the server closed before the response ended");
                response.extend_from_slice(&chunk[..n]);
            }
        })
        .await
        .expect("the response arrives in time");
        assert!(
            response.starts_with(b"HTTP/1.1 200"),
            "{}",
            String::from_utf8_lossy(&response)
        );
    }

    /// How long until the server closes `stream`; fails if it is still
    /// open after `PATIENCE`.
    async fn time_to_close<S: AsyncRead + Unpin>(stream: &mut S) -> Duration {
        let start = Instant::now();
        let mut chunk = [0u8; 64];
        let read = tokio::time::timeout(PATIENCE, stream.read(&mut chunk))
            .await
            .expect("the server closes the connection in time");
        match read {
            Ok(0) | Err(_) => start.elapsed(),
            Ok(n) => panic!("the server sent {n} unexpected bytes"),
        }
    }

    #[tokio::test]
    async fn plaintext_slowloris_is_dropped_after_the_header_read_timeout() {
        let addr = serve_plain(HEADER_ONLY).await;
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream.write_all(PARTIAL_HEAD).await.expect("write");
        assert!(time_to_close(&mut stream).await >= SHORT / 2);
    }

    #[tokio::test]
    async fn plaintext_stalled_second_request_is_dropped_after_the_header_read_timeout() {
        let addr = serve_plain(HEADER_ONLY).await;
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        get_ok(&mut stream, "/", "ok").await;
        stream.write_all(PARTIAL_HEAD).await.expect("write");
        assert!(time_to_close(&mut stream).await >= SHORT / 2);
    }

    #[tokio::test]
    async fn plaintext_idle_keep_alive_is_closed_after_the_idle_timeout() {
        let addr = serve_plain(IDLE_ONLY).await;
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        get_ok(&mut stream, "/", "ok").await;
        assert!(time_to_close(&mut stream).await >= SHORT / 2);
    }

    #[tokio::test]
    async fn tls_slowloris_is_dropped_after_the_header_read_timeout() {
        let (addr, connector) = serve_tls(HEADER_ONLY).await;
        let mut stream = tls_connect(addr, &connector).await;
        stream.write_all(PARTIAL_HEAD).await.expect("write");
        stream.flush().await.expect("flush");
        assert!(time_to_close(&mut stream).await >= SHORT / 2);
    }

    #[tokio::test]
    async fn tls_idle_keep_alive_is_closed_after_the_idle_timeout() {
        let (addr, connector) = serve_tls(IDLE_ONLY).await;
        let mut stream = tls_connect(addr, &connector).await;
        get_ok(&mut stream, "/", "ok").await;
        assert!(time_to_close(&mut stream).await >= SHORT / 2);
    }

    /// Neither deadline cuts a request whose head has arrived, however
    /// long the handler takes, and the connection stays usable after it.
    #[tokio::test]
    async fn a_slow_request_outlives_both_deadlines_and_keeps_its_connection() {
        let addr = serve_plain(BOTH).await;
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        get_ok(&mut stream, "/slow", "slow").await;
        get_ok(&mut stream, "/", "ok").await;
    }

    /// An h2c connection is left to the HTTP/2 PING keepalive: the HTTP/1
    /// deadlines do not close it between requests.
    #[tokio::test]
    async fn http2_connections_are_not_subject_to_the_http1_deadlines() {
        let addr = serve_plain(BOTH).await;
        let tcp = TcpStream::connect(addr).await.expect("connect");
        let (mut sender, conn) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
                .await
                .expect("h2 handshake");
        tokio::spawn(conn);
        for _ in 0..2 {
            let request = Request::get("http://localhost/")
                .body(Body::empty())
                .expect("request");
            let response = sender.send_request(request).await.expect("h2 request");
            assert_eq!(response.status(), StatusCode::OK);
            tokio::time::sleep(SHORT * 3).await;
        }
    }

    /// Shutdown stops the listener, lets the in-flight request finish,
    /// closes the connection, and only then returns.
    #[tokio::test]
    async fn shutdown_drains_in_flight_requests_then_returns() {
        let (listener, addr) = bind().await;
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_with(
            PlainListener::new(listener, LISTENER_HTTP),
            app(),
            async move {
                let _ = stopped.await;
            },
            super::DEADLINES,
        ));
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let client = tokio::spawn(async move {
            get_ok(&mut stream, "/slow", "slow").await;
            time_to_close(&mut stream).await
        });
        tokio::time::sleep(SLOW_HANDLER / 3).await;
        stop.send(()).expect("signal shutdown");
        client.await.expect("the in-flight request completes");
        tokio::time::timeout(PATIENCE, server)
            .await
            .expect("serve returns once drained")
            .expect("serve task");
        assert!(
            TcpStream::connect(addr).await.is_err(),
            "the listener is closed"
        );
    }

    #[tokio::test]
    async fn accepted_sockets_have_tcp_keepalive() {
        let (listener, addr) = bind().await;
        let mut plain = PlainListener::new(listener, LISTENER_HTTP);
        let _client = TcpStream::connect(addr).await.expect("connect");
        let (tcp, _) = plain.accept().await;
        assert!(
            socket2::SockRef::from(&tcp)
                .keepalive()
                .expect("SO_KEEPALIVE")
        );

        let (listener, addr) = bind().await;
        let (acceptor, connector) = tls_pair();
        let mut tls = TlsListener::new(listener, acceptor, LISTENER_HTTP);
        let client = tokio::spawn(async move { tls_connect(addr, &connector).await });
        let (stream, _) = tls.accept().await;
        let _client = client.await.expect("client");
        assert!(
            socket2::SockRef::from(stream.get_ref().0)
                .keepalive()
                .expect("SO_KEEPALIVE")
        );
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_exhaustion_is_told_apart_from_other_accept_errors() {
        assert!(fd_exhausted(&std::io::Error::from_raw_os_error(
            libc::EMFILE
        )));
        assert!(fd_exhausted(&std::io::Error::from_raw_os_error(
            libc::ENFILE
        )));
        assert!(!fd_exhausted(&std::io::Error::from(
            std::io::ErrorKind::ConnectionAborted
        )));
    }
}
