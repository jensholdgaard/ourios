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
use ourios_config::MinerConfig;
use ourios_ingester::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use ourios_ingester::barrier::Barrier;
use ourios_ingester::cadence::BarrierEpochs;
use ourios_ingester::publish::PublishCoordinator;
use ourios_ingester::receiver::grpc::{AuthLayer, LogsReceiver};
use ourios_ingester::receiver::http::{HttpConfig, router};
use ourios_ingester::receiver::pipeline::RotationHook;
use ourios_ingester::receiver::{CommitCoordinator, IngestPipeline, SharedPipeline};
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_ingester::recovery;
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::{PromotedAttributes, Store};
use ourios_serving::AuthResolver;
use ourios_serving::tls::{ALPN_GRPC, ALPN_HTTP, TlsSettings};
use ourios_serving::tls_serve::{
    LISTENER_GRPC, LISTENER_HTTP, TlsListener, reloading_acceptor, tls_incoming,
};
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

/// RFC 0052 §3.1's `barrier_secs`, defaulting to the sink's age trigger.
///
/// Two cadences deliberately: a cut drains *every* buffered partition,
/// so running it on the 60-second housekeeping interval would create a
/// sub-target Parquet object per low-volume partition per tick — RFC
/// 0014's small-file hazard reintroduced by the reclamation path. At the
/// age trigger, a partition holding data that old would have flushed
/// anyway.
const BARRIER_TICK: Duration = SINK_MAX_BUFFER_AGE;

/// Soft ceiling on the audit sink's in-memory event buffer (issue #302):
/// reaching it signals an eager off-runtime flush, which keeps the buffer
/// bounded whenever the store is healthy (the realistic case). It is **not** a
/// hard cap — `emit` never drops; under sustained store-unavailability the
/// buffer is retained and may transiently exceed this, exactly like the record
/// sink (dropping would lose template events the WAL can't re-mine past the
/// snapshot gate — `CLAUDE.md` §3.3). Generous by default — a buffered audit
/// event is small and the normal driver is the cadence, not this signal.
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
/// fix #3) — publish the aged record partitions, audit-ordered and race-free
/// (issue #302 fix #1).
///
/// Each sweep takes an **atomic snapshot** of both buffers under the pipeline's
/// miner lock (`with_miner` → [`PublishCoordinator::drain_aged`]; a microsecond
/// memory move, no I/O), then writes off the lock
/// ([`PublishCoordinator::write_ordered`]): the audit batch to durability first,
/// and the record partitions only after — so a record never reaches the store
/// before its template event is durable, and no concurrent `ingest` can split a
/// record from its audit event across the drain. Stops when `shutdown` fires;
/// the shutdown path then drains both sinks fully.
///
/// Each drained snapshot holds the record sink's in-flight publish guard
/// until its off-lock write settles (issue #578), so a rotation or shutdown
/// `wal_high_water` stamp racing the sweep waits it out in
/// [`flush_then_snapshot`] rather than stamping over records that exist only
/// in this task's memory.
fn spawn_age_sweep(
    pipeline: SharedPipeline,
    coordinator: PublishCoordinator,
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
            // The drain takes the miner lock (atomic w.r.t. ingest) for only a
            // memory move; the ordered write does the blocking store I/O off the
            // lock. Run the whole step on the blocking pool rather than stalling
            // a runtime worker.
            let pipeline = pipeline.clone();
            let coordinator = coordinator.clone();
            let step = tokio::task::spawn_blocking({
                let coordinator = coordinator.clone();
                move || {
                    // RFC 0052 §3.1: the sweep's drain takes the barrier
                    // exclusion in shared mode, **before** the miner
                    // lock. Under the miner lock alone a sweep could
                    // begin after a cut's quiesce and before its stamp,
                    // leaving a drained-but-undurable batch outside the
                    // buffers that the barrier then reads as empty —
                    // `quiesce_publishes` waits only for a sweep already
                    // in flight and prevents no new drain. Taken inside
                    // `with_miner` instead, the sweep would hold the
                    // miner lock waiting for the shared exclusion while
                    // a capture held the exclusive one waiting for the
                    // miner.
                    let drained = pipeline.with_bound_miner(|_miner| coordinator.drain_aged());
                    // The cadence is best-effort: a partial write (transient store
                    // error) retains the un-published data + audit (the WAL is the
                    // durability of record) and the next tick retries — so the
                    // published-everything signal is not needed here (no snapshot is
                    // taken at the cadence; that's the rotation/shutdown path).
                    let _published = coordinator.write_ordered(drained, "age");
                }
            })
            .await;
            // A `JoinError` is two different events and the sweep used to treat
            // them alike: `is_cancelled` is the runtime going away, but
            // `is_panic` is a bug in the step. Either way the loop still stops
            // here — see below for why a panic is not yet survivable — and the
            // point of separating them is that a panic now leaves a countable
            // trace. Before this, a panic retired the cadence for the life of
            // the process while the task returned `()` cleanly, so the
            // `JoinHandle` still looked healthy, `shutdown()` ignores it anyway,
            // and nothing logged, counted, or failed. Partitions then drained
            // only on rotation and shutdown, and buffers grew toward an OOM kill
            // with no trail back to the cause (#791).
            //
            // Surviving the panic and sweeping on is the behaviour we actually
            // want, and it is deliberately NOT done here. `write_ordered`
            // consumes the batches `drain_aged` has already taken out of the
            // sink, so a panic inside it drops them: they are neither in the
            // buffers nor in Parquet, and a later rotation seeing empty buffers
            // can stamp a WAL high-water mark over frames that never landed
            // (#796). Stopping bounds that to one step's records; looping would
            // repeat it every tick, which is unbounded loss. Making it safe needs
            // requeue-on-unwind semantics — a §3.4 decision for the #791 RFC,
            // not something to improvise under a panic handler.
            if let Err(join_error) = step {
                count_step_panic(&coordinator, &join_error);
                break;
            }
        }
    })
}

/// Count a cadence-step `JoinError` when it was a panic, and report whether
/// it counted.
///
/// Cancellation must not count: it is the runtime going away, which is
/// ordinary shutdown, and tagging it would make the "dead cadence" signal
/// fire on every clean stop. The old code could not tell the two apart at
/// all (#791), which is the regression the return value exists to pin.
fn count_step_panic(coordinator: &PublishCoordinator, join_error: &tokio::task::JoinError) -> bool {
    if !join_error.is_panic() {
        return false;
    }
    coordinator.record_cadence_panic();
    true
}

/// Quiesce the age sweep's in-flight off-lock publishes (issue #578), flush
/// the audit sink, then the record sink, then write the per-tenant miner
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
/// What a non-barrier cadence point may stamp, and the state it must
/// clear first.
///
/// Two variants rather than an offset beside an `Option<&_>`: only one
/// of the two call sites can have a cadence latch at all, and a caller
/// holding a bare offset could not tell whether it owed the check.
enum Stamp {
    /// `serve`'s post-recovery point. It runs before the pipeline, its
    /// encode pool and its publish guards exist, so nothing can have
    /// latched and there is no state to consult.
    PreFlight(Option<WalOffset>),
    /// A running receiver's shutdown. Refused while the latch is set: a
    /// latch means an encode or a publish unwound and dropped records
    /// this process can no longer account for, and unlike a requeue
    /// those records are in no buffer for the flush below to find.
    /// Stamping over them would put the snapshot horizon above data that
    /// reached neither the store nor a buffer, and recovery suppresses
    /// frames at or below that horizon — silent loss.
    Cadence(Option<WalOffset>, Arc<BarrierEpochs>),
}

