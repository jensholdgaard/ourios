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
use ourios_ingester::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use ourios_ingester::receiver::grpc::LogsReceiver;
use ourios_ingester::receiver::http::{HttpConfig, router};
use ourios_ingester::receiver::pipeline::RotationHook;
use ourios_ingester::receiver::{CommitCoordinator, IngestPipeline, SharedPipeline, TenantRule};
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_ingester::recovery;
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::Store;
use ourios_wal::{Wal, WalConfig, WalOffset};
use tokio::net::TcpListener;
use tokio::sync::{Notify, watch};
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

/// Soft ceiling on the audit sink's in-memory event buffer (issue #302). The
/// miner has no template-count cap, so an adversarial burst of template churn
/// could otherwise grow the buffer unbounded between `SINK_FLUSH_TICK`s;
/// reaching this signals an eager off-runtime flush (it never blocks `emit` or
/// drops events). Generous by default — a buffered audit event is small and the
/// normal driver is the cadence, not this cap.
const AUDIT_SINK_CEILING_EVENTS: usize = 100_000;

fn flush_config() -> FlushConfig {
    FlushConfig {
        target_bytes: SINK_TARGET_BYTES,
        max_buffer_age: SINK_MAX_BUFFER_AGE,
        ceiling_bytes: SINK_CEILING_BYTES,
    }
}

/// The age-sweep task (RFC0014.2): every [`SINK_FLUSH_TICK`] — or sooner, when
/// the audit buffer reaches its ceiling and raises `audit_overflow` (issue #302
/// fix #4) — flush partitions whose oldest record has reached `max_buffer_age`,
/// so a low-volume partition becomes queryable without waiting for a WAL
/// rotation. The audit sink flushes first, and the record flush is **skipped**
/// when the audit sink did not fully drain: a non-empty buffer means a transient
/// store error (permanent errors drop), so the record flush to the same store
/// would fail anyway, and flushing it would expose a clean row before its
/// template event is durable (issue #302 §3.3). Audit volume is low, so the
/// whole buffer flushes each sweep rather than tracking per-event age. Stops
/// when `shutdown` fires; the shutdown path then drains both sinks fully.
fn spawn_age_sweep(
    sink: SharedParquetSink,
    audit_sink: SharedParquetAuditSink,
    audit_overflow: Arc<Notify>,
    mut shutdown: watch::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SINK_FLUSH_TICK);
        // A slow sweep (e.g. against S3) must not make the interval "catch up"
        // with back-to-back flushes; keep a steady cadence from the last tick.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // the first tick is immediate; skip it
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                () = audit_overflow.notified() => {}
                _ = shutdown.changed() => break,
            }
            // Both flushes encode Parquet and do blocking store I/O (more so
            // against S3), so run them on the blocking pool rather than stalling
            // a runtime worker. A `JoinError` means the runtime is shutting down
            // — stop sweeping.
            let sink = sink.clone();
            let audit_sink = audit_sink.clone();
            if tokio::task::spawn_blocking(move || {
                audit_sink.flush();
                // Only flush records once the audit sink fully drained (issue
                // #302 §3.3 — see this function's doc).
                if audit_sink.buffered_events() == 0 {
                    sink.flush_aged();
                }
            })
            .await
            .is_err()
            {
                break;
            }
        }
    })
}

