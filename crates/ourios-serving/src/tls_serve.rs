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
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use futures_core::Stream;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use ourios_semconv as semconv;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use crate::serve::{NamedListener, accept_failed, set_tcp_keepalive};
use crate::tls::TlsSettings;
use crate::tls_reload;
#[cfg(feature = "fault-injection")]
pub use crate::tls_reload::panic_next_reload;

/// A [`TlsAcceptor`] whose backing config can be hot-swapped without
/// dropping the listener (RFC0030.6). Cheap to clone — clones share the
/// lock; the serve adapters read the live acceptor per handshake, so a
/// swap is visible to the next connection while in-flight ones keep
/// their session.
#[derive(Clone)]
pub struct ReloadingAcceptor {
    current: Arc<RwLock<TlsAcceptor>>,
    /// The listener it terminates TLS for, which its handshake failures
    /// are counted under.
    listener: &'static str,
}

impl ReloadingAcceptor {
    /// A non-reloading acceptor — a fixed config wrapped so the serve
    /// adapters take one type whether or not reload is configured.
    #[must_use]
    pub fn fixed(acceptor: TlsAcceptor) -> Self {
        Self {
            current: Arc::new(RwLock::new(acceptor)),
            listener: LISTENER_GRPC,
        }
    }

    /// The `ourios.server.listener.name` it serves.
    #[must_use]
    pub fn listener(&self) -> &'static str {
        self.listener
    }

    fn current(&self) -> TlsAcceptor {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Build a [`ReloadingAcceptor`] for `settings` advertising `alpn`. When
/// `settings.reload_interval` is set, spawn a task that re-reads the
/// cert/key(/CA) files on the interval and swaps the acceptor on a
/// content change; an unreadable or invalid reload logs, counts
/// `ourios.receiver.tls.reload_failures`, and keeps the last good config
/// — it never takes the listener down. The re-read + rebuild run in
/// `spawn_blocking` so the sync filesystem I/O never stalls a runtime
/// worker. The task self-terminates when the returned acceptor (and all
/// clones) is dropped, via a `Weak` upgrade check.
///
/// # Errors
///
/// Whatever [`TlsSettings::acceptor`] returns for the initial build
/// (unusable PEM, naming the path) — startup fails fast, as at config
/// preflight.
pub fn reloading_acceptor(
    settings: &TlsSettings,
    alpn: &[&[u8]],
    listener: &'static str,
) -> Result<ReloadingAcceptor, String> {
    let owned = alpn.iter().map(|p| p.to_vec()).collect();
    let current = tls_reload::follow(settings, (owned, listener), |settings| {
        settings.acceptor(alpn)
    })?;
    Ok(ReloadingAcceptor { current, listener })
}

/// Per-connection handshake deadline. A handshake that has not completed
/// within this bound is abandoned (RFC 0030 §3.2 — a stalled peer must
/// not wedge the listener). Generous enough for a real TLS 1.2/1.3
/// exchange over a slow link, short enough to bound a slowloris.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on in-flight handshakes per listener. With the deadline above, a
/// flood of connect-and-stall clients would otherwise accumulate
/// unbounded handshake tasks (a resource-exhaustion `DoS`); at the cap,
/// new connections wait in the OS accept backlog until a slot frees —
/// backpressure, not unbounded growth.
pub const MAX_CONCURRENT_HANDSHAKES: usize = 256;

/// `ourios.server.listener.name` values.
pub const LISTENER_GRPC: &str = "grpc";
pub const LISTENER_HTTP: &str = "http";
/// The querier HTTP listener (distinct from the receiver HTTP listener).
pub const LISTENER_QUERIER: &str = "querier";
/// A replicated node's peer listener (RFC 0065 §3.8).
pub const LISTENER_REPLICATION: &str = "replication";
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
        let counter = ourios_core::telemetry::meter("ourios.receiver")
            .u64_counter(semconv::OURIOS_RECEIVER_TLS_HANDSHAKE_FAILURES)
            .with_description(semconv::OURIOS_RECEIVER_TLS_HANDSHAKE_FAILURES_DESCRIPTION)
            .with_unit(semconv::OURIOS_RECEIVER_TLS_HANDSHAKE_FAILURES_UNIT)
            .build();
        Self { counter, listener }
    }

    fn record(&self, failure: &'static str) {
        self.counter.add(
            1,
            &[
                KeyValue::new(semconv::OURIOS_SERVER_LISTENER_NAME, self.listener),
                KeyValue::new(semconv::OURIOS_TLS_FAILURE, failure),
            ],
        );
    }
}