impl Stamp {
    fn high_water(&self) -> Option<WalOffset> {
        match self {
            Self::PreFlight(mark) | Self::Cadence(mark, _) => *mark,
        }
    }

    fn refused(&self) -> bool {
        match self {
            Self::PreFlight(_) => false,
            // Every epoch this process has handed out is at or below
            // `current`, so a latch anywhere refuses the stamp.
            Self::Cadence(_, epochs) => epochs.capture().refuses(epochs.current()),
        }
    }
}

fn flush_then_snapshot(
    sink: &SharedParquetSink,
    audit_sink: &SharedParquetAuditSink,
    snapshots_root: &Path,
    miner: &MinerCluster,
    stamp: &Stamp,
    cadence: &str,
) -> bool {
    // The publish half of the RFC 0035 §3.1 barrier (issue #578). Every
    // caller stamps `wal_high_water` from here with exclusive access to
    // `miner` — the rotation hook and shutdown hold the pipeline's miner
    // lock; the post-recovery call in `serve` runs before the pipeline
    // (and its mutex) exists, so exclusivity is by construction. The stamp
    // asserts every acked record at or below the mark is durably captured.
    // At this point those records fall into three disjoint classes, and
    // the quiesce order — encodes, then publishes, then flush, then stamp
    // — covers each:
    //
    //  1. **In-flight encodes**: when an encode pool exists the caller
    //     quiesced it first (the rotation branch in `pipeline.rs`,
    //     `ReceiverHandle::shutdown`); the post-recovery call runs before
    //     any pool is configured, so this class is empty there. Submission
    //     is ingest-gate-ordered, so every frame ≤ mark has finished its
    //     sink emit by then — its records are now buffered or already
    //     published.
    //  2. **Drained-in-flight publishes**: records the age sweep took *out*
    //     of the buffers whose off-lock `write_ordered` has not settled are
    //     exactly the coordinator's in-flight set — each drain acquires a
    //     `PublishGuard` before the take, under this same miner lock.
    //     `quiesce_publishes` waits until every such snapshot is durable in
    //     the store or requeued into the buffers. Before this barrier, a
    //     rotation could stamp across that window and a crash before the
    //     sweep's store PUT completed lost the drained records (#578).
    //  3. **Buffered records**: everything else is in the sinks; the flush
    //     below either drains it or the stamp is skipped.
    //
    // Holding the miner lock keeps the in-flight count at zero from the wait
    // until the stamp (no drain can begin without the lock), so no class-2
    // record can reappear. Shutdown additionally joins the sweep task before
    // it gets here (`flush_tick.await`), but the barrier must not rely on
    // that ordering — this quiesce is what makes every stamping path safe by
    // construction.
    // A settlement that *failed* requeues its records into the buffers,
    // where `flush_all` below finds them and the retained-records gate
    // skips the stamp — so the outcomes need no separate check here. A
    // settlement that *unwound* dropped them instead, and that is what
    // the latch records.
    let _outcomes = sink.quiesce_publishes();
    if stamp.refused() {
        tracing::warn!(
            name: ourios_semconv::EVENT_OURIOS_RECEIVER_SINK_RETAINED,
            "{cadence}: the cadence latch is set (an encode or a publish unwound), so the \
             snapshot is skipped — the next start replays from the checkpoint and re-mines \
             those frames (no acknowledged data is lost; the WAL is durable)"
        );
        return false;
    }
    if !audit_sink.flush() {
        let audit_events = audit_sink.buffered_events();
        tracing::warn!(
            name: ourios_semconv::EVENT_OURIOS_RECEIVER_AUDIT_SINK_RETAINED,
            "{cadence}: audit sink retained {audit_events} event(s) (store unavailable?); skipping \
             the record flush + snapshot this cycle so a clean row isn't exposed before its \
             template event is durable — no acknowledged data is lost (the WAL is durable)"
        );
        return false;
    }
    sink.flush_all();
    let records = sink.buffered_records();
    if records != 0 {
        tracing::warn!(
            name: ourios_semconv::EVENT_OURIOS_RECEIVER_SINK_RETAINED,
            "{cadence}: record sink retained {records} record(s) (store unavailable?); skipping the \
             snapshot so recovery re-mines them — no acknowledged data is lost (the WAL is durable)"
        );
        return false;
    }
    if let Err(e) = recovery::write_snapshots(snapshots_root, miner, stamp.high_water()) {
        tracing::warn!(
            name: ourios_semconv::EVENT_OURIOS_RECEIVER_SNAPSHOT_ERROR,
            "{cadence} snapshot write failed (next start may replay more from the WAL): {e}"
        );
    }
    true
}