/// Flush the audit sink, then the record sink, then write the per-tenant miner
/// snapshot **only if both sinks fully drained**.
///
/// This is the no-loss invariant (`CLAUDE.md` §3.4) extended to the audit
/// stream (issue #302). A flush retains any partition whose store write failed
/// (the WAL is the durability of record, so a flush failure is non-fatal).
/// Writing the snapshot anyway would advance the miner's snapshot horizon past
/// data that never reached the store — and recovery suppresses frames at or
/// below that horizon, so on the next start they would never be re-emitted into
/// a fresh sink. For records that is data loss; for the audit stream it is a
/// permanently-empty body on a clean row (`derive_template_registry` would lack
/// the row's `template_created` event, so reconstruction falls back to the
/// empty retained `body` — `CLAUDE.md` §3.3). Skipping the snapshot instead
/// degrades the next start to a fuller replay (which re-mines + re-emits both
/// the un-flushed records *and* their template events, and retries the flush),
/// never loss. Best-effort, like every RFC 0001 §6.9 cadence point; `cadence`
/// names the call site for the log line.
///
/// The audit sink flushes **before** the record sink so a row's template event
/// is durable no later than the row it describes (the registry can render it).
/// If the audit sink does not fully drain, the **record flush is skipped** this
/// cycle (issue #302 §3.3): a non-empty audit buffer means a transient store
/// error (permanent errors drop, leaving it empty), so the record flush to the
/// same store would fail anyway, and flushing it would expose a clean row
/// before its template event is durable.
///
/// Returns whether both sinks fully drained: `true` means both buffers cleared
/// and the snapshot was *attempted* (a write failure there is a separate,
/// logged, rebuildable-cache miss — it does not endanger no-loss, since the
/// data is in the store); `false` means data was retained and the snapshot was
/// skipped. Callers log via `cadence`; the value is for tests today and
/// sink-flush metrics later (RFC 0014 §6.3).
fn flush_then_snapshot(
    sink: &SharedParquetSink,
    audit_sink: &SharedParquetAuditSink,
    snapshots_root: &Path,
    miner: &MinerCluster,
    high_water: Option<WalOffset>,
    cadence: &str,
) -> bool {
    audit_sink.flush();
    let audit_events = audit_sink.buffered_events();
    if audit_events != 0 {
        eprintln!(
            "{cadence}: audit sink retained {audit_events} event(s) (store unavailable?); skipping \
             the record flush + snapshot this cycle so a clean row isn't exposed before its \
             template event is durable — no acknowledged data is lost (the WAL is durable)"
        );
        return false;
    }
    sink.flush_all();
    let records = sink.buffered_records();
    if records != 0 {
        eprintln!(
            "{cadence}: record sink retained {records} record(s) (store unavailable?); skipping the \
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
    /// The audit sink (issue #302). Drained on graceful shutdown alongside the
    /// data sink (before it, so a row's template event is durable no later than
    /// the row), and likewise gates the shutdown snapshot.
    audit_sink: SharedParquetAuditSink,
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
                    &self.audit_sink,
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

/// Build the two shared write sinks over the data `store` (RFC 0013/0019,
/// local or S3): the RFC 0014 record sink and the issue #302 audit sink.
///
/// Both buffer cheaply on the request path and flush off the runtime at the
/// same cadence points. The audit sink carries the miner's `template_created` /
/// `template_widened` / `template_type_expanded` events to the RFC 0005 §3.7
/// audit Parquet stream; without it the querier's read-time registry is empty
/// and a clean row's body renders empty (`CLAUDE.md` §3.3). The WAL stays under
/// `wal.root` on local disk regardless (RFC0013.6 / `CLAUDE.md` §3.6).
fn build_write_sinks(store: Store) -> (SharedParquetSink, SharedParquetAuditSink) {
    let sink = SharedParquetSink::new(ParquetRecordSink::new(store.clone(), flush_config()));
    let audit_sink =
        SharedParquetAuditSink::new(BufferingAuditSink::new(store, AUDIT_SINK_CEILING_EVENTS));
    (sink, audit_sink)
}

/// The WAL-segment-rotation hook (RFC 0001 §6.9 primary cadence point):
/// force-flush every partition through `flush_then_snapshot`, then snapshot at
/// the rotation `mark` only if both sinks drained (the no-loss invariant). The
/// hook fires before the new segment's first record reaches the miner, so the
/// buffers hold exactly the sealed segment's data (RFC0014.3/.5, `CLAUDE.md`
/// §3.4). It runs on the request path (inside `ingest`) and does blocking
/// Parquet/store I/O, so `block_in_place` lets the runtime relocate other tasks.
fn rotation_snapshot_hook(
    sink: SharedParquetSink,
    audit_sink: SharedParquetAuditSink,
    snapshots_root: PathBuf,
) -> RotationHook {
    Box::new(move |miner, mark| {
        tokio::task::block_in_place(|| {
            flush_then_snapshot(
                &sink,
                &audit_sink,
                &snapshots_root,
                miner,
                Some(mark),
                "rotation",
            );
        });
    })
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

    let (sink, audit_sink) = build_write_sinks(config.store);

    // Wire both sinks into the miner *before* recovery: replay re-mines the
    // un-flushed tail through `miner.ingest`, which re-emits its records into
    // the record sink and its template events into the audit sink (RFC0014.5 —
    // recovery rebuilds the in-memory buffers the crash dropped; the durability
    // of record is the WAL, never the buffers).
    let mut miner =
        MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(audit_sink.clone()))
            .with_record_sink(Box::new(sink.clone()));
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
            &audit_sink,
            &snapshots_root,
            &miner,
            report.max_delivered,
            "post-recovery",
        );
    });

    // The group-commit coordinator owns the single-writer WAL and folds
    // concurrent appends into one fsync per `wal_batch_window_ms`
    // (RFC0008.8); the pipeline owns the miner + the rotation hook (the §6.9
    // *primary* cadence point). `with_last_durable` seeds the durable mark from
    // replay so a process serving zero requests still stamps its shutdown
    // snapshots with a concrete horizon — an unstamped snapshot is discarded at
    // the next start (RFC 0001 §6.9), which would overwrite the post-recovery
    // artefacts with full-replay-only ones.
    let coordinator = CommitCoordinator::new(Box::new(wal), batch_window, segment_size_bytes);
    let pipeline: SharedPipeline = Arc::new(
        IngestPipeline::new(coordinator, miner, rule)
            .with_last_durable(report.max_delivered)
            .with_rotation_hook(rotation_snapshot_hook(
                sink.clone(),
                audit_sink.clone(),
                snapshots_root.clone(),
            )),
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

    let flush_tick = spawn_age_sweep(
        sink.clone(),
        audit_sink.clone(),
        audit_sink.overflow_notify(),
        shutdown_rx.clone(),
    );

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
        audit_sink,
        flush_tick,
    })
}

