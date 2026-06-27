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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsServiceServer;
use ourios_core::config::MinerConfig;
use ourios_ingester::receiver::grpc::LogsReceiver;
use ourios_ingester::receiver::http::{HttpConfig, router};
use ourios_ingester::receiver::{CommitCoordinator, IngestPipeline, SharedPipeline, TenantRule};
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_ingester::recovery;
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::Store;
use ourios_wal::{Wal, WalConfig, WalOffset};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;

/// Snapshot artefacts live WAL-adjacent, under the WAL root
/// (RFC 0001 §6.9 *Target store*).
const SNAPSHOTS_DIR: &str = "snapshots";

/// RFC 0014 §3 flush-policy defaults for the receiver's data sink. These are
/// go-live starting points; tuning against representative corpora — and
/// exposing them as RFC 0004 config knobs — is RFC 0014 §7.
///
/// `target_bytes` is the per-partition in-memory estimate that triggers a
/// flush, aimed at the RFC 0005 §3.5 file-size band; `max_buffer_age` bounds
/// how long a low-volume partition's data stays unqueryable; `ceiling_bytes`
/// is the hard cap on total buffered bytes (RFC0014.4).
const SINK_TARGET_BYTES: usize = 256 * 1024 * 1024;
const SINK_MAX_BUFFER_AGE: Duration = Duration::from_secs(300);
const SINK_CEILING_BYTES: usize = 1024 * 1024 * 1024;
/// How often the age sweep runs (≤ `SINK_MAX_BUFFER_AGE`): an aged partition
/// flushes within `SINK_MAX_BUFFER_AGE + SINK_FLUSH_TICK`.
const SINK_FLUSH_TICK: Duration = Duration::from_secs(30);

fn flush_config() -> FlushConfig {
    FlushConfig {
        target_bytes: SINK_TARGET_BYTES,
        max_buffer_age: SINK_MAX_BUFFER_AGE,
        ceiling_bytes: SINK_CEILING_BYTES,
    }
}

/// The age-sweep task (RFC0014.2): every [`SINK_FLUSH_TICK`], flush partitions
/// whose oldest record has reached `max_buffer_age`, so a low-volume partition
/// becomes queryable without waiting for a WAL rotation. Stops when `shutdown`
/// fires; the shutdown path then drains the sink fully (`flush_all`).
fn spawn_age_sweep(sink: SharedParquetSink, mut shutdown: watch::Receiver<()>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SINK_FLUSH_TICK);
        // A slow sweep (e.g. against S3) must not make the interval "catch up"
        // with back-to-back flushes; keep a steady cadence from the last tick.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // the first tick is immediate; skip it
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    // `flush_aged` encodes Parquet and does blocking store I/O
                    // (more so against S3), so run it on the blocking pool
                    // rather than stalling a runtime worker. A `JoinError`
                    // means the runtime is shutting down — stop sweeping.
                    let sink = sink.clone();
                    if tokio::task::spawn_blocking(move || sink.flush_aged())
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                _ = shutdown.changed() => break,
            }
        }
    })
}

/// Flush the sink, then write the per-tenant miner snapshot **only if the sink
/// fully drained**.
///
/// This is the no-loss invariant (`CLAUDE.md` §3.4). `flush_all` retains any
/// partition whose store write failed (the WAL is the durability of record, so
/// a flush failure is non-fatal). Writing the snapshot anyway would advance the
/// miner's snapshot horizon past records that never reached the store — and
/// recovery suppresses frames at or below that horizon, so on the next start
/// they would never be re-emitted into a fresh sink. That is data loss when the
/// store is unavailable. Skipping the snapshot instead degrades the next start
/// to a fuller replay (which re-mines + re-emits the un-flushed records and
/// retries the flush), never loss. Best-effort, like every RFC 0001 §6.9
/// cadence point; `cadence` names the call site for the log line.
///
/// Returns whether the sink fully drained: `true` means the buffer cleared and
/// the snapshot was *attempted* (a write failure there is a separate, logged,
/// rebuildable-cache miss — it does not endanger no-loss, since the data is in
/// the store); `false` means records were retained and the snapshot was
/// skipped. Callers log via `cadence`; the value is for tests today and
/// sink-flush metrics later (RFC 0014 §6.3).
fn flush_then_snapshot(
    sink: &SharedParquetSink,
    snapshots_root: &Path,
    miner: &MinerCluster,
    high_water: Option<WalOffset>,
    cadence: &str,
) -> bool {
    sink.flush_all();
    let buffered = sink.buffered_records();
    if buffered != 0 {
        eprintln!(
            "{cadence}: sink retained {buffered} record(s) (store unavailable?); skipping the \
             snapshot so recovery re-mines them — no acknowledged data is lost (the WAL is durable)"
        );
        return false;
    }
    if let Err(e) = recovery::write_snapshots(snapshots_root, miner, high_water) {
        eprintln!("{cadence} snapshot write failed (next start may replay more from the WAL): {e}");
    }
    true
}