/// Where the receiver role binds, the WAL it persists to, and the object
/// store its mined data lands in.
pub struct ReceiverConfig {
    pub grpc_addr: SocketAddr,
    /// RFC 0030 §3.1 — TLS on the gRPC listener (`receiver.grpc_tls`);
    /// `None` serves plaintext.
    pub grpc_tls: Option<TlsSettings>,
    pub http_addr: SocketAddr,
    /// RFC 0030 §3.1 — TLS on the HTTP listener (`receiver.http_tls`);
    /// `None` serves plaintext.
    pub http_tls: Option<TlsSettings>,
    pub wal: WalConfig,
    /// The data store (RFC 0013/0019), opened by the server — local or S3. The
    /// data write path (RFC 0014) flushes Parquet through it; the WAL stays
    /// under `wal.root` on local disk regardless (RFC0013.6 / `CLAUDE.md`
    /// §3.4, §3.6 — the WAL is never on object storage).
    pub store: Store,
    /// The RFC 0022 promoted attribute set every flushed data file projects
    /// (`storage.promoted_attributes`, §3.2).
    pub promoted: PromotedAttributes,
    /// The RFC 0026 / RFC 0029 credential resolver (static store and/or
    /// OIDC verifier; `AuthResolver::static_only(None)` is open mode,
    /// §3.1). Applied to both listeners: the gRPC auth layer and the HTTP
    /// handler authenticate before decode, and the pipeline binds each
    /// batch to the resolved tenant set before the WAL append (§3.2).
    pub auth: AuthResolver,
    /// RFC 0035 §3.1 — worker count for the concurrent encode pool
    /// (`receiver.encode_workers`; the config layer validates ≥ 1 and
    /// defaults to the host's available cores).
    pub encode_workers: usize,
    /// RFC 0050 §3.2 — the miner configuration, carrying the
    /// upstream-template dial (`miner.*`; defaults are byte-identical
    /// pre-RFC behaviour).
    pub miner: MinerConfig,
    /// The RFC 0047 §3.3 graph emitter, fed on the flush cadence, when the
    /// graph is configured with a bound conversation object.
    pub graph_emitter: Option<Arc<ourios_ingester::graph_emitter::GraphEmitter>>,
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
    /// The RFC 0052 §3.1 barrier task (one cut per [`BARRIER_TICK`]);
    /// joined before the shutdown flush so no cut is in flight when it
    /// runs.
    barrier_tick: JoinHandle<()>,
    /// The cadence latch every guard in this receiver reports into. The
    /// shutdown stamp consults it: it is the one stamping path left
    /// outside [`Barrier::run_cut`]'s own checks.
    epochs: Arc<BarrierEpochs>,
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
        // RFC 0052 §3.1: the receiver joins the barrier task after its
        // running cut finishes, so the flush below cannot race a cut
        // holding drained batches outside the buffers. A store failure
        // in that last cut requeues into the buffers as any cut's does,
        // and the flush covers the requeue.
        // A `JoinError` is the barrier task panicking or being aborted
        // outside `tick`'s own `catch_unwind`; the cut it was running
        // may have drained batches it never settled, so latch its epoch
        // and let the stamp below refuse rather than advance past them.
        if self.barrier_tick.await.is_err() {
            self.epochs.report(self.epochs.current());
        }
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
            // RFC 0035 §3.1 barrier at the shutdown cadence point: the
            // listeners are stopped (every acked batch has submitted its
            // encodes), so draining the pool before the flush + snapshot
            // guarantees no record ≤ the stamped high-water is still
            // in-flight or buffered-but-unflushed. The publish half of the
            // barrier (issue #578) is `flush_then_snapshot`'s own
            // `quiesce_publishes` — trivially settled here because the
            // sweep task was joined above, but not reliant on that.
            self.pipeline.quiesce_encodes();
            self.pipeline.with_miner(|miner| {
                flush_then_snapshot(
                    &self.sink,
                    &self.audit_sink,
                    &self.snapshots_root,
                    miner,
                    &Stamp::Cadence(last_durable, Arc::clone(&self.epochs)),
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
///
/// The audit sink is built first so the record sink can take an **audit
/// barrier** (issue #302 fix #2): before any inline size/ceiling publish the
/// record sink flushes the audit sink to durability, so a partition is never
/// put to the store before its template events are durable. That inline publish
/// runs under the miner lock, so the barrier flush + the publish are atomic
/// w.r.t. ingest.
fn build_write_sinks(
    store: Store,
    promoted: PromotedAttributes,
) -> (SharedParquetSink, SharedParquetAuditSink) {
    let audit_sink = SharedParquetAuditSink::new(BufferingAuditSink::new(
        store.clone(),
        AUDIT_SINK_CEILING_EVENTS,
    ));
    let barrier_audit = audit_sink.clone();
    let quarantine_audit = audit_sink.clone();
    let sink = SharedParquetSink::new(
        ParquetRecordSink::new(store, flush_config())
            .with_promoted_attributes(promoted)
            .with_audit_barrier(Box::new(move || barrier_audit.flush()))
            // RFC 0025 §3.3: permanently-rejected records quarantine
            // to the shared audit stream instead of wedging the
            // partition buffer (#362).
            .with_audit_sink(Box::new(quarantine_audit)),
    );
    (sink, audit_sink)
}

/// The WAL-segment-rotation hook (RFC 0001 §6.9 primary cadence point):
/// force-flush every partition through `flush_then_snapshot`, then snapshot at
/// the rotation `mark` only if both sinks drained (the no-loss invariant). The
/// hook fires before the new segment's first record reaches the miner, so the
/// buffers hold exactly the sealed segment's data (RFC0014.3/.5, `CLAUDE.md`
/// §3.4).
///
/// **Capture-only** (RFC 0052 §3.1). It runs on the request path, inside
/// `ingest`, under the ingest gate and the miner lock — so it does no
/// store I/O at all: it drains both sinks into owned batches, serialises
/// the miner, and hands that cut to the barrier task, which flushes,
/// installs and stamps outside every one of those locks. The invariant
/// is unchanged — no snapshot is stamped at `mark` until every record at
/// or below it is durable in the store — but it is now established by
/// the cut the barrier runs rather than by a flush inside the request.
/// The removed `flush_then_snapshot` here was the largest
/// store-I/O-under-the-ingest-lock site in the receiver (issue #791).
///
fn rotation_capture_hook(barrier: Arc<Barrier>) -> RotationHook {
    Box::new(move |miner, mark| {
        barrier.capture_rotation(miner, mark);
    })
}

/// The RFC 0052 §3.1 barrier task: one cut per `BARRIER_TICK`, plus the
/// idle rotation that lets a node with no traffic reclaim at all.
///
/// Append-independent by construction — which is the whole point: the
/// rotation hook fires only after a successful append observes a segment
/// change, so a node that stops receiving traffic would otherwise never
/// advance its checkpoint again, and an idle node is exactly the one
/// whose retained segments have the least reason to exist.
///
/// Every tick runs on the blocking pool: the capture quiesces the encode
/// pool and the run does store I/O.
fn spawn_barrier(
    pipeline: SharedPipeline,
    barrier: Arc<Barrier>,
    mut shutdown: watch::Receiver<()>,
) -> JoinHandle<()> {
    let epochs = barrier.epochs();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(BARRIER_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // the first tick is immediate; skip it
        loop {
            // `shutdown` wins both races: the pre-check for a signal
            // that arrived while the last cut ran, and `biased` for one
            // that is ready alongside the tick. `ReceiverHandle::shutdown`
            // awaits this task, so a cut started here after the signal
            // makes a graceful stop wait out a whole capture and its
            // store I/O.
            if shutdown.has_changed().unwrap_or(true) {
                break;
            }
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = tick.tick() => {}
            }
            let epoch = epochs.current();
            let pipeline = pipeline.clone();
            let barrier = barrier.clone();
            // `tick` catches its own panic and lowers the latch itself,
            // so a `JoinError` means the tick never reached a decision —
            // a cancellation, or an abort `catch_unwind` cannot see. The
            // cut it was running may have drained batches it never
            // settled, so latch its epoch: shutdown's own stamp then
            // refuses rather than advancing the horizon past them.
            if tokio::task::spawn_blocking(move || barrier.tick(&pipeline, true))
                .await
                .is_err()
            {
                epochs.report(epoch);
                break;
            }
        }
    })
}

/// Bind both transports and start serving over one shared
/// `IngestPipeline`. Recovery (RFC 0008 §6.6) runs to completion first,
/// then the post-recovery snapshots are written, and only then do the
/// sockets bind (RFC0008.10). Returns once both sockets are bound — so
/// the caller can observe the addresses (e.g. when binding `:0`) — with
/// serving running on spawned tasks until [`ReceiverHandle::shutdown`].
/// RFC 0030 §3.2: build each listener's TLS acceptor at startup, so
/// unusable material fails here — the config path already preflighted
/// it, but the served role re-derives from `TlsSettings`.
///
/// ALPN is per-listener: gRPC is h2-only, HTTP offers http/1.1 only
/// (axum is built with just the http1 feature).
type Acceptors = (
    Option<ourios_serving::tls_serve::ReloadingAcceptor>,
    Option<ourios_serving::tls_serve::ReloadingAcceptor>,
);

fn build_acceptors(
    grpc_tls: Option<&TlsSettings>,
    http_tls: Option<&TlsSettings>,
) -> Result<Acceptors, String> {
    let grpc = match grpc_tls {
        Some(tls) => Some(
            reloading_acceptor(tls, ALPN_GRPC, LISTENER_GRPC)
                .map_err(|e| format!("receiver.grpc_tls: {e}"))?,
        ),
        None => None,
    };
    let http = match http_tls {
        Some(tls) => Some(
            reloading_acceptor(tls, ALPN_HTTP, LISTENER_HTTP)
                .map_err(|e| format!("receiver.http_tls: {e}"))?,
        ),
        None => None,
    };
    Ok((grpc, http))
}

/// The two background cadences the receiver runs: RFC0014.2's age sweep
/// and RFC 0052 §3.1's barrier.
struct Cadences {
    flush_tick: JoinHandle<()>,
    barrier_tick: JoinHandle<()>,
}

/// What the cadences are built over. A value rather than three more
/// parameters: the barrier and the sweep share a publish coordinator by
/// construction, and handing them separate ones would put two owners on
/// one sink's in-flight accounting.
struct CadenceInputs {
    publisher: PublishCoordinator,
    graph: Option<Arc<ourios_ingester::graph_emitter::GraphEmitter>>,
    /// Built before the pipeline, because the capture-only rotation hook
    /// the pipeline installs holds it (RFC 0052 §3.1).
    barrier: Arc<Barrier>,
}

/// Start both cadences over one publish coordinator.
fn spawn_cadences(
    pipeline: &SharedPipeline,
    inputs: CadenceInputs,
    shutdown: &watch::Receiver<()>,
) -> Cadences {
    let CadenceInputs {
        mut publisher,
        graph,
        barrier,
    } = inputs;
    if let Some(emitter) = graph {
        publisher = publisher.with_graph_emitter(emitter);
    }
    let overflow = publisher.audit().overflow_notify();
    Cadences {
        barrier_tick: spawn_barrier(pipeline.clone(), barrier, shutdown.clone()),
        // The age sweep drains under the barrier exclusion and the miner
        // lock, and writes audit-ordered off both (issue #302 #1/#2).
        flush_tick: spawn_age_sweep(pipeline.clone(), publisher, overflow, shutdown.clone()),
    }
}

/// Bind both listeners before serving, so a `:0` request resolves to the
/// real port in the returned handle. gRPC first, then HTTP.
async fn bind_listeners(
    grpc: SocketAddr,
    http: SocketAddr,
) -> Result<(TcpIncoming, SocketAddr, TcpListener, SocketAddr), String> {
    let grpc_incoming = TcpIncoming::bind(grpc).map_err(|e| format!("bind gRPC {grpc}: {e}"))?;
    let grpc_addr = grpc_incoming
        .local_addr()
        .map_err(|e| format!("gRPC local_addr: {e}"))?;
    let http_listener = TcpListener::bind(http)
        .await
        .map_err(|e| format!("bind HTTP {http}: {e}"))?;
    let http_addr = http_listener
        .local_addr()
        .map_err(|e| format!("HTTP local_addr: {e}"))?;
    Ok((grpc_incoming, grpc_addr, http_listener, http_addr))
}

// Straight-line orchestration: recovery, sink/pipeline assembly, the
// cadence sweep, and the two listener spawns. The RFC 0030 TLS branches
// pushed it past the line cap; splitting it would scatter the shared
// setup across helpers with long capture lists for no clarity gain.
#[allow(clippy::too_many_lines)]
pub async fn serve(config: ReceiverConfig) -> Result<ReceiverHandle, String> {
    let snapshots_root = config.wal.root.join(SNAPSHOTS_DIR);
    // The §3.4 group-commit knobs, captured before `config.wal` is moved
    // into `Wal::open`: the batch window and the segment-fill early-cut.
    let batch_window = Duration::from_millis(config.wal.batch_window_ms);
    let segment_size_bytes = config.wal.segment_size_bytes;
    // RFC0052.13: a snapshot listed at startup governs reclamation only
    // once the snapshots root and its parent are durable **in this
    // process**. A failure here fails startup rather than discarding the
    // snapshots — reclamation may already have removed the frames they
    // cover, so a horizon whose directory entry may not be durable must
    // never be used, and throwing the artefacts away would lose state
    // nothing else can rebuild.
    ourios_ingester::barrier::fsync_snapshots_root(&snapshots_root)
        .map_err(|e| format!("fsync snapshots root: {e}"))?;
    let mut wal = Wal::open(config.wal).map_err(|e| format!("open WAL: {e:?}"))?;

    let (sink, audit_sink) = build_write_sinks(config.store, config.promoted);

    // Wire both sinks into the miner *before* recovery: replay re-mines the
    // un-flushed tail through `miner.ingest`, which re-emits its records into
    // the record sink and its template events into the audit sink (RFC0014.5 —
    // recovery rebuilds the in-memory buffers the crash dropped; the durability
    // of record is the WAL, never the buffers).
    let mut miner = MinerCluster::with_audit_sink(config.miner, Box::new(audit_sink.clone()))
        .with_record_sink(Box::new(sink.clone()));

    let report = recovery::recover(&mut wal, &snapshots_root, &mut miner)
        .map_err(|e| format!("startup recovery: {e}"))?;
    for tenant in report.tenants.iter().filter(|t| t.stale_gap) {
        tracing::warn!(
            name: ourios_semconv::EVENT_OURIOS_RECEIVER_WAL_TRUNCATED,
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
            // The highest offset replay delivered is the right horizon
            // for a *snapshot*: the miner state below covers exactly
            // those frames. It is not a checkpoint mark, and the
            // pipeline's `DurableMark::Replayed` seed keeps the barrier
            // from mistaking it for one.
            &Stamp::PreFlight(report.max_delivered),
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
    let commits = CommitCoordinator::new(Box::new(wal), batch_window, segment_size_bytes);
    // RFC 0052 §3.1: the barrier is built before the pipeline, because
    // the pipeline's rotation hook is now a capture into it. Both
    // cadences share this one publish coordinator — two would put two
    // owners on a single sink's in-flight accounting.
    let publisher = PublishCoordinator::new(sink.clone(), audit_sink.clone());
    let barrier = Arc::new(Barrier::new(
        publisher.clone(),
        Arc::clone(&commits),
        snapshots_root.clone(),
        SINK_CEILING_BYTES,
    ));
    let pipeline: SharedPipeline = Arc::new(
        IngestPipeline::new(Arc::clone(&commits), miner)
            // RFC 0026 §3.4: tenant-binding denials emit `ingest_denied`
            // through the same durable audit sink as every other event.
            .with_denial_audit_sink(Box::new(audit_sink.clone()))
            .with_last_durable(report.max_delivered)
            .with_rotation_hook(rotation_capture_hook(Arc::clone(&barrier)))
            // RFC 0035 §3.1: Parquet encoding runs on the pool, off the
            // global commit gate; the pool emits into the same shared
            // sink the miner holds, so a cut's drain covers it. The
            // pipeline drains the pool inside every capture; shutdown
            // drains it below.
            .with_encode_pool(ourios_ingester::encode_pool::EncodePool::new(
                &sink,
                config.encode_workers,
            )),
    );

    let (grpc_incoming, grpc_addr, http_listener, http_addr) =
        bind_listeners(config.grpc_addr, config.http_addr).await?;

    let (shutdown, shutdown_rx) = watch::channel(());

    // The pool's latch, which the pipeline adopted and the barrier shares
    // — one word across every guard in the receiver (RFC 0052 §3.1).
    let pipeline_epochs = pipeline.epochs();

    let Cadences {
        flush_tick,
        barrier_tick,
    } = spawn_cadences(
        &pipeline,
        CadenceInputs {
            publisher,
            graph: config.graph_emitter.clone(),
            barrier: Arc::clone(&barrier),
        },
        &shutdown_rx,
    );

    // RFC 0026 §3.2 / RFC 0029 §3.3: the auth layer resolves before the
    // message decode (open mode passes through unbound); the handler's
    // pipeline enforces the tenant binding it attaches. A tower layer
    // rather than a sync interceptor because OIDC resolution may await a
    // JWKS refetch.
    let (grpc_acceptor, http_acceptor) =
        build_acceptors(config.grpc_tls.as_ref(), config.http_tls.as_ref())?;

    // The OTel Collector's OTLP exporter gzip-compresses by default, so the
    // receiver must accept gzip to interoperate with a stock Collector
    // (tests/it/collector_interop.rs). Identity stays accepted; this is
    // additive.
    let grpc_service = LogsServiceServer::new(LogsReceiver::new(pipeline.clone()))
        .accept_compressed(tonic::codec::CompressionEncoding::Gzip);
    let auth_layer = AuthLayer::new(config.auth.clone());
    let grpc = tokio::spawn({
        let mut rx = shutdown_rx.clone();
        // `tokio::spawn` heap-allocates the task, so tonic's large serve
        // future never sits on the caller's stack — the lint's concern.
        #[allow(clippy::large_futures)]
        async move {
            let shutdown = async move {
                let _ = rx.changed().await;
            };
            let server = Server::builder()
                .layer(auth_layer)
                .add_service(grpc_service);
            match grpc_acceptor {
                Some(acceptor) => {
                    server
                        .serve_with_incoming_shutdown(
                            tls_incoming(grpc_incoming, acceptor),
                            shutdown,
                        )
                        .await
                }
                None => {
                    server
                        .serve_with_incoming_shutdown(grpc_incoming, shutdown)
                        .await
                }
            }
        }
    });

    let http_router = router(
        pipeline.clone(),
        &HttpConfig {
            auth: config.auth.clone(),
            ..HttpConfig::default()
        },
    );
    let http = tokio::spawn({
        let mut rx = shutdown_rx;
        async move {
            let shutdown = async move {
                let _ = rx.changed().await;
            };
            let make = http_router.into_make_service();
            match http_acceptor {
                Some(acceptor) => {
                    axum::serve(
                        TlsListener::new(http_listener, acceptor, LISTENER_HTTP),
                        make,
                    )
                    .with_graceful_shutdown(shutdown)
                    .await
                }
                None => {
                    axum::serve(http_listener, make)
                        .with_graceful_shutdown(shutdown)
                        .await
                }
            }
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
        barrier_tick,
        epochs: pipeline_epochs,
    })
}

#[cfg(test)]
mod tests {
    use ourios_core::audit::{AuditSink, ParamType};
    use ourios_core::record::{BodyKind, MinedRecord, Param, RecordSink};
    use ourios_core::tenant::TenantId;

    use super::*;

    /// #791: the sweep could not tell a cancelled step from a panicked one —
    /// it broke its loop on either, so a panic retired the flush cadence for
    /// the life of the process while the task returned `()` cleanly and
    /// nothing logged, counted, or failed.
    ///
    /// These drive the real routing with real `JoinError`s and a real
    /// `PublishCoordinator`, so a regression to treating both alike fails
    /// here — and one of them asserts the exported counter too, so a `true`
    /// return with the forwarding call removed fails as well.
    ///
    /// Installing the global meter for that is safe in this binary for a
    /// narrow reason: its unit tests contain no other installer (the server
    /// crate's lives in the separate `rfc0016_6_query_metrics.rs` integration
    /// binary, a different process) and no sibling asserts on metrics. It is
    /// still one installer only — which is why the two cases are one test
    /// rather than two, since siblings sharing a global meter accumulate on
    /// the same counter.
    ///
    /// `ourios-ingester`'s `cadence_panic_metric` covers the complementary
    /// half: that the dimension distinguishes a dead sweep from an ordinary
    /// store error on the same counter.
    mod count_step_panic {
        use super::super::count_step_panic;
        use ourios_ingester::publish::PublishCoordinator;
        use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
        use ourios_parquet::store::Store;
        use std::time::Duration;

        fn coordinator(root: &std::path::Path) -> PublishCoordinator {
            // `Store::local` canonicalizes, so the directories must exist.
            for leaf in ["records", "audit"] {
                std::fs::create_dir_all(root.join(leaf)).expect("store dir");
            }
            let never_flush = FlushConfig {
                target_bytes: usize::MAX,
                max_buffer_age: Duration::from_secs(86_400),
                ceiling_bytes: usize::MAX,
            };
            let records = SharedParquetSink::new(ParquetRecordSink::new(
                Store::local(root.join("records")).expect("record store"),
                never_flush,
            ));
            let audit =
                super::super::SharedParquetAuditSink::new(super::super::BufferingAuditSink::new(
                    Store::local(root.join("audit")).expect("audit store"),
                    super::super::AUDIT_SINK_CEILING_EVENTS,
                ));
            PublishCoordinator::new(records, audit)
        }

        /// A panicked step must be counted, and counted **through the
        /// production helper**.
        ///
        /// Asserting only the returned boolean would leave one edge untested:
        /// dropping the `coordinator.record_cadence_panic()` call while still
        /// returning `true` satisfies a boolean assertion, and the integration
        /// test calls the coordinator directly, so both would pass. Hence one
        /// test covering both the routing and its side effect rather than two.
        ///
        /// It installs the **global** in-memory meter, which is safe here for a
        /// narrow reason: this binary's unit tests contain no other installer
        /// (the server crate's one lives in the separate
        /// `rfc0016_6_query_metrics.rs` integration binary, a different
        /// process), and no sibling test asserts on metrics.
        #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
        async fn the_panic_count_reaches_the_counter_through_the_helper() {
            use opentelemetry_sdk::metrics::data::{
                AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics, SumDataPoint,
            };

            let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");
            let root = tempfile::TempDir::new().expect("root");
            let join_error = tokio::spawn(async { panic!("the step blew up") })
                .await
                .expect_err("a panicking task joins as an Err");

            assert!(count_step_panic(&coordinator(root.path()), &join_error));
            guard.force_flush().expect("force_flush");

            let rms = exporter.get_finished_metrics().expect("metrics exported");
            let tagged: u64 = rms
                .iter()
                .flat_map(ResourceMetrics::scope_metrics)
                .flat_map(ScopeMetrics::metrics)
                .filter(|m| m.name() == ourios_semconv::OURIOS_SINK_FLUSH_ERRORS)
                .filter_map(|m| match m.data() {
                    AggregatedMetrics::U64(MetricData::Sum(sum)) => Some(sum),
                    _ => None,
                })
                .flat_map(opentelemetry_sdk::metrics::data::Sum::data_points)
                .filter(|dp| {
                    dp.attributes().any(|kv| {
                        kv.key.as_str() == "error.type" && kv.value.as_str() == "cadence_panic"
                    })
                })
                .map(SumDataPoint::value)
                .sum();
            assert_eq!(
                tagged, 1,
                "the helper must actually forward to the coordinator — a \
                 `true` return with the call removed is the regression this \
                 pins",
            );
        }

        #[tokio::test]
        async fn a_cancelled_step_is_not_counted() {
            let root = tempfile::TempDir::new().expect("root");
            let handle = tokio::spawn(async {
                // Never completes, so the abort below is what ends it.
                std::future::pending::<()>().await;
            });
            handle.abort();
            let join_error = handle.await.expect_err("an aborted task joins as an Err");
            assert!(join_error.is_cancelled());
            assert!(
                !count_step_panic(&coordinator(root.path()), &join_error),
                "cancellation is ordinary shutdown; counting it would make \
                 the dead-cadence signal fire on every clean stop",
            );
        }
    }

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
    /// eager-flush signal out of these tests.
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
            &Stamp::PreFlight(None),
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
        let drained = flush_then_snapshot(
            &sink,
            &audit,
            &snapshots_root,
            &miner,
            &Stamp::PreFlight(None),
            "test",
        );

        assert!(!drained, "an unavailable store does not drain the sink");
        assert_eq!(
            sink.buffered_records(),
            2,
            "records are retained, not lost — the WAL is the durability of record",
        );
        let snapshot_written =
            std::fs::read_dir(&snapshots_root).is_ok_and(|mut d| d.next().is_some());
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
        let drained = flush_then_snapshot(
            &sink,
            &audit,
            &snapshots_root,
            &miner,
            &Stamp::PreFlight(None),
            "test",
        );

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
        let snapshot_written =
            std::fs::read_dir(&snapshots_root).is_ok_and(|mut d| d.next().is_some());
        assert!(!snapshot_written, "the snapshot is skipped too");
    }

    fn test_wal_config(root: &Path) -> WalConfig {
        WalConfig {
            root: root.to_path_buf(),
            batch_window_ms: 100,
            segment_size_bytes: 128 * 1024 * 1024,
            segment_age_secs: 600,
            housekeeping_secs: 60,
            max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
            rotation_retry_attempts: ourios_wal::DEFAULT_ROTATION_RETRY_ATTEMPTS,
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
            grpc_tls: None,
            http_addr: "127.0.0.1:0".parse().expect("addr"),
            http_tls: None,
            wal: test_wal_config(wal_dir.path()),
            store,
            promoted: PromotedAttributes::default(),
            auth: AuthResolver::static_only(None),
            graph_emitter: None,
            encode_workers: 2,
            miner: MinerConfig::default(),
        })
        .await
        .expect("serve");
        assert_ne!(handle.grpc_addr.port(), 0, "gRPC bound to a real port");
        assert_ne!(handle.http_addr.port(), 0, "HTTP bound to a real port");
        handle.shutdown().await.expect("graceful shutdown");
    }

    /// The `OTel` Collector's OTLP exporter gzip-compresses by default, so the
    /// receiver must accept gzip or a stock Collector fails with
    /// `Unimplemented: Content is compressed with gzip`. The container
    /// interop test (`tests/it/collector_interop.rs`) catches this
    /// end-to-end; this guards it without Docker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_accepts_gzip_compressed_grpc() {
        use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;
        use tonic::codec::CompressionEncoding;

        let wal_dir = tempfile::TempDir::new().expect("wal dir");
        let data_dir = tempfile::TempDir::new().expect("data dir");
        let store = Store::local(data_dir.path()).expect("local store");
        let handle = serve(ReceiverConfig {
            grpc_addr: "127.0.0.1:0".parse().expect("addr"),
            grpc_tls: None,
            http_addr: "127.0.0.1:0".parse().expect("addr"),
            http_tls: None,
            wal: test_wal_config(wal_dir.path()),
            store,
            promoted: PromotedAttributes::default(),
            auth: AuthResolver::static_only(None),
            graph_emitter: None,
            encode_workers: 2,
            miner: MinerConfig::default(),
        })
        .await
        .expect("serve");

        let mut client = LogsServiceClient::connect(format!("http://{}", handle.grpc_addr))
            .await
            .expect("grpc connect")
            .send_compressed(CompressionEncoding::Gzip);
        let mut export = tonic::Request::new(export_request("acme", &["gzip line"]));
        export
            .metadata_mut()
            .insert("x-ourios-tenant", "acme".parse().expect("ascii"));
        client
            .export(export)
            .await
            .expect("a gzip-compressed OTLP export acks");

        handle.shutdown().await.expect("graceful shutdown");
    }

    /// RFC 0050 §3.2 — a non-default `MinerConfig` actually reaches the
    /// served pipeline: under `adopt`, an annotated record's adoption
    /// lands a `template_adopted` audit event in the store. Guards the
    /// `serve` wiring regressing to `MinerConfig::default()`, which
    /// would silently ignore the attribute.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_propagates_the_miner_config() {
        use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;
        use opentelemetry_proto::tonic::common::v1::any_value::Value;
        use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
        use ourios_config::UpstreamTemplates;

        let wal_dir = tempfile::TempDir::new().expect("wal dir");
        let data_dir = tempfile::TempDir::new().expect("data dir");
        let store = Store::local(data_dir.path()).expect("local store");
        let handle = serve(ReceiverConfig {
            grpc_addr: "127.0.0.1:0".parse().expect("addr"),
            grpc_tls: None,
            http_addr: "127.0.0.1:0".parse().expect("addr"),
            http_tls: None,
            wal: test_wal_config(wal_dir.path()),
            store,
            promoted: PromotedAttributes::default(),
            auth: AuthResolver::static_only(None),
            graph_emitter: None,
            encode_workers: 2,
            miner: MinerConfig::default().with_upstream_templates(UpstreamTemplates::Adopt),
        })
        .await
        .expect("serve");

        let mut request = export_request("acme", &["user alice logged in"]);
        request.resource_logs[0].scope_logs[0].log_records[0]
            .attributes
            .push(KeyValue {
                key: "log.record.template".to_owned(),
                value: Some(AnyValue {
                    value: Some(Value::StringValue("user <*> logged in".to_owned())),
                }),
                ..Default::default()
            });
        let mut client = LogsServiceClient::connect(format!("http://{}", handle.grpc_addr))
            .await
            .expect("connect");
        let mut export = tonic::Request::new(request);
        export
            .metadata_mut()
            .insert("x-ourios-tenant", "acme".parse().expect("ascii"));
        client.export(export).await.expect("export acks");

        // The graceful shutdown drains the audit sink to the store.
        handle.shutdown().await.expect("graceful shutdown");

        let mut kinds = Vec::new();
        for path in parquet_files_under(&data_dir.path().join("audit")) {
            let events = ourios_parquet::AuditReader::open_file(&path)
                .expect("open audit file")
                .read_all()
                .expect("read audit file");
            kinds.extend(events.iter().map(|e| e.payload.event_type().to_owned()));
        }
        assert!(
            kinds.iter().any(|k| k == "template_adopted"),
            "an adopt-mode ingest must audit the adoption; saw {kinds:?}",
        );
    }

    /// Every `*.parquet` under `root`, recursively (empty when the
    /// directory does not exist).
    fn parquet_files_under(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(root) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(parquet_files_under(&path));
            } else if path.extension().is_some_and(|e| e == "parquet") {
                out.push(path);
            }
        }
        out
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
            "POST /v1/logs HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/x-protobuf\r\nX-Ourios-Tenant: checkout\r\n\
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
            grpc_tls: None,
            http_addr: "127.0.0.1:0".parse().expect("addr"),
            http_tls: None,
            wal: test_wal_config(wal_dir.path()),
            store,
            promoted: PromotedAttributes::default(),
            auth: AuthResolver::static_only(None),
            graph_emitter: None,
            encode_workers: 2,
            miner: MinerConfig::default(),
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