#[cfg(test)]
mod tests {
    use ourios_core::audit::{AuditSink, ParamType};
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

    /// An audit sink rooted at `store_root`. A generous ceiling keeps the
    /// bounding signal out of these tests.
    fn audit_sink(store_root: &Path) -> SharedParquetAuditSink {
        std::fs::create_dir_all(store_root).expect("create audit store root");
        SharedParquetAuditSink::new(BufferingAuditSink::new(
            Store::local(store_root).expect("audit store"),
            10_000,
        ))
    }

    /// An audit event for `tenant` (used to seed the audit sink in the
    /// flush-gating test).
    fn audit_event(tenant: &str) -> ourios_core::audit::AuditEvent {
        ourios_core::audit::AuditEvent {
            tenant_id: TenantId::new(tenant),
            timestamp: std::time::UNIX_EPOCH + Duration::from_secs(1_775_127_480),
            payload: ourios_core::audit::AuditPayload::Template {
                template_id: 1,
                triggering_line_hash: ourios_core::audit::hash_triggering_line(b"line"),
                triggering_line_sample: Some("line".to_owned()),
                change: ourios_core::audit::TemplateChange::Created {
                    new_template: "user <*> logged in".to_owned(),
                },
            },
        }
    }

    #[test]
    fn flush_then_snapshot_drains_and_snapshots_when_the_store_accepts_writes() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let sink = buffered_sink(&tmp.path().join("store"));
        let audit = audit_sink(&tmp.path().join("audit"));
        let miner = MinerCluster::new(MinerConfig::default());

        let drained = flush_then_snapshot(
            &sink,
            &audit,
            &tmp.path().join("snapshots"),
            &miner,
            None,
            "test",
        );

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
        let audit = audit_sink(&tmp.path().join("audit"));

        // Make `put_blocking` fail deterministically: replace the store root
        // directory with a regular file, so writing under it errors.
        std::fs::remove_dir_all(&store_root).expect("remove store dir");
        std::fs::write(&store_root, b"not a directory").expect("write sabotage file");

        let snapshots_root = tmp.path().join("snapshots");
        let miner = MinerCluster::new(MinerConfig::default());
        let drained = flush_then_snapshot(&sink, &audit, &snapshots_root, &miner, None, "test");

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