/// Where the receiver role binds, the WAL it persists to, and the object
/// store its mined data lands in.
pub struct ReceiverConfig {
    pub grpc_addr: SocketAddr,
    pub http_addr: SocketAddr,
    pub wal: WalConfig,
    /// The data store (RFC 0013/0019), opened by the server — local or S3. The
    /// data write path (RFC 0014) flushes Parquet through it; the WAL stays
    /// under `wal.root` on local disk regardless (RFC0013.6 / `CLAUDE.md`
    /// §3.4, §3.6 — the WAL is never on object storage).
    pub store: Store,
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
    /// The data sink (RFC 0014). Drained on graceful shutdown, before the
    /// shutdown snapshot, to keep the miner's snapshot horizon at or below the
    /// sink's flushed horizon (the no-loss invariant; see [`serve`]).
    sink: SharedParquetSink,
    /// The age-sweep task (`flush_aged` every [`SINK_FLUSH_TICK`]); awaited to a
    /// clean exit on shutdown via the `shutdown` watch signal.
    flush_tick: JoinHandle<()>,
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
        // Both listeners are stopped, so no more records reach the sink. The
        // `shutdown.send` above already signalled the age-sweep task; await it
        // (rather than abort it) so an in-flight `spawn_blocking` flush runs to
        // completion and the task exits via its `shutdown.changed()` arm. An
        // abort would cancel the async task but leave that blocking flush
        // holding the sink mutex, which the drain below would then wait on
        // anyway. A `JoinError` (the task panicked) is ignored — the drain
        // below still runs.
        let _ = self.flush_tick.await;
        // Both listener tasks are gone, so the pipeline's inner locks are
        // uncontended. `with_miner` recovers a poisoned miner mutex
        // (`PoisonError::into_inner`) — at shutdown the listeners are
        // already stopped, so any poison is from a past panic on a path
        // that left the miner consistent by construction (the rotation
        // hook is caught, and `ingest` mutates the miner only after the
        // batch is durable); the recovered state is the best snapshot we
        // can write, and a bad one only degrades the next start to a full
        // replay (the snapshot is a rebuildable cache). `flush_then_snapshot`
        // drains the sink first and writes the snapshot only if it drained —
        // the no-loss invariant (see `serve`).
        let last_durable = self.pipeline.last_durable();
        tokio::task::block_in_place(|| {
            self.pipeline.with_miner(|miner| {
                flush_then_snapshot(
                    &self.sink,
                    &self.snapshots_root,
                    miner,
                    last_durable,
                    "shutdown",
                );
            });
        });
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
    // The §3.4 group-commit knobs, captured before `config.wal` is moved
    // into `Wal::open`: the batch window and the segment-fill early-cut.
    let batch_window = Duration::from_millis(config.wal.batch_window_ms);
    let segment_size_bytes = config.wal.segment_size_bytes;
    let mut wal = Wal::open(config.wal).map_err(|e| format!("open WAL: {e:?}"))?;

    // The production data write path (RFC 0014): mined records buffer per
    // partition and flush to Parquet objects on the RFC 0013/0019 store —
    // local or S3, opened by the server and passed in. Only Parquet/audit/
    // manifest objects reach the store; the WAL stays under `wal.root` on local
    // disk (RFC0013.6 / `CLAUDE.md` §3.6 — the WAL is never on object storage).
    let sink = SharedParquetSink::new(ParquetRecordSink::new(config.store, flush_config()));

    // Wire the sink into the miner *before* recovery: replay re-mines the
    // un-flushed tail through `miner.ingest`, which re-emits it into the sink
    // (RFC0014.5 — recovery rebuilds the in-memory buffer the crash dropped;
    // the records are durable in the WAL, never in the buffer).
    let mut miner =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
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
    // Post-recovery cadence point (RFC 0001 §6.9): drain the replayed tail,
    // then persist what replay rebuilt so a crash before the next cadence point
    // doesn't redo it. `flush_then_snapshot` gates the snapshot on the drain
    // succeeding (the no-loss invariant); `block_in_place` keeps its blocking
    // Parquet/store I/O off a runtime worker, as at the other cadence points.
    tokio::task::block_in_place(|| {
        flush_then_snapshot(
            &sink,
            &snapshots_root,
            &miner,
            report.max_delivered,
            "post-recovery",
        );
    });

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
    let hook_sink = sink.clone();
    // The group-commit coordinator owns the single-writer WAL and folds
    // concurrent appends into one fsync per `wal_batch_window_ms`
    // (RFC0008.8); the pipeline owns the miner + the rotation hook.
    let coordinator = CommitCoordinator::new(Box::new(wal), batch_window, segment_size_bytes);
    let pipeline: SharedPipeline = Arc::new(
        IngestPipeline::new(coordinator, miner, rule)
            .with_last_durable(report.max_delivered)
            .with_rotation_hook(Box::new(move |miner, mark| {
                // Force-flush every partition, then snapshot at the rotation
                // mark only if the sink drained (`flush_then_snapshot` — the
                // no-loss invariant). The hook fires before the new segment's
                // first record reaches the miner, so the buffer holds exactly
                // the sealed segment's records (RFC0014.3/.5, `CLAUDE.md` §3.4).
                // This runs on the request path (inside `ingest`) and does
                // blocking Parquet/store I/O, so `block_in_place` lets the
                // runtime relocate other tasks off this worker.
                tokio::task::block_in_place(|| {
                    flush_then_snapshot(&hook_sink, &hook_root, miner, Some(mark), "rotation");
                });
            })),
    );

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