/// Handshake one accepted `TcpStream` under the deadline, recording the
/// cause of any failure. Returns `None` when the connection is dropped
/// (already counted + logged with the peer address for diagnosis).
async fn handshake(
    acceptor: &TlsAcceptor,
    tcp: TcpStream,
    metrics: &HandshakeFailures,
) -> Option<TlsStream<TcpStream>> {
    // Capture the peer before the stream is consumed by the handshake —
    // it's the only thread through which an intermittent client failure
    // (wrong CA, protocol mismatch, slowloris) can be diagnosed.
    let peer = tcp.peer_addr().ok();
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
        Ok(Ok(tls)) => Some(tls),
        Ok(Err(e)) => {
            tracing::debug!(error = %e, listener = metrics.listener, ?peer, "TLS handshake failed");
            metrics.record(FAILURE_HANDSHAKE);
            None
        }
        Err(_) => {
            tracing::debug!(
                listener = metrics.listener,
                ?peer,
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
    acceptor: ReloadingAcceptor,
) -> impl Stream<Item = io::Result<TlsStream<TcpStream>>>
where
    S: Stream<Item = io::Result<TcpStream>> + Send + 'static,
{
    let metrics = HandshakeFailures::new(acceptor.listener());
    // Successful handshakes flow through this channel; the bound caps
    // in-flight completed-but-unconsumed connections.
    let (tx, rx) = tokio::sync::mpsc::channel(1024);
    // Bound concurrent handshake tasks: the driver waits for a free slot
    // before spawning, so a stall flood queues in the accept backlog
    // rather than accumulating tasks.
    let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_HANDSHAKES));
    tokio::spawn(async move {
        use tokio_stream::StreamExt as _;
        tokio::pin!(incoming);
        loop {
            // Stop as soon as the consumer (tonic's server) drops the
            // stream — otherwise this detached task would keep accepting
            // and handshaking forever, leaking the listener past
            // shutdown. `closed()` tracks the receiver, not the cloned
            // senders held by in-flight handshakes.
            let conn = tokio::select! {
                () = tx.closed() => break,
                conn = incoming.next() => conn,
            };
            let Some(conn) = conn else { break };
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
            // Backpressure: block accepting until a handshake slot frees.
            // `acquire_owned` errors only if the semaphore is closed,
            // which never happens (it lives as long as this task).
            let Ok(permit) = slots.clone().acquire_owned().await else {
                break;
            };
            // Read the live acceptor per connection so a reload is
            // visible to the next handshake.
            let acceptor = acceptor.current();
            let metrics = metrics.clone();
            let tx = tx.clone();
            // One task per connection: the handshake can't block the
            // accept loop or any sibling. The permit is held for the
            // handshake's lifetime and released on completion.
            tokio::spawn(async move {
                let _permit = permit;
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
    acceptor: ReloadingAcceptor,
    metrics: HandshakeFailures,
    handshakes: JoinSet<Option<(TlsStream<TcpStream>, SocketAddr)>>,
}

impl TlsListener {
    /// Wrap a bound `TcpListener` with `acceptor`. `listener` is the
    /// `ourios.server.listener.name` label for this listener's handshake-failure
    /// metric (e.g. `LISTENER_HTTP` for the receiver, `LISTENER_QUERIER`
    /// for the querier) — so the two HTTP listeners don't aggregate.
    #[must_use]
    pub fn new(inner: TcpListener, acceptor: ReloadingAcceptor, listener: &'static str) -> Self {
        Self {
            inner,
            acceptor,
            metrics: HandshakeFailures::new(listener),
            handshakes: JoinSet::new(),
        }
    }
}

impl NamedListener for TlsListener {
    fn name(&self) -> &'static str {
        self.metrics.listener
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
                // Gated by the in-flight cap: at capacity the arm is
                // disabled, so `select!` only drains completions until a
                // slot frees, and excess connections wait in the accept
                // backlog (backpressure, not unbounded task growth).
                accepted = self.inner.accept(),
                    if self.handshakes.len() < MAX_CONCURRENT_HANDSHAKES => {
                    match accepted {
                        Ok((tcp, addr)) => {
                            set_tcp_keepalive(&tcp);
                            // Live acceptor per connection (reload-visible).
                            let acceptor = self.acceptor.current();
                            let metrics = self.metrics.clone();
                            self.handshakes.spawn(async move {
                                handshake(&acceptor, tcp, &metrics).await.map(|tls| (tls, addr))
                            });
                        }
                        Err(e) => accept_failed(&e, self.metrics.listener).await,
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

#[cfg(test)]
mod tests {
    use super::{LISTENER_GRPC, LISTENER_REPLICATION, ReloadingAcceptor, reloading_acceptor};
    use crate::tls::{ALPN_GRPC, TlsMinVersion, TlsSettings};

    /// Settings for a certificate minted now, under `dir`.
    fn settings(dir: &std::path::Path) -> TlsSettings {
        let minted =
            rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("mint cert");
        let cert_file = dir.join("listener.crt");
        let key_file = dir.join("listener.key");
        std::fs::write(&cert_file, minted.cert.pem()).expect("cert");
        std::fs::write(&key_file, minted.signing_key.serialize_pem()).expect("key");
        TlsSettings {
            cert_file,
            key_file,
            client_ca_file: None,
            min_version: TlsMinVersion::V1_2,
            reload_interval: None,
        }
    }

    /// An acceptor keeps the listener it was made for, which its handshake
    /// failures are counted under: a replicated node's peer listener is not
    /// the OTLP gRPC listener.
    #[test]
    fn an_acceptor_names_the_listener_it_serves() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers = reloading_acceptor(&settings(dir.path()), ALPN_GRPC, LISTENER_REPLICATION)
            .expect("acceptor");
        assert_eq!(peers.listener(), LISTENER_REPLICATION);

        let tls = settings(dir.path()).acceptor(ALPN_GRPC).expect("acceptor");
        assert_eq!(ReloadingAcceptor::fixed(tls).listener(), LISTENER_GRPC);
    }
}