    #[test]
    fn flush_then_snapshot_skips_the_record_flush_when_audit_retains() {
        // issue #302 fix #3: a clean row must not be exposed before its template
        // event is durable. When the audit sink retains events (a transient
        // store error), the record flush is skipped this cycle even though the
        // record store is healthy — flushing it would publish a row whose
        // template the read-time registry can't yet see.
        let tmp = tempfile::TempDir::new().expect("temp");

        // A healthy record store with buffered records.
        let sink = buffered_sink(&tmp.path().join("store"));

        // An audit sink with a buffered event, then a sabotaged store so its
        // flush fails transiently (Io) and the event is retained.
        let audit_root = tmp.path().join("audit");
        let audit = audit_sink(&audit_root);
        {
            let mut producer = audit.clone();
            producer.emit(audit_event("checkout"));
        }
        std::fs::remove_dir_all(&audit_root).expect("remove audit dir");
        std::fs::write(&audit_root, b"not a directory").expect("sabotage audit store");

        let snapshots_root = tmp.path().join("snapshots");
        let miner = MinerCluster::new(MinerConfig::default());
        let drained = flush_then_snapshot(&sink, &audit, &snapshots_root, &miner, None, "test");

        assert!(!drained, "a retained audit buffer blocks the drain");
        assert_eq!(
            audit.buffered_events(),
            1,
            "the audit event is retained (transient store error)",
        );
        assert_eq!(
            sink.buffered_records(),
            2,
            "the record flush is skipped while the audit event isn't durable (issue #302 §3.3)",
        );
        let snapshot_written = std::fs::read_dir(&snapshots_root)
            .ok()
            .is_some_and(|mut d| d.next().is_some());
        assert!(!snapshot_written, "the snapshot is skipped too");
    }

    fn test_wal_config(root: &Path) -> WalConfig {
        WalConfig {
            root: root.to_path_buf(),
            batch_window_ms: 100,
            segment_size_bytes: 128 * 1024 * 1024,
            segment_age_secs: 600,
            housekeeping_secs: 60,
            macos_full_fsync: false,
        }
    }

