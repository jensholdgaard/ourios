//! Serving the replicated WAL (RFC 0065): a node started from the
//! receiver's configuration, answering its peers over mutual TLS, running
//! its background work, and taking OTLP exports into its groups.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use ourios_ingester::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use ourios_ingester::receiver::http::{HttpConfig, router};
use ourios_ingester::receiver::{Drain, SharedIngest, Tracked};
use ourios_ingester::replicated::node::{self, Node, NodeError, NodeStart};
use ourios_ingester::replicated::raft::host::GroupMined;
use ourios_ingester::replicated::raft::log_ids::StartError as LockError;
use ourios_ingester::template_ids::TemplateIdsError;
use ourios_parquet::store::Caller;
use ourios_server::config::wal::ReplicatedWal;
use ourios_serving::tls::ALPN_GRPC;
use ourios_serving::tls_serve::{LISTENER_REPLICATION, reloading_acceptor};
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;
use tonic::transport::server::TcpIncoming;

use super::super::receiver::{
    AUDIT_SINK_CEILING_EVENTS, ReceiverConfig, StartError, bind_listeners, build_acceptors,
    spawn_grpc, spawn_http,
};
use super::{Plan, plan};

/// A running replicated receiver: its OTLP listeners' bound addresses and
/// what it stops.
pub(crate) struct ReplicatedHandle {
    pub(crate) grpc_addr: SocketAddr,
    pub(crate) http_addr: SocketAddr,
    /// Stops the OTLP listeners, first.
    otlp: watch::Sender<()>,
    grpc: JoinHandle<Result<(), tonic::transport::Error>>,
    http: JoinHandle<()>,
    /// Stops the node's background work and its peer listener.
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    peers: JoinHandle<Result<(), tonic::transport::Error>>,
    node: Node,
    /// The node's tenant denials (RFC 0026 §3.4).
    audit: SharedParquetAuditSink,
    /// The exports still ingesting once the listeners stop, a client's
    /// that disconnected included.
    drain: Drain,
}

impl ReplicatedHandle {
    /// The node it serves.
    #[cfg(test)]
    pub(super) fn node(&self) -> &Node {
        &self.node
    }

    /// Stop taking exports, then the node's background work, its peers and
    /// its groups, then make its last audit events durable.
    ///
    /// # Errors
    ///
    /// A listener failed, the groups did not shut down cleanly, or the
    /// audit events did not all flush.
    pub(crate) async fn shutdown(self) -> Result<(), String> {
        let _ = self.otlp.send(());
        let grpc = self.grpc.await;
        let _ = self.http.await;
        // An ingest whose client went away runs on past its listener; its
        // denial must reach the audit sink before the last flush.
        self.drain.close();
        self.drain.drained().await;
        self.stop.send_replace(true);
        for task in self.tasks {
            let _ = task.await;
        }
        let peers = self.peers.await;
        let groups = self.node.host.shutdown().await;
        let flushed = flushed(self.audit).await;
        served("gRPC", grpc)?;
        served("replication", peers)?;
        groups.map_err(|e| format!("the groups did not shut down: {e}"))?;
        flushed
            .then_some(())
            .ok_or_else(|| "the last audit events did not flush".to_owned())
    }
}

/// How the `name` listener's task ended.
fn served(
    name: &str,
    joined: Result<Result<(), tonic::transport::Error>, tokio::task::JoinError>,
) -> Result<(), String> {
    joined
        .map_err(|e| format!("the {name} listener's task: {e}"))?
        .map_err(|e| format!("the {name} listener: {e}"))
}

/// Whether `audit`'s buffered events all reached the store.
async fn flushed(audit: SharedParquetAuditSink) -> bool {
    tokio::task::spawn_blocking(move || audit.flush())
        .await
        .unwrap_or(false)
}