    // --- RFC0035.2 flush half, through the REAL `flush_then_snapshot`
    // path (`rotation_snapshot_hook`): a buffered-but-unflushed record
    // ≤ the mark either flushes before the stamp, or the stamp is
    // skipped. The ingester-side barrier test covers the drain half +
    // inline-published records; these two arms pin the buffered case
    // against the production hook. ---

    /// A pooled pipeline over a real 1 s-age WAL whose rotation hook is
    /// the production capture-only `rotation_capture_hook`, plus the
    /// barrier it captures into — the caller runs the cut where the hook
    /// used to flush and stamp inline.
    fn rotating_pooled_pipeline(
        wal_root: &Path,
        store: Store,
        snapshots_root: &Path,
    ) -> (SharedPipeline, SharedParquetSink, Arc<Barrier>) {
        let wal = Wal::open(WalConfig {
            segment_age_secs: 1,
            ..test_wal_config(wal_root)
        })
        .expect("open WAL");
        let (sink, audit_sink) = build_write_sinks(store, PromotedAttributes::default());
        let miner =
            MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(audit_sink.clone()))
                .with_record_sink(Box::new(sink.clone()));
        let coordinator =
            CommitCoordinator::new(Box::new(wal), Duration::from_millis(100), 128 * 1024 * 1024);
        let barrier = Arc::new(Barrier::new(
            PublishCoordinator::new(sink.clone(), audit_sink),
            Arc::clone(&coordinator),
            snapshots_root.to_path_buf(),
            SINK_CEILING_BYTES,
        ));
        let pipeline = Arc::new(
            IngestPipeline::new(coordinator, miner)
                .with_encode_pool(ourios_ingester::encode_pool::EncodePool::new(&sink, 2))
                .with_rotation_hook(rotation_capture_hook(Arc::clone(&barrier))),
        );
        (pipeline, sink, barrier)
    }

    /// The barrier task must not start a cut once shutdown is signalled:
    /// `ReceiverHandle::shutdown` awaits this task, so a cut begun here
    /// makes a graceful stop wait out a whole capture and its store I/O.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn barrier_task_takes_no_cut_once_shutdown_is_signalled() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let store_root = tmp.path().join("store");
        std::fs::create_dir_all(&store_root).expect("store root");
        let (pipeline, sink, barrier) = rotating_pooled_pipeline(
            &tmp.path().join("wal"),
            Store::local(&store_root).expect("local store"),
            &tmp.path().join("snapshots"),
        );
        pipeline
            .ingest(
                export_request("checkout", &["user 1 logged in"]),
                ourios_core::tenant::TenantId::new("checkout"),
            )
            .await
            .expect("batch acks");
        pipeline.quiesce_encodes();
        let epochs = barrier.epochs();
        let before = epochs.current();

        // Signalled before the task is spawned, so the very first loop
        // iteration sees it — the arm the pre-check covers, and the one a
        // signal arriving during a cut lands in.
        let (shutdown, shutdown_rx) = watch::channel(());
        shutdown.send(()).expect("signal shutdown");
        spawn_barrier(pipeline.clone(), Arc::clone(&barrier), shutdown_rx)
            .await
            .expect("the barrier task exits cleanly");

        assert_eq!(
            epochs.current(),
            before,
            "no cut was opened after the shutdown signal",
        );
        assert_eq!(
            sink.buffered_records(),
            1,
            "and the record was never drained out of the buffers",
        );
        assert_eq!(barrier.pending_mark(), None, "nothing was left pending");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rfc0035_2_rotation_flushes_buffered_records_before_stamping() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let store_root = tmp.path().join("store");
        std::fs::create_dir_all(&store_root).expect("store root");
        let snapshots_root = tmp.path().join("snapshots");
        let (pipeline, sink, barrier) = rotating_pooled_pipeline(
            &tmp.path().join("wal"),
            Store::local(&store_root).expect("local store"),
            &snapshots_root,
        );

        // Batch A buffers (the production 256 MiB size target never
        // fires for two records) — encoded but NOT flushed.
        pipeline
            .ingest(
                export_request("checkout", &["user 1 logged in", "user 2 logged in"]),
                ourios_core::tenant::TenantId::new("checkout"),
            )
            .await
            .expect("batch A acks");
        let rotation_point = pipeline.last_durable().expect("durable after batch A");
        pipeline.quiesce_encodes();
        assert_eq!(sink.buffered_records(), 2, "batch A is buffered, unflushed");

        // Rotation: the hook is now capture-only (RFC 0052 §3.1), so it
        // takes batch A out of the buffers into a cut and returns without
        // touching the store. Nothing is stamped yet — the invariant moves
        // from "by the time `ingest` returns" to "by the time the cut the
        // rotation handed over has run".
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        pipeline
            .ingest(
                export_request("checkout", &["payment 9 settled"]),
                ourios_core::tenant::TenantId::new("checkout"),
            )
            .await
            .expect("batch B acks");
        pipeline.quiesce_encodes();
        assert_eq!(
            barrier.pending_mark(),
            Some(rotation_point),
            "the rotation handed the barrier a cut at the rotation point",
        );
        assert!(
            !std::fs::read_dir(&snapshots_root).is_ok_and(|mut d| d.next().is_some()),
            "and stamped nothing on the request path",
        );

        // Run the cut the rotation captured: batch A's records reach the
        // store, and only then is the snapshot stamped.
        let cut = {
            let barrier = Arc::clone(&barrier);
            tokio::task::spawn_blocking(move || barrier.run_pending())
                .await
                .expect("the cut runs")
        };
        assert_eq!(cut, ourios_ingester::barrier::CutOutcome::Stamped);

        assert_eq!(
            sink.buffered_records(),
            1,
            "only batch B's record remains buffered — the cut took batch A \
             out of the buffers and published it before the stamp",
        );
        assert!(
            !data_parquet_files(&store_root.join("data")).is_empty(),
            "batch A's records are durably in the store",
        );
        let artefacts =
            ourios_ingester::snapshot_store::load_all(&snapshots_root).expect("load snapshots");
        assert_eq!(artefacts.len(), 1, "the rotation snapshot was stamped");
        let state = ourios_miner::snapshot::load_snapshot(&artefacts[0].1).expect("known version");
        let mark = state.wal_high_water.expect("stamped with a horizon");
        assert_eq!(mark.segment, rotation_point.segment.to_string());
        assert_eq!(mark.byte, rotation_point.byte);
    }

    /// A pooled pipeline over a real 1 s-age WAL, an **age-zero** record
    /// sink (so `drain_aged` takes every partition — the sweep's view of an
    /// aged one, without waiting out a real age), an audit sink, and the
    /// production capture-only `rotation_capture_hook` with the barrier it
    /// captures into.
    fn sweep_race_pipeline(
        wal_root: &Path,
        store_root: &Path,
        audit_root: &Path,
        snapshots_root: &Path,
    ) -> SweepRaceRig {
        let wal = Wal::open(WalConfig {
            segment_age_secs: 1,
            ..test_wal_config(wal_root)
        })
        .expect("open WAL");
        std::fs::create_dir_all(store_root).expect("store root");
        let sink = SharedParquetSink::new(ParquetRecordSink::new(
            Store::local(store_root).expect("local store"),
            FlushConfig {
                target_bytes: usize::MAX,
                max_buffer_age: Duration::ZERO,
                ceiling_bytes: usize::MAX,
            },
        ));
        let audit = audit_sink(audit_root);
        let miner = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(audit.clone()))
            .with_record_sink(Box::new(sink.clone()));
        let coordinator =
            CommitCoordinator::new(Box::new(wal), Duration::from_millis(100), 128 * 1024 * 1024);
        let barrier = Arc::new(Barrier::new(
            PublishCoordinator::new(sink.clone(), audit.clone()),
            Arc::clone(&coordinator),
            snapshots_root.to_path_buf(),
            SINK_CEILING_BYTES,
        ));
        let pipeline = Arc::new(
            IngestPipeline::new(coordinator, miner)
                .with_encode_pool(ourios_ingester::encode_pool::EncodePool::new(&sink, 2))
                .with_rotation_hook(rotation_capture_hook(Arc::clone(&barrier))),
        );
        SweepRaceRig {
            pipeline,
            sink,
            audit,
            barrier,
        }
    }

    /// An age sweep stopped halfway: the drain has happened, the
    /// off-lock `write_ordered` has not, and it stays that way until
    /// `release` is sent. That gap is the #578 window — batch A is in
    /// neither the buffers nor the store.
    struct HeldSweep {
        sweep: std::thread::JoinHandle<()>,
        release: std::sync::mpsc::Sender<()>,
    }

    impl HeldSweep {
        /// Returns once the drain has happened, so the caller can observe
        /// the window rather than race it.
        fn drain_and_hold(pipeline: &SharedPipeline, coordinator: PublishCoordinator) -> Self {
            let (drained_tx, drained_rx) = std::sync::mpsc::channel();
            let (release, release_rx) = std::sync::mpsc::channel::<()>();
            let pipeline = pipeline.clone();
            let sweep = std::thread::spawn(move || {
                let drained = pipeline.with_bound_miner(|_miner| coordinator.drain_aged());
                assert!(!drained.is_empty(), "the sweep drained batch A");
                drained_tx.send(()).expect("signal drained");
                release_rx.recv().expect("hold the write in flight");
                assert!(
                    coordinator.write_ordered(drained, "age"),
                    "the held-back publish lands",
                );
            });
            drained_rx.recv().expect("sweep drained");
            Self { sweep, release }
        }
    }

    /// Four values, so a struct rather than a tuple nobody can read.
    struct SweepRaceRig {
        pipeline: SharedPipeline,
        sink: SharedParquetSink,
        audit: SharedParquetAuditSink,
        barrier: Arc<Barrier>,
    }

    /// Issue #578 — the publish half of the RFC0035.2 barrier: a rotation
    /// firing while the age sweep's off-lock `write_ordered` is in flight
    /// must not stamp `wal_high_water` until that publish settles. The
    /// in-flight publish is the sweep's own two steps run by hand with the
    /// gap held open — the atomic drain under the miner lock, then (held
    /// back by the test) the off-lock ordered write — the exact window a
    /// slow S3 PUT opens. Mutation check: reverting the `quiesce_publishes`
    /// in `flush_then_snapshot` makes the rotation stamp during the window
    /// and the mid-window assertion fail deterministically.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rfc0035_2_rotation_stamp_waits_for_the_sweeps_in_flight_publish() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let store_root = tmp.path().join("store");
        let snapshots_root = tmp.path().join("snapshots");
        let SweepRaceRig {
            pipeline,
            sink,
            audit,
            barrier,
        } = sweep_race_pipeline(
            &tmp.path().join("wal"),
            &store_root,
            &tmp.path().join("audit"),
            &snapshots_root,
        );

        // Batch A: acked, encoded, buffered (no trigger flushes it).
        pipeline
            .ingest(
                export_request("checkout", &["user 1 logged in", "user 2 logged in"]),
                ourios_core::tenant::TenantId::new("checkout"),
            )
            .await
            .expect("batch A acks");
        let rotation_point = pipeline.last_durable().expect("durable after batch A");
        pipeline.quiesce_encodes();
        assert_eq!(sink.buffered_records(), 2, "batch A is buffered");

        // Sweep half 1: the atomic drain under the miner lock. Batch A's
        // records now exist only in `drained` and the WAL — the #578 window.
        let HeldSweep { sweep, release } =
            HeldSweep::drain_and_hold(&pipeline, PublishCoordinator::new(sink.clone(), audit));
        assert_eq!(
            sink.buffered_records(),
            0,
            "batch A left the buffers — in flight, durable nowhere but the WAL",
        );

        // The rotation races the in-flight publish: batch B lands in a new
        // segment and fires the production capture-only hook on the ingest
        // path. RFC 0052 §3.1 moves the wait off that path — the capture
        // returns at once and the cut it handed over does the waiting — so
        // the rotating `ingest` is expected to complete here.
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        let rotate_pipeline = pipeline.clone();
        let rotation = tokio::spawn(async move {
            rotate_pipeline
                .ingest(
                    export_request("checkout", &["payment 9 settled"]),
                    ourios_core::tenant::TenantId::new("checkout"),
                )
                .await
                .expect("batch B acks")
        });
        assert_eq!(rotation.await.expect("rotation task"), 1, "batch B acked");
        assert_eq!(
            barrier.pending_mark(),
            Some(rotation_point),
            "the rotation handed over a cut at the rotation point",
        );

        // Run that cut concurrently with the held publish. It must block in
        // `quiesce_publishes` rather than stamp.
        let cut = tokio::task::spawn_blocking({
            let barrier = Arc::clone(&barrier);
            move || barrier.run_pending()
        });

        // Mid-window: the cut must still be waiting, not stamping. Without
        // the barrier's quiesce the cut has nothing of its own to publish
        // (the sweep already emptied the buffers) and would stamp
        // immediately, far inside this 800 ms observation point.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let snapshot_written =
            std::fs::read_dir(&snapshots_root).is_ok_and(|mut d| d.next().is_some());
        assert!(
            !snapshot_written,
            "the stamp waits while the sweep's publish is in flight (issue #578)",
        );

        release.send(()).expect("release the publish");
        sweep.join().expect("sweep thread");
        assert_eq!(
            cut.await.expect("the cut runs"),
            ourios_ingester::barrier::CutOutcome::Stamped,
        );

        // The stamp landed only after the publish settled: batch A is
        // durably in the store and the snapshot carries the rotation mark.
        assert!(
            !data_parquet_files(&store_root.join("data")).is_empty(),
            "batch A's records are durably in the store",
        );
        let artefacts =
            ourios_ingester::snapshot_store::load_all(&snapshots_root).expect("load snapshots");
        assert_eq!(artefacts.len(), 1, "the rotation snapshot was stamped");
        let state = ourios_miner::snapshot::load_snapshot(&artefacts[0].1).expect("known version");
        let mark = state.wal_high_water.expect("stamped with a horizon");
        assert_eq!(mark.segment, rotation_point.segment.to_string());
        assert_eq!(mark.byte, rotation_point.byte);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rfc0035_2_rotation_skips_the_stamp_when_the_flush_cannot_drain() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let store_root = tmp.path().join("store");
        std::fs::create_dir_all(&store_root).expect("store root");
        let snapshots_root = tmp.path().join("snapshots");
        let (pipeline, sink, barrier) = rotating_pooled_pipeline(
            &tmp.path().join("wal"),
            Store::local(&store_root).expect("local store"),
            &snapshots_root,
        );

        pipeline
            .ingest(
                export_request("checkout", &["user 1 logged in"]),
                ourios_core::tenant::TenantId::new("checkout"),
            )
            .await
            .expect("batch A acks");
        pipeline.quiesce_encodes();
        assert_eq!(sink.buffered_records(), 1, "batch A is buffered, unflushed");

        // Sabotage the store: the cut's publish cannot land, so the
        // snapshot must be skipped — stamping would advance the horizon
        // past a record ≤ the mark that reached no Parquet object.
        std::fs::remove_dir_all(&store_root).expect("remove store dir");
        std::fs::write(&store_root, b"not a directory").expect("sabotage store");

        tokio::time::sleep(Duration::from_millis(1_200)).await;
        pipeline
            .ingest(
                export_request("checkout", &["payment 9 settled"]),
                ourios_core::tenant::TenantId::new("checkout"),
            )
            .await
            .expect("batch B still acks — the capture is best-effort");
        pipeline.quiesce_encodes();

        let cut = {
            let barrier = Arc::clone(&barrier);
            tokio::task::spawn_blocking(move || barrier.run_pending())
                .await
                .expect("the cut runs")
        };
        assert_eq!(
            cut,
            ourios_ingester::barrier::CutOutcome::Retained,
            "the cut retained rather than stamping",
        );

        assert_eq!(
            sink.buffered_records(),
            2,
            "the un-publishable records are back in the buffers (the WAL is the \
             durability of record)",
        );
        let snapshot_written =
            std::fs::read_dir(&snapshots_root).is_ok_and(|mut d| d.next().is_some());
        assert!(
            !snapshot_written,
            "the stamp is skipped while a record ≤ the mark reached no Parquet object",
        );
    }
}