    let flush_tick = spawn_age_sweep(sink.clone(), shutdown_rx.clone());

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
        sink,
        flush_tick,
    })
}

#[cfg(test)]
mod tests {
    use ourios_core::audit::ParamType;
    use ourios_core::record::{BodyKind, MinedRecord, Param, RecordSink};
    use ourios_core::tenant::TenantId;

    use super::*;

    fn rec() -> MinedRecord {
        MinedRecord {
            tenant_id: TenantId::new("checkout"),
            template_id: 1,
            template_version: 1,
            severity_number: 9,
            severity_text: None,
            scope_name: None,
            scope_version: None,
            scope_attributes: Vec::new(),
            resource_schema_url: None,
            scope_schema_url: None,
            time_unix_nano: 1_775_127_480_000_000_000,
            observed_time_unix_nano: None,
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            resource_attributes: Vec::new(),
            trace_id: None,
            span_id: None,
            flags: 0,
            event_name: None,
            body_kind: BodyKind::String,
            params: vec![Param {
                type_tag: ParamType::Num,
                value: "1".to_string(),
            }],
            separators: vec![String::new(), String::new()],
            body: None,
            confidence: 1.0,
            lossy_flag: false,
        }
    }

    fn never_flush() -> FlushConfig {
        FlushConfig {
            target_bytes: usize::MAX,
            max_buffer_age: Duration::from_secs(86_400),
            ceiling_bytes: usize::MAX,
        }
    }

    fn buffered_sink(store_root: &Path) -> SharedParquetSink {
        std::fs::create_dir_all(store_root).expect("create store root");
        let sink = SharedParquetSink::new(ParquetRecordSink::new(
            Store::local(store_root).expect("store"),
            never_flush(),
        ));
        let mut producer = sink.clone();
        producer.emit(rec());
        producer.emit(rec());
        assert_eq!(
            sink.buffered_records(),
            2,
            "records buffered, not yet flushed"
        );
        sink
    }

    #[test]
    fn flush_then_snapshot_drains_and_snapshots_when_the_store_accepts_writes() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let sink = buffered_sink(&tmp.path().join("store"));
        let miner = MinerCluster::new(MinerConfig::default());

        let drained =
            flush_then_snapshot(&sink, &tmp.path().join("snapshots"), &miner, None, "test");

        assert!(drained, "a working store drains the sink");
        assert_eq!(sink.buffered_records(), 0, "the buffer cleared on flush");
    }

    #[test]
    fn flush_then_snapshot_skips_the_snapshot_when_the_sink_cannot_drain() {
        // The no-loss guard (`CLAUDE.md` §3.4): when the store rejects writes,
        // the records stay buffered (durable in the WAL) and the snapshot is
        // skipped, so the miner's horizon can't advance past un-flushed data
        // and recovery will re-mine them.
        let tmp = tempfile::TempDir::new().expect("temp");
        let store_root = tmp.path().join("store");
        let sink = buffered_sink(&store_root);

        // Make `put_blocking` fail deterministically: replace the store root
        // directory with a regular file, so writing under it errors.
        std::fs::remove_dir_all(&store_root).expect("remove store dir");
        std::fs::write(&store_root, b"not a directory").expect("write sabotage file");

        let snapshots_root = tmp.path().join("snapshots");
        let miner = MinerCluster::new(MinerConfig::default());
        let drained = flush_then_snapshot(&sink, &snapshots_root, &miner, None, "test");

        assert!(!drained, "an unavailable store does not drain the sink");
        assert_eq!(
            sink.buffered_records(),
            2,
            "records are retained, not lost — the WAL is the durability of record",
        );
        let snapshot_written = std::fs::read_dir(&snapshots_root)
            .ok()
            .is_some_and(|mut d| d.next().is_some());
        assert!(
            !snapshot_written,
            "the snapshot is skipped, so the horizon cannot advance past un-flushed data",
        );
    }
}
