//! Serving the replicated WAL (RFC 0065): a node started from the
//! receiver's configuration, answering its peers over mutual TLS, running
//! its background work, and taking OTLP exports into its groups.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use ourios_ingester::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use ourios_ingester::receiver::SharedIngest;
use ourios_ingester::receiver::http::{HttpConfig, router};
use ourios_ingester::replicated::node::{self, Node, NodeStart};
use ourios_ingester::replicated::raft::host::GroupMined;
use ourios_parquet::store::Caller;
use ourios_server::config::wal::ReplicatedWal;
use ourios_serving::tls::ALPN_GRPC;
use ourios_serving::tls_serve::{LISTENER_REPLICATION, reloading_acceptor};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tonic::transport::server::TcpIncoming;

use super::super::receiver::{
    AUDIT_SINK_CEILING_EVENTS, ReceiverConfig, bind_listeners, build_acceptors, spawn_grpc,
    spawn_http,
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
    /// The groups did not shut down cleanly, or the audit events did not
    /// all flush.
    pub(crate) async fn shutdown(self) -> Result<(), String> {
        let _ = self.otlp.send(());
        let grpc = self.grpc.await;
        let _ = self.http.await;
        self.stop.send_replace(true);
        for task in self.tasks {
            let _ = task.await;
        }
        let _ = self.peers.await;
        let groups = self.node.host.shutdown().await;
        let audit = self.audit;
        let flushed = tokio::task::spawn_blocking(move || audit.flush())
            .await
            .unwrap_or(false);
        grpc.map_err(|e| format!("the gRPC listener's task: {e}"))?
            .map_err(|e| format!("the gRPC listener: {e}"))?;
        groups.map_err(|e| format!("the groups did not shut down: {e}"))?;
        flushed
            .then_some(())
            .ok_or_else(|| "the last audit events did not flush".to_owned())
    }
}

/// Serve the replicated node `replicated` and `config` describe, its peers
/// on `peer_listener`, its template-id scan stopping once `stopping` is set.
///
/// # Errors
///
/// The node did not plan, start or bind.
pub(crate) async fn serve_replicated(
    replicated: &ReplicatedWal,
    config: ReceiverConfig,
    (peer_listener, stopping): (TcpListener, Arc<AtomicBool>),
) -> Result<ReplicatedHandle, String> {
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
            return Err(stopped_with(node, e).await);
        }
    };
    tasks.extend(node.run(&background, &stopped));
    tasks.push(spawn_audit_flush(
        audit.clone(),
        config.max_buffer_age,
        stopped.clone(),
    ));
    let ingest: SharedIngest = Arc::new(
        node.ingest()
            .with_denial_audit_sink(Box::new(audit.clone())),
    );
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
            return Err(started.abandon(e).await);
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
        let audit = self.audit;
        let _ = tokio::task::spawn_blocking(move || audit.flush()).await;
        error
    }
}

/// Start the node, a tail's mined output taken from before it opens: its
/// groups replay their committed entries through the miner as they open, so
/// a backlog past the channels would otherwise hold the start forever.
async fn start_draining(
    start: NodeStart,
    mined: Option<mpsc::Receiver<GroupMined>>,
    (stop, stopped): (&watch::Sender<bool>, &watch::Receiver<bool>),
) -> Result<(Node, Vec<JoinHandle<()>>), String> {
    let tasks: Vec<_> = mined
        .map(|mined| spawn_mined_drain(mined, stopped.clone()))
        .into_iter()
        .collect();
    match node::start(start, &mut |_| {}).await {
        Ok(node) => Ok((node, tasks)),
        Err(e) => {
            joined(stop, tasks).await;
            Err(e.to_string())
        }
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
    mut stopped: watch::Receiver<bool>,
) -> JoinHandle<()> {
    // A burst past the sink's ceiling flushes at once, as the local
    // receiver's age sweep does, rather than growing until the next tick.
    let overflow = audit.overflow_notify();
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(every);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticks.tick() => {}
                () = overflow.notified() => {}
                _ = stopped.wait_for(|stop| *stop) => return,
            }
            let audit = audit.clone();
            let _ = tokio::task::spawn_blocking(move || audit.flush()).await;
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
