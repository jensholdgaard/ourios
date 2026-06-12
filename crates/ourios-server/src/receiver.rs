//! The OTLP receiver role (RFC 0003 §6.2 / the §9 process-model
//! resolution): both transports — gRPC (`tonic`) and HTTP (`axum`) —
//! over **one** shared `IngestPipeline` backed by a single `Wal`
//! (RFC 0008 §3.1's single-writer rule). Graceful shutdown is driven by
//! one `watch` channel fanned out to both listeners.
//!
//! Startup runs the RFC 0008 §6.6 recovery driver to completion —
//! snapshot restore + WAL replay under per-consumer horizons — before
//! either listener binds (RFC0008.10: no live append interleaves with
//! replay). Snapshots are written post-recovery and again at graceful
//! shutdown (RFC 0001 §6.9 cadence points; per-segment-rotation cadence
//! is blocked on rotation itself, RFC0008.6).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsServiceServer;
use ourios_core::config::MinerConfig;
use ourios_ingester::receiver::grpc::LogsReceiver;
use ourios_ingester::receiver::http::{HttpConfig, router};
use ourios_ingester::receiver::{IngestPipeline, SharedPipeline, TenantRule};
use ourios_ingester::recovery;
use ourios_miner::cluster::MinerCluster;
use ourios_wal::{Wal, WalConfig};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;

/// Snapshot artefacts live WAL-adjacent, under the WAL root
/// (RFC 0001 §6.9 *Target store*).
const SNAPSHOTS_DIR: &str = "snapshots";

/// Where the receiver role binds, and the WAL it persists to.
pub struct ReceiverConfig {
    pub grpc_addr: SocketAddr,
    pub http_addr: SocketAddr,
    pub wal: WalConfig,
}

/// A running receiver role: the **resolved** bound addresses (so a `:0`
/// request is observable) plus the handles to shut it down.
pub struct ReceiverHandle {
    pub grpc_addr: SocketAddr,
    pub http_addr: SocketAddr,
    shutdown: watch::Sender<()>,
    grpc: JoinHandle<Result<(), tonic::transport::Error>>,
    http: JoinHandle<std::io::Result<()>>,
    pipeline: SharedPipeline,
    snapshots_root: PathBuf,
}

impl ReceiverHandle {
    /// Signal both listeners to stop and await their graceful shutdown.
    /// Once both tasks return, this handle holds the last reference to
    /// the pipeline (and so to the single `Wal`), with no contention
    /// left on its mutex; the shutdown snapshots are written at that
    /// point (the second §6.9 cadence point) — best-effort: a snapshot
    /// is a rebuildable cache, so a failed write degrades the next
    /// start to a full replay, never a shutdown error. The `Wal` is
    /// released when the handle drops after this returns.
    pub async fn shutdown(self) -> Result<(), String> {
        // A send error just means both listeners already stopped — nothing
        // left to signal.
        let _ = self.shutdown.send(());
        self.grpc
            .await
            .map_err(|e| format!("gRPC listener task: {e}"))?
            .map_err(|e| format!("gRPC listener: {e}"))?;
        self.http
            .await
            .map_err(|e| format!("HTTP listener task: {e}"))?
            .map_err(|e| format!("HTTP listener: {e}"))?;
        // Both listener tasks are gone, so the lock is uncontended; a
        // poisoned mutex means a listener panicked mid-ingest and the
        // miner state is suspect — skip the snapshot (full replay next
        // start) rather than persist it.
        match self.pipeline.lock() {
            Ok(pipeline) => {
                if let Err(e) = recovery::write_snapshots(
                    &self.snapshots_root,
                    pipeline.miner(),
                    pipeline.last_durable(),
                ) {
                    eprintln!("shutdown snapshot write failed (next start full-replays): {e}");
                }
            }
            Err(_) => {
                eprintln!(
                    "pipeline mutex poisoned at shutdown; skipping snapshot write \
                     (next start full-replays)"
                );
            }
        }
        Ok(())
    }
}