/// Serve the replicated node `replicated` and `config` describe, its peers
/// on `peer_listener`, its template-id scan stopping once `stopping` is set.
///
/// # Errors
///
/// The node did not plan, start or bind; [`StartError::Interrupted`] only
/// when a shutdown interrupted its start, which wrote nothing.
pub(crate) async fn serve_replicated(
    replicated: &ReplicatedWal,
    config: ReceiverConfig,
    (peer_listener, stopping): (TcpListener, Arc<AtomicBool>),
) -> Result<ReplicatedHandle, StartError> {
    let Plan {
        start,
        background,
        mined,
    } = plan(replicated, &config, stopping)?;
    let (stop, stopped) = watch::channel(false);
    let (node, mut tasks) = start_draining(start, mined, (&stop, &stopped)).await?;
    let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(
        config.store.clone().with_caller(Caller::AuditSink),
        AUDIT_SINK_CEILING_EVENTS,
    ));
    let peers = match serve_peers(
        &node,
        replicated,
        (&config, &audit),
        (peer_listener, &stopped),
    ) {
        Ok(peers) => peers,
        Err(e) => {
            joined(&stop, tasks).await;
            return Err(stopped_with(node, e).await.into());
        }
    };
    tasks.extend(node.run(&background, &stopped));
    tasks.push(spawn_audit_flush(
        audit.clone(),
        config.max_buffer_age,
        stopped.clone(),
    ));
    let (ingest, drain) = Tracked::new(Arc::new(
        node.ingest()
            .with_denial_audit_sink(Box::new(audit.clone())),
    ));
    let ingest: SharedIngest = Arc::new(ingest);
    let otlp = match serve_otlp(&config, ingest).await {
        Ok(otlp) => otlp,
        Err(e) => {
            let started = Started {
                stop,
                tasks,
                peers,
                node,
                audit,
            };
            return Err(started.abandon(e).await.into());
        }
    };
    Ok(ReplicatedHandle {
        grpc_addr: otlp.grpc_addr,
        http_addr: otlp.http_addr,
        otlp: otlp.shutdown,
        grpc: otlp.grpc,
        http: otlp.http,
        stop,
        tasks,
        peers,
        node,
        audit,
        drain,
    })
}

/// Serve `node`'s peers on `listener` over mutual TLS until `stopped`.
fn serve_peers(
    node: &Node,
    replicated: &ReplicatedWal,
    (config, audit): (&ReceiverConfig, &SharedParquetAuditSink),
    (listener, stopped): (TcpListener, &watch::Receiver<bool>),
) -> Result<JoinHandle<Result<(), tonic::transport::Error>>, String> {
    let acceptor = reloading_acceptor(&replicated.tls, ALPN_GRPC, LISTENER_REPLICATION)
        .map_err(|e| format!("receiver.wal.replicated.tls: {e}"))?;
    let mut stopping = stopped.clone();
    let shutdown = async move {
        let _ = stopping.wait_for(|stop| *stop).await;
    };
    let answer = (config.auth.clone(), Box::new(audit.clone()) as _);
    Ok(node.serve(answer, (TcpIncoming::from(listener), acceptor), shutdown))
}

/// The OTLP listeners, running.
struct Otlp {
    grpc_addr: SocketAddr,
    http_addr: SocketAddr,
    shutdown: watch::Sender<()>,
    grpc: JoinHandle<Result<(), tonic::transport::Error>>,
    http: JoinHandle<()>,
}

/// Serve OTLP gRPC and HTTP as `config` says, into `ingest`.
async fn serve_otlp(config: &ReceiverConfig, ingest: SharedIngest) -> Result<Otlp, String> {
    let (grpc_tls, http_tls) = build_acceptors(config.grpc_tls.as_ref(), config.http_tls.as_ref())?;
    let (grpc_incoming, grpc_addr, http_listener, http_addr) =
        bind_listeners(config.grpc_addr, config.http_addr).await?;
    let (shutdown, closing) = watch::channel(());
    let grpc = spawn_grpc(
        grpc_incoming,
        grpc_tls,
        config.auth.clone(),
        (Arc::clone(&ingest), config.max_request_bytes),
        closing.clone(),
    );
    let http_config = HttpConfig {
        auth: config.auth.clone(),
        max_body_bytes: config.max_request_bytes,
        ..HttpConfig::default()
    };
    let http = spawn_http(
        http_listener,
        http_tls,
        router(ingest, &http_config),
        closing,
    );
    Ok(Otlp {
        grpc_addr,
        http_addr,
        shutdown,
        grpc,
        http,
    })
}

/// What a start has running once its node serves its peers.
struct Started {
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    peers: JoinHandle<Result<(), tonic::transport::Error>>,
    node: Node,
    audit: SharedParquetAuditSink,
}

