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
//! Both **run each handshake concurrently and under a deadline**: a
//! client that connects but never finishes (or never starts) its
//! `ClientHello` — slowloris, a stalled peer, a wrong-CA client under
//! mTLS — is dropped after [`HANDSHAKE_TIMEOUT`] and, crucially, does
//! **not** hold up accepting or handshaking any other connection.
//! Every drop increments `ourios.receiver.tls.handshake_failures`
//! (keyed by listener + cause) — a dropped connection never reaches the
//! auth layer or the WAL, so the counter is the only signal it happened.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use futures_core::Stream;
use opentelemetry::metrics::Counter;
use opentelemetry::{KeyValue, global};
use ourios_semconv as semconv;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

/// Per-connection handshake deadline. A handshake that has not completed
/// within this bound is abandoned (RFC 0030 §3.2 — a stalled peer must
/// not wedge the listener). Generous enough for a real TLS 1.2/1.3
/// exchange over a slow link, short enough to bound a slowloris.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// `ourios.tls.listener` values.
const LISTENER_GRPC: &str = "grpc";
const LISTENER_HTTP: &str = "http";
/// `ourios.tls.failure` values.
const FAILURE_HANDSHAKE: &str = "handshake";
const FAILURE_TIMEOUT: &str = "timeout";

/// The `ourios.receiver.tls.handshake_failures` counter on the
/// `ourios.receiver` meter, built once per listener. Instruments
/// resolve through the global meter (a no-op until a provider is
/// installed), so constructing this is always cheap.
#[derive(Clone)]
struct HandshakeFailures {
    counter: Counter<u64>,
    listener: &'static str,
}

impl HandshakeFailures {
    fn new(listener: &'static str) -> Self {
        let counter = global::meter("ourios.receiver")
            .u64_counter(semconv::OURIOS_RECEIVER_TLS_HANDSHAKE_FAILURES)
            .build();
        Self { counter, listener }
    }

    fn record(&self, failure: &'static str) {
        self.counter.add(
            1,
            &[
                KeyValue::new(semconv::OURIOS_TLS_LISTENER, self.listener),
                KeyValue::new(semconv::OURIOS_TLS_FAILURE, failure),
            ],
        );
    }
}

/// Handshake one accepted `TcpStream` under the deadline, recording the
/// cause of any failure. `Ok(None)` means "dropped, already counted".
async fn handshake(
    acceptor: &TlsAcceptor,
    tcp: TcpStream,
    metrics: &HandshakeFailures,
) -> Option<TlsStream<TcpStream>> {
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
        Ok(Ok(tls)) => Some(tls),
        Ok(Err(e)) => {
            tracing::debug!(error = %e, listener = metrics.listener, "TLS handshake failed");
            metrics.record(FAILURE_HANDSHAKE);
            None
        }
        Err(_) => {
            tracing::debug!(
                listener = metrics.listener,
                "TLS handshake timed out; dropping the connection",
            );
            metrics.record(FAILURE_TIMEOUT);
            None
        }
    }
}

/// Wrap a stream of accepted `TcpStream`s (e.g. tonic's `TcpIncoming`)
/// in the TLS handshake, yielding `TlsStream`s ready for
/// `serve_with_incoming`. Each handshake runs as its own task under
/// [`HANDSHAKE_TIMEOUT`], so a stalled client neither blocks new accepts
/// nor other handshakes; failures are counted and dropped, never
/// yielded.
pub fn tls_incoming<S>(
    incoming: S,
    acceptor: TlsAcceptor,
) -> impl Stream<Item = io::Result<TlsStream<TcpStream>>>
where
    S: Stream<Item = io::Result<TcpStream>> + Send + 'static,
{
    let metrics = HandshakeFailures::new(LISTENER_GRPC);
    // Successful handshakes flow through this channel; the bound caps
    // in-flight completed-but-unconsumed connections.
    let (tx, rx) = tokio::sync::mpsc::channel(1024);
    tokio::spawn(async move {
        use tokio_stream::StreamExt as _;
        tokio::pin!(incoming);
        while let Some(conn) = incoming.next().await {
            let tcp = match conn {
                Ok(tcp) => tcp,
                // A TCP-accept error is the listener's, not a
                // connection's — forward it so tonic sees it.
                Err(e) => {
                    if tx.send(Err(e)).await.is_err() {
                        break;
                    }
                    continue;
                }
            };
            let acceptor = acceptor.clone();
            let metrics = metrics.clone();
            let tx = tx.clone();
            // One task per connection: the handshake can't block the
            // accept loop or any sibling.
            tokio::spawn(async move {
                if let Some(tls) = handshake(&acceptor, tcp, &metrics).await {
                    let _ = tx.send(Ok(tls)).await;
                }
            });
        }
    });
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

/// An `axum::serve::Listener` that terminates TLS. `accept` drives new
/// TCP accepts and all in-flight handshakes concurrently (a `JoinSet`),
/// returning the first connection to *complete* its handshake — so a
/// stalled or failing handshake never delays a healthy one. Failures
/// are counted and dropped; the trait requires `accept` to handle its
/// own retry and never fail.
pub struct TlsListener {
    inner: TcpListener,
    acceptor: TlsAcceptor,
    metrics: HandshakeFailures,
    handshakes: JoinSet<Option<(TlsStream<TcpStream>, SocketAddr)>>,
}

impl TlsListener {
    /// Wrap a bound `TcpListener` with `acceptor`.
    #[must_use]
    pub fn new(inner: TcpListener, acceptor: TlsAcceptor) -> Self {
        Self {
            inner,
            acceptor,
            metrics: HandshakeFailures::new(LISTENER_HTTP),
            handshakes: JoinSet::new(),
        }
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            tokio::select! {
                // A new TCP connection → spawn its handshake; don't wait
                // on it here, so a slow ClientHello can't stall the loop.
                accepted = self.inner.accept() => {
                    match accepted {
                        Ok((tcp, addr)) => {
                            let acceptor = self.acceptor.clone();
                            let metrics = self.metrics.clone();
                            self.handshakes.spawn(async move {
                                handshake(&acceptor, tcp, &metrics).await.map(|tls| (tls, addr))
                            });
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "TCP accept failed on the TLS HTTP listener");
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                    }
                }
                // A pending handshake finished — return it if it succeeded.
                // The `if` guard keeps `select!` from busy-looping on an
                // empty set (`join_next` on empty resolves immediately).
                Some(joined) = self.handshakes.join_next(), if !self.handshakes.is_empty() => {
                    if let Ok(Some(pair)) = joined {
                        return pair;
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}
