//! The HTTP serve loop shared by the OTLP/HTTP and querier listeners,
//! plaintext and TLS alike, and the transport deadlines of all listeners.
//!
//! `axum::serve` builds its hyper connection with no timer and exposes no
//! settings, so hyper's header-read timeout is off and a peer that stalls
//! or goes quiet holds its socket for as long as it likes. [`serve_http`]
//! keeps `axum::serve`'s accept → spawn → graceful-drain shape, serves
//! HTTP/1 only, and sets [`HEADER_READ_TIMEOUT`]. hyper runs that clock
//! whenever a connection is waiting for a request head, including while an
//! idle keep-alive connection waits for its next request, so it also
//! closes idle keep-alive connections.
//!
//! Every listener sets [`TCP_KEEPALIVE`] on the sockets it accepts
//! ([`PlainListener`], [`TlsListener`](crate::tls_serve::TlsListener), and
//! the gRPC `TcpIncoming`); the gRPC server also sends HTTP/2 keepalive
//! PINGs ([`GRPC_KEEPALIVE_INTERVAL`] / [`GRPC_KEEPALIVE_TIMEOUT`]).
//!
//! None of these bounds a request once its head has arrived.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::serve::Listener;
use futures_core::Stream;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::Sleep;
use tower_service::Service as _;

/// How long an HTTP/1 connection may wait for a complete request head,
/// its first one or the next one on a keep-alive connection. hyper's own
/// default, which stays off unless a timer is set.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Interval between the gRPC server's HTTP/2 keepalive PINGs.
pub const GRPC_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(60);

/// How long a gRPC keepalive PING may go unanswered before the connection
/// is closed.
pub const GRPC_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Idle time before the kernel starts TCP keepalive probes on an accepted
/// socket.
pub const TCP_KEEPALIVE: Duration = Duration::from_secs(60);

/// Pause after an accept fails for want of file descriptors. Retrying at
/// once would spin: the condition clears only as other connections close.
const FD_EXHAUSTED_BACKOFF: Duration = Duration::from_millis(500);

/// Pause after any other accept error.
const ACCEPT_RETRY: Duration = Duration::from_millis(1);

/// Serve `router` over HTTP/1 on `listener` until `shutdown` resolves,
/// then stop accepting, let every open connection finish its in-flight
/// requests, and return once all of them have closed.
///
/// `listener` is a [`PlainListener`] or a
/// [`TlsListener`](crate::tls_serve::TlsListener); both set
/// [`TCP_KEEPALIVE`] on the sockets they accept.
pub async fn serve_http<L>(listener: L, router: Router, shutdown: impl Future<Output = ()>)
where
    L: Listener,
{
    serve_with(listener, router, shutdown, HEADER_READ_TIMEOUT).await;
}