/// Bind both transports and start serving over one shared
/// `IngestPipeline`. Recovery (RFC 0008 §6.6) runs to completion first,
/// then the post-recovery snapshots are written, and only then do the
/// sockets bind (RFC0008.10). Returns once both sockets are bound — so
/// the caller can observe the addresses (e.g. when binding `:0`) — with
/// serving running on spawned tasks until [`ReceiverHandle::shutdown`].
pub async fn serve(config: ReceiverConfig) -> Result<ReceiverHandle, String> {
    let snapshots_root = config.wal.root.join(SNAPSHOTS_DIR);
    let mut wal = Wal::open(config.wal).map_err(|e| format!("open WAL: {e:?}"))?;
    let mut miner = MinerCluster::new(MinerConfig::default());
    let rule = TenantRule::service_name();

    let report = recovery::recover(&mut wal, &snapshots_root, &mut miner, &rule)
        .map_err(|e| format!("startup recovery: {e}"))?;
    for tenant in report.tenants.iter().filter(|t| t.stale_gap) {
        // Structured-logging framework is still a follow-up (see
        // main.rs); stderr is the established stopgap warning channel.
        eprintln!(
            "WAL truncated past tenant {:?}'s snapshot high-water mark (external mutation); \
             templates first seen in the gap may re-mint — drift is observable via the \
             RFC 0010 drift query",
            tenant.tenant_id.as_str(),
        );
    }
    // Post-recovery cadence point (RFC 0001 §6.9): persist what replay
    // rebuilt so a crash before the next cadence point doesn't redo it.
    // Best-effort — the snapshot is a rebuildable cache.
    if let Err(e) = recovery::write_snapshots(&snapshots_root, &miner, report.max_delivered) {
        eprintln!("post-recovery snapshot write failed (next start full-replays): {e}");
    }

    // Seed the durable mark from replay so a process serving zero
    // requests still stamps its shutdown snapshots with a concrete
    // horizon — an unstamped snapshot is discarded at the next start
    // (RFC 0001 §6.9), which would overwrite the post-recovery
    // artefacts with full-replay-only ones.
    //
    // The rotation hook is the §6.9 *primary* cadence point: every WAL
    // segment rotation persists per-tenant snapshots at the
    // rotation-point high-water mark. Best-effort, like the other
    // cadence points — a snapshot is a rebuildable cache.
    let hook_root = snapshots_root.clone();
    let pipeline: SharedPipeline = Arc::new(Mutex::new(
        IngestPipeline::new(Box::new(wal), miner, rule)
            .with_last_durable(report.max_delivered)
            .with_rotation_hook(Box::new(move |miner, mark| {
                if let Err(e) = recovery::write_snapshots(&hook_root, miner, Some(mark)) {
                    eprintln!(
                        "rotation snapshot write failed (recovery falls back to the WAL): {e}"
                    );
                }
            })),
    ));

    // gRPC: bind first so `:0` resolves to a real port before serving.
    let grpc_incoming = TcpIncoming::bind(config.grpc_addr)
        .map_err(|e| format!("bind gRPC {}: {e}", config.grpc_addr))?;
    let grpc_addr = grpc_incoming
        .local_addr()
        .map_err(|e| format!("gRPC local_addr: {e}"))?;

    // HTTP: same — bind the listener, then hand it to `axum::serve`.
    let http_listener = TcpListener::bind(config.http_addr)
        .await
        .map_err(|e| format!("bind HTTP {}: {e}", config.http_addr))?;
    let http_addr = http_listener
        .local_addr()
        .map_err(|e| format!("HTTP local_addr: {e}"))?;

    let (shutdown, shutdown_rx) = watch::channel(());

    let grpc_service = LogsServiceServer::new(LogsReceiver::new(pipeline.clone()));
    let grpc = tokio::spawn({
        let mut rx = shutdown_rx.clone();
        async move {
            Server::builder()
                .add_service(grpc_service)
                .serve_with_incoming_shutdown(grpc_incoming, async move {
                    let _ = rx.changed().await;
                })
                .await
        }
    });

    let http_router = router(pipeline.clone(), &HttpConfig::default());
    let http = tokio::spawn({
        let mut rx = shutdown_rx;
        async move {
            axum::serve(http_listener, http_router.into_make_service())
                .with_graceful_shutdown(async move {
                    let _ = rx.changed().await;
                })
                .await
        }
    });

    Ok(ReceiverHandle {
        grpc_addr,
        http_addr,
        shutdown,
        grpc,
        http,
        pipeline,
        snapshots_root,
    })
}