    /// `serve` threads the server-opened [`Store`] (RFC 0019 slice 2c) into the
    /// data write path and binds the listeners. This drives the local backend in
    /// process — a `Store::local` is passed in, `:0` resolves to real ports, and
    /// graceful shutdown drains cleanly. The S3 backend is exercised end to end
    /// by the RFC0019.3 localstack scenario (slice 3). The binary-spawn
    /// `rfc0013_6_wal_stays_local` covers the full local request path; this is
    /// the focused in-process check of the `ReceiverConfig.store` plumbing.
    // Multi-thread runtime: `serve` uses `block_in_place` for its blocking
    // recovery/flush I/O, which a current-thread runtime can't host.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_threads_the_store_and_binds_then_shuts_down() {
        let wal_dir = tempfile::TempDir::new().expect("wal dir");
        let data_dir = tempfile::TempDir::new().expect("data dir");
        let store = Store::local(data_dir.path()).expect("local store");
        let handle = serve(ReceiverConfig {
            grpc_addr: "127.0.0.1:0".parse().expect("addr"),
            http_addr: "127.0.0.1:0".parse().expect("addr"),
            wal: test_wal_config(wal_dir.path()),
            store,
        })
        .await
        .expect("serve");
        assert_ne!(handle.grpc_addr.port(), 0, "gRPC bound to a real port");
        assert_ne!(handle.http_addr.port(), 0, "HTTP bound to a real port");
        handle.shutdown().await.expect("graceful shutdown");
    }

    /// One OTLP/HTTP export of `bodies` for `service` (its `service.name` routes
    /// to the matching tenant, RFC 0003 §6.3), each record at INFO with a fixed
    /// in-partition timestamp.
    fn export_request(
        service: &str,
        bodies: &[&str],
    ) -> opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest {
        use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
        use opentelemetry_proto::tonic::common::v1::any_value::Value;
        use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
        use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
        use opentelemetry_proto::tonic::resource::v1::Resource;

        let string_value = |s: &str| AnyValue {
            value: Some(Value::StringValue(s.to_owned())),
        };
        let log_records = bodies
            .iter()
            .enumerate()
            .map(|(i, b)| LogRecord {
                body: Some(string_value(b)),
                severity_number: 9, // INFO (RFC 0018)
                time_unix_nano: 1_775_127_480_000_000_000 + u64::try_from(i).unwrap_or(0),
                ..Default::default()
            })
            .collect();
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![KeyValue {
                        key: "service.name".to_owned(),
                        value: Some(string_value(service)),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                scope_logs: vec![ScopeLogs {
                    log_records,
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    /// Hand-rolled OTLP/HTTP `POST /v1/logs` (no HTTP-client dependency); asserts
    /// a `200`.
    async fn post_otlp_http(addr: SocketAddr, body: &[u8]) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut stream = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect HTTP");
        let head = format!(
            "POST /v1/logs HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/x-protobuf\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len(),
        );
        stream.write_all(head.as_bytes()).await.expect("write head");
        stream.write_all(body).await.expect("write body");
        stream.flush().await.expect("flush request");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("read response");
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "export returns 200, got status line {:?}",
            response.lines().next(),
        );
    }

    /// Every `*.parquet` data file under `root`, recursively.
    fn data_parquet_files(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "parquet") {
                    out.push(path);
                }
            }
        }
        out
    }

    /// issue #302: the receiver wires the miner's audit sink, so its
    /// `template_created` / `template_widened` events reach the audit stream and
    /// the read-time registry (RFC 0017 `derive_template_registry`) can render a
    /// clean, high-confidence row's body bit-for-bit — rather than the empty
    /// retained `body` a clean row carries (`CLAUDE.md` §3.3). Before the fix the
    /// registry was empty, so every clean row rendered empty + `RetainedVerbatim`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn receiver_persists_template_audit_so_clean_rows_reconstruct() {
        let wal_dir = tempfile::TempDir::new().expect("wal dir");
        let data_dir = tempfile::TempDir::new().expect("data dir");
        let store = Store::local(data_dir.path()).expect("local store");
        let handle = serve(ReceiverConfig {
            grpc_addr: "127.0.0.1:0".parse().expect("addr"),
            http_addr: "127.0.0.1:0".parse().expect("addr"),
            wal: test_wal_config(wal_dir.path()),
            store,
        })
        .await
        .expect("serve");

        // Clean, near-identical lines mine to a stable `user <*> logged in`
        // template; their body column is dropped (high confidence, §3.1).
        let bodies = ["user 1 logged in", "user 2 logged in", "user 3 logged in"];
        let request = export_request("checkout", &bodies);
        let encoded = {
            use prost::Message;
            request.encode_to_vec()
        };
        post_otlp_http(handle.http_addr, &encoded).await;

        // Graceful shutdown drains the audit sink (before the record sink) and
        // the record sink to the local store.
        handle.shutdown().await.expect("graceful shutdown");

        // The registry folds the template events the receiver persisted.
        let tenant = ourios_core::tenant::TenantId::new("checkout");
        let registry = ourios_querier::derive_template_registry(
            ourios_querier::StoreRef::Local(data_dir.path()),
            &tenant,
        )
        .expect("derive registry");
        assert!(
            !registry.is_empty(),
            "the receiver persisted the miner's template audit events",
        );

        // Every stored data record reconstructs its original line bit-for-bit.
        // Scope the walk to the `data/` subtree so the audit Parquet (a
        // different schema, under `audit/`) isn't read as a data file.
        let mut rendered = Vec::new();
        for file in data_parquet_files(&data_dir.path().join("data")) {
            let records = ourios_parquet::Reader::open_file(&file)
                .expect("open data file")
                .read_all()
                .expect("read records");
            for record in records {
                let ourios_querier::LogBody::Rendered {
                    line,
                    reconstruction,
                } = ourios_querier::render_log_body(&record, &registry)
                else {
                    panic!("a string body renders to a line");
                };
                assert!(
                    matches!(
                        reconstruction,
                        ourios_miner::reconstruct::Reconstruction::Faithful
                    ),
                    "a clean row reconstructs faithfully from its template, not the empty \
                     retained body (issue #302)",
                );
                rendered.push(String::from_utf8(line).expect("utf8 line"));
            }
        }
        rendered.sort();
        let mut want: Vec<String> = bodies.iter().map(|s| (*s).to_owned()).collect();
        want.sort();
        assert_eq!(
            rendered, want,
            "every ingested clean line round-trips out of the store rendered from its template",
        );
    }
}