async fn serve_with<L>(
    mut listener: L,
    router: Router,
    shutdown: impl Future<Output = ()>,
    header_read: Duration,
) where
    L: Listener,
{
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(header_read);

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
    builder: http1::Builder,
    mut stop: watch::Receiver<bool>,
    _open: watch::Receiver<()>,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service =
        service_fn(move |request: Request<Incoming>| router.clone().call(request.map(Body::new)));
    let mut conn = pin!(
        builder
            .serve_connection(TokioIo::new(io), service)
            .with_upgrades()
    );
    let served = tokio::select! {
        served = conn.as_mut() => Some(served),
        _ = stop.wait_for(|stop| *stop) => None,
    };
    let served = if let Some(served) = served {
        served
    } else {
        conn.as_mut().graceful_shutdown();
        conn.await
    };
    if let Err(e) = served {
        tracing::debug!(error = %e, "HTTP connection ended with an error");
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
/// next accept waits 500 ms, instead of tonic retrying in a tight loop.
/// Every item passes through unchanged.
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
    use axum::http::Request;
    use axum::routing::get;
    use axum::serve::Listener as _;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use rustls_pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::client::TlsStream;
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    use futures_core::Stream;

    use super::{
        FD_EXHAUSTED_BACKOFF, HEADER_READ_TIMEOUT, PlainListener, accept_backoff, fd_exhausted,
        serve_with,
    };
    use crate::tls_serve::{LISTENER_GRPC, LISTENER_HTTP, ReloadingAcceptor, TlsListener};

    /// The header-read timeout the tests serve with, in place of 30 s.
    const SHORT: Duration = Duration::from_millis(300);
    const SLOW_HANDLER: Duration = Duration::from_millis(900);
    const PATIENCE: Duration = Duration::from_secs(5);
    const PARTIAL_HEAD: &[u8] = b"GET / HTTP/1.1\r\nhost: localhost\r\n";

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

    async fn serve_plain() -> SocketAddr {
        let (listener, addr) = bind().await;
        tokio::spawn(serve_with(
            PlainListener::new(listener, LISTENER_HTTP),
            app(),
            std::future::pending(),
            SHORT,
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
        let addr = serve_plain().await;
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream.write_all(PARTIAL_HEAD).await.expect("write");
        assert!(time_to_close(&mut stream).await >= SHORT / 2);
    }

    #[tokio::test]
    async fn plaintext_stalled_second_request_is_dropped_after_the_header_read_timeout() {
        let addr = serve_plain().await;
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        get_ok(&mut stream, "/", "ok").await;
        stream.write_all(PARTIAL_HEAD).await.expect("write");
        assert!(time_to_close(&mut stream).await >= SHORT / 2);
    }

    #[tokio::test]
    async fn tls_slowloris_is_dropped_after_the_header_read_timeout() {
        let (listener, addr) = bind().await;
        let (acceptor, connector) = tls_pair();
        tokio::spawn(serve_with(
            TlsListener::new(listener, acceptor, LISTENER_HTTP),
            app(),
            std::future::pending(),
            SHORT,
        ));
        let mut stream = tls_connect(addr, &connector).await;
        stream.write_all(PARTIAL_HEAD).await.expect("write");
        stream.flush().await.expect("flush");
        assert!(time_to_close(&mut stream).await >= SHORT / 2);
    }

    /// hyper's header-read clock also runs while a keep-alive connection
    /// waits for its next request, so an idle connection closes on it.
    #[tokio::test]
    async fn idle_keep_alive_is_closed_after_the_header_read_timeout() {
        let addr = serve_plain().await;
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        get_ok(&mut stream, "/", "ok").await;
        assert!(time_to_close(&mut stream).await >= SHORT / 2);
    }

    /// The header-read timeout does not cut a request whose head has
    /// arrived, however long the handler takes.
    #[tokio::test]
    async fn a_slow_request_outlives_the_header_read_timeout_and_keeps_its_connection() {
        let addr = serve_plain().await;
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        get_ok(&mut stream, "/slow", "slow").await;
        get_ok(&mut stream, "/", "ok").await;
    }

    /// The HTTP listeners speak HTTP/1 only: a cleartext HTTP/2
    /// prior-knowledge client gets no response.
    #[tokio::test]
    async fn h2c_prior_knowledge_is_not_served() {
        let addr = serve_plain().await;
        let tcp = TcpStream::connect(addr).await.expect("connect");
        let exchange = async {
            let (mut sender, conn) =
                hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tcp))
                    .await?;
            tokio::spawn(conn);
            let request = Request::get("http://localhost/")
                .body(Body::empty())
                .expect("request");
            sender.send_request(request).await
        };
        let exchange = tokio::time::timeout(PATIENCE, exchange)
            .await
            .expect("the server answers or closes in time");
        assert!(
            exchange.is_err(),
            "h2c was served: {:?}",
            exchange.map(|r| r.status())
        );
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
            HEADER_READ_TIMEOUT,
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

    /// Yields a fixed script of accept results, then ends.
    struct Scripted(VecDeque<std::io::Result<u8>>);

    impl Stream for Scripted {
        type Item = std::io::Result<u8>;

        fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.0.pop_front())
        }
    }

    fn poll_once<S: Stream + Unpin>(stream: &mut S) -> Poll<Option<S::Item>> {
        Pin::new(stream).poll_next(&mut Context::from_waker(Waker::noop()))
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn accept_backoff_pauses_after_descriptor_exhaustion_only() {
        let mut incoming = accept_backoff(
            Scripted(VecDeque::from([
                Err(std::io::Error::from_raw_os_error(libc::EMFILE)),
                Ok(1),
            ])),
            LISTENER_GRPC,
        );
        match poll_once(&mut incoming) {
            Poll::Ready(Some(Err(e))) => assert_eq!(e.raw_os_error(), Some(libc::EMFILE)),
            other => panic!("expected the EMFILE error to be forwarded, got {other:?}"),
        }
        assert!(poll_once(&mut incoming).is_pending());
        tokio::time::advance(FD_EXHAUSTED_BACKOFF / 2).await;
        assert!(poll_once(&mut incoming).is_pending());
        tokio::time::advance(FD_EXHAUSTED_BACKOFF / 2).await;
        assert!(matches!(poll_once(&mut incoming), Poll::Ready(Some(Ok(1)))));

        let mut incoming = accept_backoff(
            Scripted(VecDeque::from([
                Err(std::io::Error::from(std::io::ErrorKind::ConnectionAborted)),
                Ok(2),
            ])),
            LISTENER_GRPC,
        );
        assert!(matches!(
            poll_once(&mut incoming),
            Poll::Ready(Some(Err(_)))
        ));
        assert!(matches!(poll_once(&mut incoming), Poll::Ready(Some(Ok(2)))));
    }
}