impl Started {
    /// Stop all of it, as a shutdown does, and return `error`: the
    /// background work and the peer listener joined, the groups shut down,
    /// the denials taken meanwhile made durable.
    async fn abandon(self, error: String) -> String {
        self.stop.send_replace(true);
        for task in self.tasks {
            let _ = task.await;
        }
        let _ = self.peers.await;
        let _ = self.node.host.shutdown().await;
        if flushed(self.audit).await {
            error
        } else {
            format!("{error}; the denials it took did not flush")
        }
    }
}

/// Start the node, a tail's mined output taken from before it opens: its
/// groups replay their committed entries through the miner as they open, so
/// a backlog past the channels would otherwise hold the start forever.
async fn start_draining(
    start: NodeStart,
    mined: Option<mpsc::Receiver<GroupMined>>,
    (stop, stopped): (&watch::Sender<bool>, &watch::Receiver<bool>),
) -> Result<(Node, Vec<JoinHandle<()>>), StartError> {
    let tasks: Vec<_> = mined
        .map(|mined| spawn_mined_drain(mined, stopped.clone()))
        .into_iter()
        .collect();
    match node::start(start, &mut |_| {}).await {
        Ok(node) => Ok((node, tasks)),
        Err(e) => {
            joined(stop, tasks).await;
            Err(start_failed(&e))
        }
    }
}

/// A failed node start as a [`StartError`]: only a start a shutdown
/// interrupted, having written nothing, is a requested stop.
pub(super) fn start_failed(error: &NodeError) -> StartError {
    let message = error.to_string();
    match error {
        NodeError::LogIds(LockError::HighWater(TemplateIdsError::Interrupted { .. })) => {
            StartError::Interrupted(message)
        }
        _ => StartError::Failed(message),
    }
}

/// Signal `stop` and wait for `tasks`.
async fn joined(stop: &watch::Sender<bool>, tasks: Vec<JoinHandle<()>>) {
    stop.send_replace(true);
    for task in tasks {
        let _ = task.await;
    }
}

/// `error`, once `node`'s groups have shut down, for a start that failed
/// after the node started.
async fn stopped_with(node: Node, error: String) -> String {
    let _ = node.host.shutdown().await;
    error
}

/// Flush `audit` every `every` until `stopped`, off the runtime's workers.
pub(super) fn spawn_audit_flush(
    audit: SharedParquetAuditSink,
    every: Duration,
    stopped: watch::Receiver<bool>,
) -> JoinHandle<()> {
    // A burst past the sink's ceiling flushes at once, as the local
    // receiver's age sweep does, rather than growing until the next tick.
    let overflow = audit.overflow_notify();
    let flush = move || flushed(audit.clone());
    spawn_flush_loop((flush, overflow), every, stopped)
}

/// Run `flush` every `every`, and at once when `overflow` fires, until
/// `stopped`. After a flush fails, `overflow` is ignored until a tick's
/// flush succeeds, so a store refusing writes is retried at the tick, not
/// sent the growing backlog again at every event past the ceiling.
pub(super) fn spawn_flush_loop<F, Flushing>(
    (flush, overflow): (F, Arc<Notify>),
    every: Duration,
    mut stopped: watch::Receiver<bool>,
) -> JoinHandle<()>
where
    F: Fn() -> Flushing + Send + 'static,
    Flushing: Future<Output = bool> + Send,
{
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(every);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate, and nothing is buffered yet.
        ticks.tick().await;
        let mut failing = false;
        loop {
            tokio::select! {
                _ = ticks.tick() => {}
                () = overflow.notified(), if !failing => {}
                _ = stopped.wait_for(|stop| *stop) => return,
            }
            failing = !flush().await;
        }
    })
}

/// Take a tail replica's mined output until `stopped`. The tail service
/// (RFC 0065 §6.7) serves it once built; until then it is dropped, and the
/// flusher publishes from what each group keeps.
fn spawn_mined_drain(
    mut mined: mpsc::Receiver<GroupMined>,
    mut stopped: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                taken = mined.recv() => if taken.is_none() { return },
                _ = stopped.wait_for(|stop| *stop) => return,
            }
        }
    })
}
