//! RFC 0030 §3.2 — TLS-wrapping adapters for the two serve loops.
//!
//! One [`tokio_rustls::TlsAcceptor`] (built by
//! [`TlsSettings::acceptor`](super::tls::TlsSettings::acceptor)) sits in
//! front of each listener; these adapters feed the handshaked streams to
//! their framework:
//!
//! - [`tls_incoming`] turns a stream of accepted `TcpStream`s (tonic's
//!   `TcpIncoming`) into a stream of `TlsStream`s for
//!   `tonic`'s `serve_with_incoming` — the gRPC listener.
//! - [`TlsListener`] implements `axum::serve::Listener` over a
//!   `TcpListener` + acceptor — the HTTP surfaces.
//!
//! Both **absorb per-connection handshake failures** (log and move to
//! the next connection) rather than propagate them: a client that
//! fails the handshake — no client cert under mTLS, a stale
//! certificate, a version mismatch — must not take the listener down.

use std::io;
use std::net::SocketAddr;

use futures_core::Stream;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tokio_stream::StreamExt as _;

/// Wrap a stream of accepted `TcpStream`s (e.g. tonic's `TcpIncoming`)
/// in the TLS handshake, yielding `TlsStream`s ready for
/// `serve_with_incoming`. A connection whose handshake fails is logged
/// and dropped — never yielded — so one bad client can't stall or kill
/// the gRPC listener.
pub fn tls_incoming<S>(
    incoming: S,
    acceptor: TlsAcceptor,
) -> impl Stream<Item = io::Result<TlsStream<TcpStream>>>
where
    S: Stream<Item = io::Result<TcpStream>>,
{
    async_stream::stream! {
        tokio::pin!(incoming);
        while let Some(conn) = incoming.next().await {
            let tcp = match conn {
                Ok(tcp) => tcp,
                // A TCP-accept error (fd exhaustion, etc.) is the
                // listener's, not a connection's — surface it.
                Err(e) => { yield Err(e); continue; }
            };
            match acceptor.accept(tcp).await {
                Ok(tls) => yield Ok(tls),
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        "TLS handshake failed on the gRPC listener; dropping the connection",
                    );
                }
            }
        }
    }
}

/// An `axum::serve::Listener` that terminates TLS: each `accept` does a
/// TCP accept followed by the TLS handshake, retrying past both TCP and
/// handshake errors (the trait requires `accept` to handle its own
/// logging + retry and never fail).
pub struct TlsListener {
    inner: TcpListener,
    acceptor: TlsAcceptor,
}

impl TlsListener {
    /// Wrap a bound `TcpListener` with `acceptor`.
    #[must_use]
    pub fn new(inner: TcpListener, acceptor: TlsAcceptor) -> Self {
        Self { inner, acceptor }
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (tcp, addr) = match self.inner.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    // Mirror axum's own TcpListener retry: a transient
                    // accept error backs off briefly rather than spinning.
                    tracing::debug!(error = %e, "TCP accept failed on the TLS HTTP listener");
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    continue;
                }
            };
            match self.acceptor.accept(tcp).await {
                Ok(tls) => return (tls, addr),
                Err(e) => {
                    tracing::debug!(
                        error = %e, %addr,
                        "TLS handshake failed on the HTTP listener; dropping the connection",
                    );
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}
