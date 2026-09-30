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
use ourios_core::tenant::TenantId;
use ourios_ingester::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use ourios_ingester::barrier::{Barrier, CutOutcome};
use ourios_ingester::cadence::{self, BarrierEpochs};
use ourios_ingester::housekeeping::Housekeeper;
use ourios_ingester::publish::PublishCoordinator;
use ourios_ingester::receiver::grpc::{AuthLayer, LogsReceiver};
use ourios_ingester::receiver::http::{HttpConfig, router};
use ourios_ingester::receiver::pipeline::RotationHook;
use ourios_ingester::receiver::{CommitCoordinator, IngestPipeline, SharedPipeline};
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_ingester::recovery::{self, RecoveryReport};
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::{PromotedAttributes, Store};
use ourios_serving::AuthResolver;
use ourios_serving::serve::{
    GRPC_KEEPALIVE_INTERVAL, GRPC_KEEPALIVE_TIMEOUT, PlainListener, TCP_KEEPALIVE, accept_backoff,
    serve_http,
};
use ourios_serving::tls::{ALPN_GRPC, ALPN_HTTP, TlsSettings};
use ourios_serving::tls_serve::{
    LISTENER_GRPC, LISTENER_HTTP, ReloadingAcceptor, TlsListener, reloading_acceptor, tls_incoming,
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
/// until its off-lock write settles (issue #578), so a `wal_high_water`
/// stamp racing the sweep waits it out — a cut in [`Barrier::run_cut`],
/// shutdown in [`flush_then_snapshot`] — rather than stamping over
/// records that exist only in this task's memory. The drain itself runs
/// under the barrier exclusion (RFC 0052 §3.1), so it cannot begin
/// between a cut's quiesce and its stamp.
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
            // buffers nor in Parquet, and a later cut seeing empty buffers
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
/// Returns what the cadence point left on disk: [`Snapshotted::Skipped`] when
/// data was retained and the snapshot skipped, otherwise the outcome of the
/// write it attempted (a write failure there is a separate, logged,
/// rebuildable-cache miss — it does not endanger no-loss, since the data is in
/// the store). `serve` seeds the snapshot ledger from it.
/// Which non-barrier cadence point is stamping, and so the state it must
/// clear first. Both stamp each tenant at its own folded horizon (RFC 0052
/// §3.1), read from the miner they snapshot, so an idle tenant keeps the
/// older horizon its state reflects.
///
/// Two variants rather than a flag: only one of the two call sites can
/// have a cadence latch at all.
enum Stamp {
    /// `serve`'s post-recovery point. It runs before the pipeline, its
    /// encode pool and its publish guards exist, so nothing can have
    /// latched and there is no state to consult.
    PreFlight,
    /// A running receiver's shutdown. Refused while the latch is set: a latch means an encode or a
    /// publish unwound and dropped records this process can no longer
    /// account for, and unlike a requeue those records are in no buffer
    /// for the flush below to find. Stamping over them would put the
    /// snapshot horizon above data that reached neither the store nor a
    /// buffer, and recovery suppresses frames at or below that horizon —
    /// silent loss.
    Cadence(Arc<BarrierEpochs>),
}

impl Stamp {
    fn refused(&self) -> bool {
        match self {
            Self::PreFlight => false,
            // Every epoch this process has handed out is at or below
            // `current`, so a latch anywhere refuses the stamp.
            Self::Cadence(epochs) => epochs.capture().refuses(epochs.current()),
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
) -> Snapshotted {
    // The publish half of the RFC 0035 §3.1 barrier (issue #578). Every
    // caller stamps `wal_high_water` from here with exclusive access to
    // `miner` — shutdown holds the pipeline's miner lock; the
    // post-recovery call in `serve` runs before the pipeline (and its
    // mutex) exists, so exclusivity is by construction. (Rotation is no
    // longer a caller: RFC 0052 §3.1 made that hook capture-only, and
    // its cut stamps through `Barrier::run_cut`.) The stamp
    // asserts every acked record at or below the mark is durably captured.
    // At this point those records fall into three disjoint classes, and
    // the quiesce order — encodes, then publishes, then flush, then stamp
    // — covers each:
    //
    //  1. **In-flight encodes**: when an encode pool exists the caller
    //     quiesced it first (`ReceiverHandle::shutdown`); the
    //     post-recovery call runs before
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
        return Snapshotted::Skipped;
    }
    if !audit_sink.flush() {
        let audit_events = audit_sink.buffered_events();
        tracing::warn!(
            name: ourios_semconv::EVENT_OURIOS_RECEIVER_AUDIT_SINK_RETAINED,
            "{cadence}: audit sink retained {audit_events} event(s) (store unavailable?); skipping \
             the record flush + snapshot this cycle so a clean row isn't exposed before its \
             template event is durable — no acknowledged data is lost (the WAL is durable)"
        );
        return Snapshotted::Skipped;
    }
    sink.flush_all();
    let records = sink.buffered_records();
    if records != 0 {
        tracing::warn!(
            name: ourios_semconv::EVENT_OURIOS_RECEIVER_SINK_RETAINED,
            "{cadence}: record sink retained {records} record(s) (store unavailable?); skipping the \
             snapshot so recovery re-mines them — no acknowledged data is lost (the WAL is durable)"
        );
        return Snapshotted::Skipped;
    }
    match recovery::write_folded_snapshots(snapshots_root, miner) {
        Ok(installed) => Snapshotted::Installed(installed),
        Err(e) => {
            tracing::warn!(
                name: ourios_semconv::EVENT_OURIOS_RECEIVER_SNAPSHOT_ERROR,
                "{cadence} snapshot write failed (next start may replay more from the WAL): {e}"
            );
            Snapshotted::WriteFailed
        }
    }
}

/// What [`flush_then_snapshot`] left on disk.
#[derive(Debug)]
enum Snapshotted {
    /// Data was retained or the stamp refused: no artefact was touched.
    Skipped,
    /// Both sinks drained, but the write failed partway: the artefacts
    /// before the failure were replaced and the rest were not.
    WriteFailed,
    /// Every live tenant's artefact was replaced, carrying these
    /// horizons.
    Installed(Vec<(TenantId, WalOffset)>),
}

#[cfg(test)]
impl Snapshotted {
    fn drained(&self) -> bool {
        !matches!(self, Self::Skipped)
    }
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
    http: JoinHandle<()>,
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
    /// The three cadence tasks, each joined before the shutdown flush.
    cadences: Cadences,
    /// The same barrier the task and the rotation hook hold. Shutdown
    /// runs whatever is left in its pending slot once the task is
    /// joined — the hook can fill it after the last tick, and nothing
    /// else would ever release the publish guards it holds.
    barrier: Arc<Barrier>,
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
            .map_err(|e| format!("HTTP listener task: {e}"))?;
        // Both listeners are stopped, so no more records reach the sink. The
        // `shutdown.send` above already signalled the age-sweep task; await it
        // (rather than abort it) so an in-flight `spawn_blocking` flush runs to
        // completion and the task exits via its `shutdown.changed()` arm. An
        // abort would cancel the async task but leave that blocking flush
        // holding the sink mutex, which the drain below would then wait on
        // anyway. A `JoinError` (the task panicked) is ignored — the drain
        // below still runs.
        let _ = self.cadences.sweep.await;
        // RFC 0052 §3.1: the receiver joins the barrier task after its
        // running cut finishes, so the flush below cannot race a cut
        // holding drained batches outside the buffers. A store failure
        // in that last cut requeues into the buffers as any cut's does,
        // and the flush covers the requeue.
        // A `JoinError` from either task is a panic or an abort outside
        // its tick's own `catch_unwind`; a cut it was running may have
        // drained batches it never settled, so it is logged and latched,
        // and the stamp below refuses rather than advance past them.
        cadence::read_join(&self.epochs, "barrier", self.cadences.barrier.await);
        cadence::read_join(
            &self.epochs,
            "housekeeping",
            self.cadences.housekeeping.await,
        );
        // The barrier task is gone, but the slot it fed need not be
        // empty: the rotation hook is capture-only, so an append taken
        // just before the signal can have left a cut there with nothing
        // left to run it. Its `Drained` batches hold the sink's in-flight
        // publish guards, and `flush_then_snapshot`'s own
        // `quiesce_publishes` below would then wait on them forever.
        // Running the cut here settles it either way: one that stamps
        // publishes its batches, and one that cannot — a latched epoch, a
        // retained sink — parks them back into the buffers, where the
        // flush covers them.
        run_pending_fail_closed(&self.barrier, &self.epochs);
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
        tokio::task::block_in_place(|| {
            // RFC 0035 §3.1 barrier at the shutdown cadence point: the
            // listeners are stopped (every acked batch has submitted its
            // encodes), so draining the pool before the flush + snapshot
            // guarantees no record ≤ the stamped high-water is still
            // in-flight or buffered-but-unflushed. The publish half of the
            // barrier (issue #578) is `flush_then_snapshot`'s own
            // `quiesce_publishes`, which also waits out whatever the pool
            // handed the publisher (RFC 0052 §3.1): a detached partition
            // is written, requeued or parked before the flush below.
            self.pipeline.quiesce_encodes();
            self.pipeline.with_miner(|miner| {
                flush_then_snapshot(
                    &self.sink,
                    &self.audit_sink,
                    &self.snapshots_root,
                    miner,
                    &Stamp::Cadence(Arc::clone(&self.epochs)),
                    "shutdown",
                );
            });
        });
        // The joins and the last cut above are the steps that can set the
        // latch after the timer stopped observing it.
        self.cadences.housekeeper.observe_state();
        Ok(())
    }
}

/// Run the barrier's pending cut at shutdown, fail-closed.
///
/// Caught for the same reason [`Barrier::tick`] catches: this call is
/// outside `tick`'s own guard, the cut it runs may have drained batches
/// it never settled, and an unwind here would take the shutdown flush
/// and the snapshot with it. A panic latches, exactly as `shutdown`'s
/// `JoinError` arm does, and the stamp after it refuses.
fn run_pending_fail_closed(barrier: &Barrier, epochs: &BarrierEpochs) {
    latch_on_panic(epochs, || barrier.run_pending());
}

/// Run `cut` off the runtime and swallow an unwind, latching `epochs` to
/// the epoch current at the panic so every later stamp refuses.
fn latch_on_panic(epochs: &BarrierEpochs, cut: impl FnOnce() -> CutOutcome) {
    let ran =
        tokio::task::block_in_place(|| std::panic::catch_unwind(std::panic::AssertUnwindSafe(cut)));
    if ran.is_err() {
        epochs.report(epochs.current());
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
/// barrier** (issue #302 fix #2), so a partition is never put to the store
/// before its template events are durable. Since RFC 0052 §3.1 the barrier
/// runs on an encode worker, which a barrier holding the ingest exclusion
/// waits on, so it is `settled`: every emitted event already durable,
/// answered without store I/O. When it is not, it signals the age sweep to
/// flush the audit buffer, and the partition waits in the buffers for the
/// next append.
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
            .with_audit_barrier(Box::new(move || barrier_audit.settled()))
            // RFC 0025 §3.3: permanently-rejected records quarantine
            // to the shared audit stream instead of wedging the
            // partition buffer (#362).
            .with_audit_sink(Box::new(quarantine_audit)),
    );
    (sink, audit_sink)
}

/// The one publish coordinator both cadences share — two would put two
/// owners on a single sink's in-flight accounting — and the barrier that
/// clones it (RFC 0052 §3.1).
///
/// RFC 0047 §3.3's emitter is attached **here**, before the barrier takes
/// its clone. Attached after, it would reach only the sweep's clone, and
/// every batch a cut or the rotation hook published would be missing from
/// the authorization graph until a compaction re-derived its tuples.
fn build_barrier(
    sinks: (&SharedParquetSink, &SharedParquetAuditSink),
    commits: &Arc<CommitCoordinator>,
    snapshots_root: PathBuf,
    graph_emitter: Option<Arc<ourios_ingester::graph_emitter::GraphEmitter>>,
    horizons: Vec<(TenantId, WalOffset)>,
) -> (PublishCoordinator, Arc<Barrier>) {
    let (sink, audit_sink) = sinks;
    let mut publisher = PublishCoordinator::new(sink.clone(), audit_sink.clone());
    if let Some(emitter) = graph_emitter {
        publisher = publisher.with_graph_emitter(emitter);
    }
    let barrier = Barrier::new(
        publisher.clone(),
        Arc::clone(commits),
        snapshots_root,
        SINK_CEILING_BYTES,
    )
    .with_durable_horizons(horizons);
    (publisher, Arc::new(barrier))
}

/// The WAL-segment-rotation hook (RFC 0001 §6.9 primary cadence point):
/// capture a cut at the rotation `mark`, which the barrier then publishes
/// and snapshots only if both sinks drained (the no-loss invariant). The
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

/// RFC 0052 §3.2's housekeeping task: one capped reclamation pass every
/// `housekeeping_secs`, separate from the barrier and the age sweep so
/// that neither a latched barrier nor a stopped sweep (#795) stops it.
///
/// The first pass runs at once: the task is spawned after recovery has
/// seeded the durable horizons, and a node restarting onto a backlog —
/// #793's restart loop — must not wait a whole interval to shed it.
/// [`Housekeeper::tick`] catches its own panic and counts it, and a
/// failed pass is logged there; either way the next tick retries.
fn spawn_housekeeping(
    housekeeper: Arc<Housekeeper>,
    every: Duration,
    epochs: Arc<BarrierEpochs>,
    mut shutdown: watch::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            if shutdown.has_changed().unwrap_or(true) {
                break;
            }
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = tick.tick() => {}
            }
            let housekeeper = Arc::clone(&housekeeper);
            // The tick catches its own unwind, so a `JoinError` is a
            // cancellation or an abort; §3.2 reads it as a failed cut,
            // the same as the barrier task's.
            let joined = tokio::task::spawn_blocking(move || housekeeper.tick())
                .await
                .map(drop);
            if cadence::read_join(&epochs, "housekeeping", joined) {
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
type Acceptors = (Option<ReloadingAcceptor>, Option<ReloadingAcceptor>);

/// Serve the OTLP/HTTP router on `listener`, over TLS when `acceptor` is
/// set, until `shutdown` fires; the task returns once the listener drains.
fn spawn_http(
    listener: TcpListener,
    acceptor: Option<ReloadingAcceptor>,
    router: axum::Router,
    mut shutdown: watch::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let shutdown = async move {
            let _ = shutdown.changed().await;
        };
        match acceptor {
            Some(acceptor) => {
                serve_http(
                    TlsListener::new(listener, acceptor, LISTENER_HTTP),
                    router,
                    shutdown,
                )
                .await;
            }
            None => {
                serve_http(
                    PlainListener::new(listener, LISTENER_HTTP),
                    router,
                    shutdown,
                )
                .await;
            }
        }
    })
}

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

/// The three background cadences the receiver runs: RFC0014.2's age
/// sweep, RFC 0052 §3.1's barrier and §3.2's housekeeping.
struct Cadences {
    /// The age-sweep task (`flush_aged` every [`SINK_FLUSH_TICK`]); awaited to a
    /// clean exit on shutdown via the `shutdown` watch signal.
    sweep: JoinHandle<()>,
    /// The RFC 0052 §3.1 barrier task (one cut per [`BARRIER_TICK`]);
    /// joined before the shutdown flush so no cut is in flight when it
    /// runs.
    barrier: JoinHandle<()>,
    /// The RFC 0052 §3.2 housekeeping task (one capped pass per
    /// `housekeeping_secs`); joined with the other cadences, before the
    /// shutdown flush, so no pass holds the journal past the handle.
    housekeeping: JoinHandle<()>,
    /// The housekeeping task's owner, kept for shutdown's last
    /// observation: the latch its steps can set lands after the task is
    /// joined (RFC 0052 §3.5).
    housekeeper: Arc<Housekeeper>,
}

/// The housekeeping knobs, captured before `WalConfig` moves into
/// `Wal::open`.
struct HousekeepingPlan {
    every: Duration,
    max_unlinks: usize,
}

impl HousekeepingPlan {
    fn of(wal: &WalConfig) -> Self {
        Self {
            // `interval` panics on a zero period; the config layer's floor is 1.
            every: Duration::from_secs(wal.housekeeping_secs.max(1)),
            max_unlinks: usize::try_from(wal.max_unlinks_per_pass).unwrap_or(usize::MAX),
        }
    }
}

/// What the cadences are built over. A value rather than three more
/// parameters: the barrier and the sweep share a publish coordinator by
/// construction, and handing them separate ones would put two owners on
/// one sink's in-flight accounting.
struct CadenceInputs {
    /// Already carrying RFC 0047 §3.3's graph emitter, if one is
    /// configured: it is attached at construction so the barrier's clone
    /// has it too.
    publisher: PublishCoordinator,
    /// Built before the pipeline, because the capture-only rotation hook
    /// the pipeline installs holds it (RFC 0052 §3.1).
    barrier: Arc<Barrier>,
    /// The journal owner housekeeping reclaims through.
    commits: Arc<CommitCoordinator>,
    housekeeping: HousekeepingPlan,
}

/// Start the cadences over one publish coordinator.
fn spawn_cadences(
    pipeline: &SharedPipeline,
    inputs: CadenceInputs,
    shutdown: &watch::Receiver<()>,
) -> Cadences {
    let CadenceInputs {
        publisher,
        barrier,
        commits,
        housekeeping,
    } = inputs;
    let overflow = publisher.audit().overflow_notify();
    let housekeeper = Arc::new(Housekeeper::new(
        commits,
        Arc::clone(&barrier),
        publisher.clone(),
        housekeeping.max_unlinks,
    ));
    Cadences {
        housekeeper: Arc::clone(&housekeeper),
        housekeeping: spawn_housekeeping(
            housekeeper,
            housekeeping.every,
            barrier.epochs(),
            shutdown.clone(),
        ),
        barrier: spawn_barrier(pipeline.clone(), barrier, shutdown.clone()),
        // The age sweep drains under the barrier exclusion and the miner
        // lock, and writes audit-ordered off both (issue #302 #1/#2).
        sweep: spawn_age_sweep(pipeline.clone(), publisher, overflow, shutdown.clone()),
    }
}

/// Post-recovery cadence point (RFC 0001 §6.9): drain the replayed tail,
/// then persist what replay rebuilt so a crash before the next cadence point
/// doesn't redo it. `flush_then_snapshot` gates the snapshot on the drain
/// succeeding (the no-loss invariant); `block_in_place` keeps its blocking
/// Parquet/store I/O off a runtime worker, as at the other cadence points.
///
/// Each tenant is stamped at its own folded horizon (RFC 0052 §3.1): its
/// restored horizon when replay fed it nothing, otherwise its own last
/// replayed frame. Returns RFC 0052 §3.2's snapshot ledger — only horizons
/// the artefacts on disk carry, or lower ones.
fn snapshot_post_recovery(
    (sink, audit_sink): (&SharedParquetSink, &SharedParquetAuditSink),
    snapshots_root: &Path,
    miner: &MinerCluster,
    report: &RecoveryReport,
) -> Vec<(TenantId, WalOffset)> {
    let snapshotted = tokio::task::block_in_place(|| {
        flush_then_snapshot(
            sink,
            audit_sink,
            snapshots_root,
            miner,
            &Stamp::PreFlight,
            "post-recovery",
        )
    });
    // A skipped or failed write left the restored artefacts in place, and
    // every horizon the write installs is at or above the restored one, so
    // those are a floor under whatever did land.
    match snapshotted {
        Snapshotted::Installed(installed) => installed,
        Snapshotted::Skipped | Snapshotted::WriteFailed => report.accepted_horizons(),
    }
}

/// Bind both listeners before serving, so a `:0` request resolves to the
/// real port in the returned handle. gRPC first, then HTTP.
async fn bind_listeners(
    grpc: SocketAddr,
    http: SocketAddr,
) -> Result<(TcpIncoming, SocketAddr, TcpListener, SocketAddr), String> {
    // `Server::tcp_keepalive` is ignored under `serve_with_incoming`; the
    // incoming stream sets it on each accepted socket instead.
    let grpc_incoming = TcpIncoming::bind(grpc)
        .map_err(|e| format!("bind gRPC {grpc}: {e}"))?
        .with_keepalive(Some(TCP_KEEPALIVE));
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
    let housekeeping = HousekeepingPlan::of(&config.wal);
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
    let ledger = snapshot_post_recovery((&sink, &audit_sink), &snapshots_root, &miner, &report);

    // The group-commit coordinator owns the single-writer WAL and folds
    // concurrent appends into one fsync per `wal_batch_window_ms`
    // (RFC0008.8); the pipeline owns the miner + the rotation hook (the §6.9
    // *primary* cadence point). A process serving zero requests still stamps
    // concrete shutdown horizons: the miner carries each tenant's folded
    // horizon from its restore or its replay (RFC 0052 §3.1).
    let commits = CommitCoordinator::new(Box::new(wal), batch_window, segment_size_bytes);
    // RFC 0052 §3.1: the barrier is built before the pipeline, because
    // the pipeline's rotation hook is now a capture into it.
    let (publisher, barrier) = build_barrier(
        (&sink, &audit_sink),
        &commits,
        snapshots_root.clone(),
        config.graph_emitter.clone(),
        // RFC 0052 §3.2's snapshot ledger starts from the snapshots on
        // disk: an artefact recovery rejected would be rejected again on
        // the next start, so its horizon must never let housekeeping
        // unlink the frames that start would replay.
        ledger,
    );
    let pipeline: SharedPipeline = Arc::new(
        IngestPipeline::new(Arc::clone(&commits), miner)
            // RFC 0026 §3.4: tenant-binding denials emit `ingest_denied`
            // through the same durable audit sink as every other event.
            .with_denial_audit_sink(Box::new(audit_sink.clone()))
            .with_rotation_hook(rotation_capture_hook(Arc::clone(&barrier)))
            // RFC 0035 §3.1: Parquet encoding runs on the pool, off the
            // global commit gate; the pool emits into the same shared
            // sink the miner holds, so a cut's drain covers it. The
            // pipeline drains the pool inside every capture; shutdown
            // drains it below. What the size and ceiling triggers detach
            // goes to the coordinator's publisher (RFC 0052 §3.1), so it
            // feeds the RFC 0047 §3.3 graph like every other publish.
            .with_encode_pool(ourios_ingester::encode_pool::EncodePool::with_publisher(
                publisher.publisher(),
                config.encode_workers,
            )),
    );

    let (grpc_incoming, grpc_addr, http_listener, http_addr) =
        bind_listeners(config.grpc_addr, config.http_addr).await?;

    let (shutdown, shutdown_rx) = watch::channel(());

    // The pool's latch, which the pipeline adopted and the barrier shares
    // — one word across every guard in the receiver (RFC 0052 §3.1).
    let pipeline_epochs = pipeline.epochs();

    let cadences = spawn_cadences(
        &pipeline,
        CadenceInputs {
            publisher,
            barrier: Arc::clone(&barrier),
            commits,
            housekeeping,
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
                .http2_keepalive_interval(Some(GRPC_KEEPALIVE_INTERVAL))
                .http2_keepalive_timeout(Some(GRPC_KEEPALIVE_TIMEOUT))
                .layer(auth_layer)
                .add_service(grpc_service);
            let grpc_incoming = accept_backoff(grpc_incoming, LISTENER_GRPC);
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
    let http = spawn_http(http_listener, http_acceptor, http_router, shutdown_rx);

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
        cadences,
        barrier,
        epochs: pipeline_epochs,
    })
}

#[cfg(test)]
mod tests {
    use ourios_core::audit::{AuditSink, ParamType};
    use ourios_core::record::{BodyKind, MinedRecord, Param, RecordSink};
    use ourios_core::tenant::TenantId;

    use super::*;

    /// `Server::tcp_keepalive` is ignored under `serve_with_incoming`, so the
    /// gRPC socket keepalive rides on the `TcpIncoming` the listener binds.
    #[tokio::test]
    async fn grpc_accepted_sockets_have_tcp_keepalive() {
        let loopback: SocketAddr = "127.0.0.1:0".parse().expect("addr");
        let (mut grpc, grpc_addr, _http, _) =
            bind_listeners(loopback, loopback).await.expect("bind");
        let _client = tokio::net::TcpStream::connect(grpc_addr)
            .await
            .expect("connect");
        let accepted = std::future::poll_fn(|cx| {
            futures_core::Stream::poll_next(std::pin::Pin::new(&mut grpc), cx)
        })
        .await
        .expect("an accepted socket")
        .expect("accept");
        assert!(
            socket2::SockRef::from(&accepted)
                .keepalive()
                .expect("SO_KEEPALIVE")
        );
    }

    /// One test at a time among those that increment the global sink
    /// counter's `cadence_panic` dimension, held for the whole test: the
    /// assertion on that count would otherwise see a sibling's increment.
    async fn cadence_panic_serial() -> tokio::sync::MutexGuard<'static, ()> {
        static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        SERIAL.lock().await
    }

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
    /// the same counter. A sibling that *increments* the counter holds
    /// [`cadence_panic_serial`] for the same reason.
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

            let _serial = super::cadence_panic_serial().await;
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
            &Stamp::PreFlight,
            "test",
        )
        .drained();

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
            &Stamp::PreFlight,
            "test",
        )
        .drained();

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
            &Stamp::PreFlight,
            "test",
        )
        .drained();

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

    /// RFC 0052 §3.1: the shutdown drain runs outside [`Barrier::tick`]'s
    /// own guard, so an unwinding cut would otherwise take the shutdown
    /// flush and the snapshot with it — the two steps that still have to
    /// run for the acknowledged records to survive. The cut may also have
    /// drained batches it never settled, so swallowing the panic is only
    /// safe if it latches: the stamp that follows must refuse.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_shutdown_drain_latches_on_a_panic_rather_than_unwinding() {
        let epochs = BarrierEpochs::new();
        let epoch = epochs.current();
        assert!(
            !epochs.capture().refuses(epoch),
            "a fresh node refuses nothing",
        );

        latch_on_panic(&epochs, || panic!("injected pending-cut panic"));

        assert!(
            epochs.capture().refuses(epoch),
            "the drain's panic latched instead of escaping shutdown",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_shutdown_drain_leaves_a_clean_cut_unlatched() {
        let epochs = BarrierEpochs::new();
        let epoch = epochs.current();
        latch_on_panic(&epochs, || CutOutcome::Stamped);
        assert!(
            !epochs.capture().refuses(epoch),
            "a cut that returned is not a reason to refuse the stamp",
        );
    }

    /// RFC 0052 §3.1: the rotation hook is capture-only, so an append taken
    /// just before the shutdown signal can leave a cut in the slot with the
    /// barrier task already gone. Those `Drained` batches hold the sink's
    /// in-flight publish guards, and `flush_then_snapshot`'s
    /// `quiesce_publishes` waits on them — forever, unless shutdown runs the
    /// pending cut first. `BARRIER_TICK` is five minutes and the task skips
    /// its immediate first tick, so within this test nothing else can settle
    /// the slot: the timeout is the assertion.
    ///
    /// The scenario runs on its own runtime thread and this one is the
    /// clock. An in-runtime `timeout` would not do: the wait it guards is
    /// `quiesce_publishes`, which blocks the worker inside `shutdown`'s own
    /// poll, so the timer would never be polled again and a regression
    /// would hang the job out rather than fail.
    #[test]
    fn shutdown_runs_the_cut_the_rotation_hook_left_pending() {
        let (done, settled) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .expect("runtime");
            let published = runtime.block_on(shutdown_with_a_pending_cut());
            let _ = done.send(published);
        });

        match settled.recv_timeout(Duration::from_secs(30)) {
            Ok(published) => assert!(published, "the acknowledged records reached the store"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("shutdown waited on the pending cut's publish guards instead of running it")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("the scenario panicked; its output is above")
            }
        }
    }

    /// The body of [`shutdown_runs_the_cut_the_rotation_hook_left_pending`]:
    /// serve, leave a cut in the slot from the request path, shut down, and
    /// report whether anything reached the store.
    async fn shutdown_with_a_pending_cut() -> bool {
        use prost::Message;

        let wal_dir = tempfile::TempDir::new().expect("wal dir");
        let data_dir = tempfile::TempDir::new().expect("data dir");
        let store = Store::local(data_dir.path()).expect("local store");
        let handle = serve(ReceiverConfig {
            grpc_addr: "127.0.0.1:0".parse().expect("addr"),
            grpc_tls: None,
            http_addr: "127.0.0.1:0".parse().expect("addr"),
            http_tls: None,
            wal: WalConfig {
                segment_age_secs: 1, // the WAL's floor; slept past below
                ..test_wal_config(wal_dir.path())
            },
            store,
            promoted: PromotedAttributes::default(),
            auth: AuthResolver::static_only(None),
            graph_emitter: None,
            encode_workers: 2,
            miner: MinerConfig::default(),
        })
        .await
        .expect("serve");

        // Given an acknowledged batch, then a second append that observes the
        // aged-out segment and so captures a cut on the request path.
        let export = export_request("checkout", &["user 1 logged in"]).encode_to_vec();
        post_otlp_http(handle.http_addr, &export).await;
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        let export = export_request("checkout", &["payment 9 settled"]).encode_to_vec();
        post_otlp_http(handle.http_addr, &export).await;
        assert!(
            handle.barrier.pending_mark().is_some(),
            "the request path filled the slot and no tick has run",
        );

        // Then shutdown settles it rather than blocking on its publish guards.
        handle.shutdown().await.expect("graceful shutdown");
        !data_parquet_files(data_dir.path()).is_empty()
    }

    /// A journal whose housekeeping prepare counts its calls and unwinds
    /// on the first — the pass the task must survive.
    struct CountedPasses(Arc<std::sync::atomic::AtomicUsize>);

    impl ourios_ingester::receiver::Journal for CountedPasses {
        fn append_batch(
            &mut self,
            _payload: &[u8],
        ) -> Result<WalOffset, ourios_ingester::receiver::ReceiveError> {
            unreachable!("the housekeeping task appends nothing")
        }

        fn sync(&mut self) -> Result<WalOffset, ourios_ingester::receiver::ReceiveError> {
            unreachable!("the housekeeping task syncs nothing")
        }

        fn unflushed_bytes(&self) -> u64 {
            0
        }

        fn housekeeping_prepare(
            &mut self,
            _horizons: &ourios_wal::SnapshotHorizons,
            _max_unlinks: usize,
        ) -> Result<ourios_wal::ReclaimPlan, ourios_wal::ReclaimError> {
            let n = self.0.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            assert!(n != 0, "injected panic in the first housekeeping pass");
            Err(ourios_wal::ReclaimError::NoReclamationSurface)
        }
    }

    /// A production [`Housekeeper`] over [`CountedPasses`], and the latch
    /// its barrier shares.
    fn counted_housekeeper(
        root: &Path,
        passes: &Arc<std::sync::atomic::AtomicUsize>,
    ) -> (Housekeeper, Arc<BarrierEpochs>) {
        std::fs::create_dir_all(root.join("store")).expect("store root");
        let (sink, audit) = build_write_sinks(
            Store::local(root.join("store")).expect("local store"),
            PromotedAttributes::default(),
        );
        let publisher = PublishCoordinator::new(sink, audit);
        let commits = CommitCoordinator::new(
            Box::new(CountedPasses(Arc::clone(passes))),
            Duration::from_millis(20),
            u64::MAX,
        );
        let barrier = Arc::new(Barrier::new(
            publisher.clone(),
            Arc::clone(&commits),
            root.join("snapshots"),
            SINK_CEILING_BYTES,
        ));
        let epochs = barrier.epochs();
        (Housekeeper::new(commits, barrier, publisher, 8), epochs)
    }

    /// Yield until `passes` reaches `n`. Paused time does not advance
    /// while a `spawn_blocking` pass runs, so this waits on the pass
    /// itself rather than on the clock.
    async fn passes_reach(passes: &std::sync::atomic::AtomicUsize, n: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while passes.load(std::sync::atomic::Ordering::Acquire) < n {
            assert!(std::time::Instant::now() < deadline, "pass {n} never ran");
            tokio::task::yield_now().await;
        }
    }

    /// The production housekeeping loop: the first pass runs at once, a
    /// pass that panics costs that tick and not the task, and shutdown
    /// stops every later pass.
    #[tokio::test(start_paused = true)]
    async fn housekeeping_task_passes_at_once_survives_a_panic_and_stops_on_shutdown() {
        let _serial = cadence_panic_serial().await;
        let tmp = tempfile::TempDir::new().expect("temp");
        let passes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (housekeeper, epochs) = counted_housekeeper(tmp.path(), &passes);
        let (shutdown, shutdown_rx) = watch::channel(());
        let every = Duration::from_secs(60);
        let task = spawn_housekeeping(
            Arc::new(housekeeper),
            every,
            Arc::clone(&epochs),
            shutdown_rx,
        );

        // The first pass runs without waiting an interval, and panics.
        passes_reach(&passes, 1).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(
            passes.load(std::sync::atomic::Ordering::Acquire),
            1,
            "one pass before the first interval elapses",
        );
        assert_eq!(
            epochs.capture().failed_epoch(),
            None,
            "a caught housekeeping panic fails no cut",
        );

        // The next tick runs another pass: the panic did not end the task.
        tokio::time::sleep(every).await;
        passes_reach(&passes, 2).await;

        // Shutdown ends the task cleanly, and no later tick runs a pass.
        shutdown.send(()).expect("signal shutdown");
        task.await.expect("the housekeeping task exits cleanly");
        tokio::time::sleep(every * 10).await;
        assert_eq!(
            passes.load(std::sync::atomic::Ordering::Acquire),
            2,
            "no pass after shutdown",
        );
        assert_eq!(epochs.capture().failed_epoch(), None);
    }

    /// The pre-check: a signal that arrived before the loop's first
    /// iteration — the arm a signal landing during a pass also takes —
    /// runs no pass at all.
    #[tokio::test(start_paused = true)]
    async fn housekeeping_task_runs_no_pass_once_shutdown_is_signalled() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let passes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (housekeeper, epochs) = counted_housekeeper(tmp.path(), &passes);
        let (shutdown, shutdown_rx) = watch::channel(());
        shutdown.send(()).expect("signal shutdown");

        spawn_housekeeping(
            Arc::new(housekeeper),
            Duration::from_secs(60),
            epochs,
            shutdown_rx,
        )
        .await
        .expect("the housekeeping task exits cleanly");

        assert_eq!(
            passes.load(std::sync::atomic::Ordering::Acquire),
            0,
            "no pass ran after the shutdown signal",
        );
    }

    /// RFC 0052 §3.2 and #793: a node that restarts onto reclaimable
    /// segments reclaims them on its first housekeeping pass, which runs as
    /// soon as the durable horizons are seeded, not a whole
    /// `housekeeping_secs` later. The interval here is an hour, so only
    /// that immediate pass can explain the removal; a restart loop shorter
    /// than the interval would otherwise never reclaim anything.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_restarted_node_reclaims_on_its_first_pass_without_waiting_an_interval() {
        use prost::Message;

        let wal_dir = tempfile::TempDir::new().expect("wal dir");
        let data_dir = tempfile::TempDir::new().expect("data dir");
        let wal = WalConfig {
            segment_age_secs: 1, // the WAL's floor; slept past below
            housekeeping_secs: 3_600,
            ..test_wal_config(wal_dir.path())
        };
        let config = || ReceiverConfig {
            grpc_addr: "127.0.0.1:0".parse().expect("addr"),
            grpc_tls: None,
            http_addr: "127.0.0.1:0".parse().expect("addr"),
            http_tls: None,
            wal: wal.clone(),
            store: Store::local(data_dir.path()).expect("local store"),
            promoted: PromotedAttributes::default(),
            auth: AuthResolver::static_only(None),
            graph_emitter: None,
            encode_workers: 2,
            miner: MinerConfig::default(),
        };

        // Given a node that sealed a segment and stamped past it: the second
        // append observes the aged-out segment and captures a cut, and
        // shutdown runs that cut, which publishes, snapshots and checkpoints.
        let first = serve(config()).await.expect("serve");
        let export = export_request("checkout", &["user 1 logged in"]).encode_to_vec();
        post_otlp_http(first.http_addr, &export).await;
        let sealed = wal_segments(wal_dir.path());
        assert_eq!(sealed.len(), 1, "one segment before the rotation");
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        let export = export_request("checkout", &["payment 9 settled"]).encode_to_vec();
        post_otlp_http(first.http_addr, &export).await;
        first.shutdown().await.expect("graceful shutdown");
        assert!(
            sealed[0].exists(),
            "the sealed segment survives the first process: its only pass ran at start",
        );

        // When the node restarts with an hour-long housekeeping interval.
        let second = serve(config()).await.expect("serve again");

        // Then the sealed segment is reclaimed well inside that interval.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while sealed[0].exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let reclaimed = !sealed[0].exists();
        second.shutdown().await.expect("graceful shutdown");
        assert!(
            reclaimed,
            "the first pass after recovery reclaimed the stamped segment",
        );
    }

    /// RFC 0052 §3.7: once housekeeping has reclaimed every closed frame,
    /// replay delivers nothing, and a node that restarts idle must keep
    /// each tenant's restored horizon. Writing `None` over it made the
    /// next start discard every snapshot with no frames left to rebuild
    /// from, so the whole miner state was lost and templates re-minted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_idle_restart_onto_a_reclaimed_wal_keeps_every_restored_horizon() {
        let wal_dir = tempfile::TempDir::new().expect("wal dir");
        let data_dir = tempfile::TempDir::new().expect("data dir");
        let snapshots_root = wal_dir.path().join(SNAPSHOTS_DIR);
        let tenant = TenantId::new("checkout");

        // Given a snapshot at the tenant's last frame, and a WAL whose
        // every segment housekeeping reclaimed against that horizon.
        let (horizon, templates) = snapshot_then_reclaim(wal_dir.path(), &tenant);

        let config = || ReceiverConfig {
            grpc_addr: "127.0.0.1:0".parse().expect("addr"),
            grpc_tls: None,
            http_addr: "127.0.0.1:0".parse().expect("addr"),
            http_tls: None,
            wal: test_wal_config(wal_dir.path()),
            store: Store::local(data_dir.path()).expect("local store"),
            promoted: PromotedAttributes::default(),
            auth: AuthResolver::static_only(None),
            graph_emitter: None,
            encode_workers: 2,
            miner: MinerConfig::default(),
        };
        let expected = Some(ourios_miner::snapshot::WalHighWater {
            segment: horizon.segment.to_string(),
            byte: horizon.byte,
        });

        // When the node restarts idle twice.
        for start in ["first", "second"] {
            let node = serve(config()).await.expect("serve");

            // Then each start seeds the ledger and the durable mark from
            // the horizon its post-recovery write installed.
            assert_eq!(
                snapshot_on_disk(&snapshots_root).1,
                expected,
                "{start} start: the post-recovery write keeps the restored horizon",
            );
            assert_seeded_from(&node, &tenant, horizon, start);
            node.shutdown().await.expect("graceful shutdown");

            // And the miner state and its horizon survive the shutdown.
            let (state, high_water) = snapshot_on_disk(&snapshots_root);
            assert_eq!(
                high_water, expected,
                "{start} start: the shutdown keeps it too"
            );
            assert_eq!(state, templates, "{start} start: the templates survive");
        }
    }

    /// RFC 0052 §3.1: shutdown stamps each tenant at its own folded
    /// horizon. A tenant idle since its last frame keeps that frame's
    /// offset rather than rising to the node's last acknowledged turn.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_stamps_each_tenant_at_its_own_folded_horizon() {
        let wal_dir = tempfile::TempDir::new().expect("wal dir");
        let data_dir = tempfile::TempDir::new().expect("data dir");
        let node = serve(local_config(wal_dir.path(), data_dir.path()))
            .await
            .expect("serve");
        let mut acknowledged = Vec::new();
        for tenant in ["search", "checkout"] {
            node.pipeline
                .ingest(
                    export_request(tenant, &["user 1 logged in"]),
                    TenantId::new(tenant),
                )
                .await
                .expect("ingest");
            acknowledged.push(node.pipeline.last_durable());
        }

        node.shutdown().await.expect("graceful shutdown");

        let snapshots_root = wal_dir.path().join(SNAPSHOTS_DIR);
        assert_eq!(disk_high_water(&snapshots_root, "search"), acknowledged[0]);
        assert_eq!(
            disk_high_water(&snapshots_root, "checkout"),
            acknowledged[1]
        );
        assert!(
            acknowledged[0] < acknowledged[1],
            "fixture: search is older"
        );
    }

    fn local_config(wal_root: &Path, data_root: &Path) -> ReceiverConfig {
        ReceiverConfig {
            grpc_addr: "127.0.0.1:0".parse().expect("addr"),
            grpc_tls: None,
            http_addr: "127.0.0.1:0".parse().expect("addr"),
            http_tls: None,
            wal: test_wal_config(wal_root),
            store: Store::local(data_root).expect("local store"),
            promoted: PromotedAttributes::default(),
            auth: AuthResolver::static_only(None),
            graph_emitter: None,
            encode_workers: 2,
            miner: MinerConfig::default(),
        }
    }

    /// Append and sync one frame for `tenant`, snapshot the miner at that
    /// frame's offset, then checkpoint, rotate and run a real housekeeping
    /// pass that unlinks the sealed segment. Returns the horizon and the
    /// mined state the snapshot carries.
    fn snapshot_then_reclaim(
        wal_root: &Path,
        tenant: &TenantId,
    ) -> (WalOffset, ourios_miner::snapshot::SnapshotState) {
        use prost::Message;

        let mut wal = Wal::open(test_wal_config(wal_root)).expect("open");
        let request = export_request(tenant.as_str(), &["user 1 logged in", "user 2 logged in"]);
        let payload = ourios_wal::TenantBatch::encode(tenant.as_str(), &request.encode_to_vec())
            .expect("frame");
        wal.append(ourios_wal::FrameKind::TenantOtlpBatch, &payload)
            .expect("append");
        let horizon = wal.sync().expect("sync");
        let mut miner = MinerCluster::new(MinerConfig::default());
        for record in ourios_ingester::receiver::assign(request, tenant) {
            miner.ingest(&record);
        }
        miner.fold_through(tenant, ourios_ingester::snapshot_store::high_water(horizon));
        let snapshots_root = wal_root.join(SNAPSHOTS_DIR);
        recovery::write_folded_snapshots(&snapshots_root, &miner).expect("snapshot");
        wal.checkpoint(horizon).expect("checkpoint");
        wal.rotate(ourios_wal::RotationKind::Owed).expect("rotate");
        let cap = usize::try_from(ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS).expect("cap fits");
        let pass = wal
            .housekeeping_pass(
                &ourios_wal::SnapshotHorizons::restorable([(tenant.clone(), horizon)]),
                cap,
            )
            .expect("housekeeping");
        assert_eq!(pass.removed_segments, 1, "{pass:?}");
        (horizon, miner.snapshot_state(tenant))
    }

    /// The single artefact under `snapshots_root`, split into its miner
    /// state and its high-water mark.
    fn snapshot_on_disk(
        snapshots_root: &Path,
    ) -> (
        ourios_miner::snapshot::SnapshotState,
        Option<ourios_miner::snapshot::WalHighWater>,
    ) {
        let artefacts = ourios_ingester::snapshot_store::load_all(snapshots_root).expect("load");
        assert_eq!(artefacts.len(), 1, "one tenant's artefact");
        let (state, outcome) = ourios_miner::snapshot::recover(Some(&artefacts[0].1));
        assert_eq!(outcome, ourios_miner::snapshot::RecoveryOutcome::Restored);
        let mut state = state.expect("known-version artefact decodes");
        let high_water = state.wal_high_water.take();
        (state, high_water)
    }

    /// A running node's snapshot ledger carries the one horizon its
    /// post-recovery write installed for `tenant`.
    fn assert_seeded_from(
        node: &ReceiverHandle,
        tenant: &TenantId,
        horizon: WalOffset,
        start: &str,
    ) {
        assert_eq!(
            node.barrier.snapshot_horizons(),
            ourios_wal::SnapshotHorizons::restorable([(tenant.clone(), horizon)]),
            "{start} start: the ledger is what the artefact carries",
        );
    }

    /// RFC 0052 §3.2: the post-recovery seed sets the WAL reclamation
    /// floor. When the flush is skipped, nothing replaced the restored
    /// artefacts, so the ledger must be the restored horizons and not the
    /// higher ones the write would have installed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_skipped_post_recovery_write_seeds_the_restored_horizons() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let [restored, alpha_replayed, beta_replayed] = offsets(&tmp.path().join("wal"));
        let report = restoring("alpha", restored, beta_replayed);
        let sink = buffered_sink(&tmp.path().join("store"));
        let audit_root = tmp.path().join("audit");
        let audit = audit_sink(&audit_root);
        audit.clone().emit(audit_event("alpha"));
        std::fs::remove_dir_all(&audit_root).expect("remove audit dir");
        std::fs::write(&audit_root, b"not a directory").expect("sabotage audit store");
        let snapshots_root = tmp.path().join("snapshots");

        let ledger = snapshot_post_recovery(
            (&sink, &audit),
            &snapshots_root,
            &mined(&[("alpha", alpha_replayed), ("beta", beta_replayed)]),
            &report,
        );

        assert!(
            std::fs::read_dir(&snapshots_root).is_err(),
            "the retained audit event skipped the write",
        );
        assert_eq!(
            ledger,
            vec![(TenantId::new("alpha"), restored)],
            "the ledger is what the untouched artefacts carry",
        );
    }

    /// RFC 0052 §3.2: a write that fails partway has replaced some
    /// artefacts and not others, so the ledger falls back to the restored
    /// horizons. Each tenant's folded horizon — the one it is written at —
    /// is at or above its restored one, so that is never above what is
    /// durable for any tenant. Lower is
    /// the safe direction: a lower floor only retains more WAL, while a
    /// horizon above the artefact on disk would let housekeeping unlink
    /// frames the next start must replay.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_partial_post_recovery_write_never_seeds_above_the_disk() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let [restored, alpha_replayed, beta_replayed] = offsets(&tmp.path().join("wal"));
        let snapshots_root = tmp.path().join("snapshots");
        let miner = mined(&[("alpha", alpha_replayed), ("beta", beta_replayed)]);
        let alpha = TenantId::new("alpha");
        let mut restored_state = miner.snapshot_state(&alpha);
        restored_state.wal_high_water = Some(ourios_ingester::snapshot_store::high_water(restored));
        ourios_ingester::snapshot_store::write(&snapshots_root, &alpha, &restored_state)
            .expect("alpha's restored artefact");
        // Beta, known to replay only, is written second; a non-empty
        // directory cannot be replaced by a rename, so its write fails.
        std::fs::create_dir(snapshots_root.join("beta.snap")).expect("block beta");
        std::fs::write(snapshots_root.join("beta.snap").join("occupied"), b"x").expect("occupy");

        let ledger = snapshot_post_recovery(
            (
                &buffered_sink(&tmp.path().join("store")),
                &audit_sink(&tmp.path().join("audit")),
            ),
            &snapshots_root,
            &miner,
            &restoring("alpha", restored, beta_replayed),
        );

        assert_eq!(
            disk_high_water(&snapshots_root, "alpha"),
            Some(alpha_replayed),
            "alpha, first, was replaced at its own replayed frame before beta failed",
        );
        assert_eq!(ledger, vec![(TenantId::new("alpha"), restored)]);
        assert!(
            restored < alpha_replayed,
            "below alpha's artefact, never above it"
        );
        assert!(
            !ledger.iter().any(|(tenant, _)| tenant.as_str() == "beta"),
            "beta has no durable artefact, so no horizon may cover its frames",
        );
    }

    /// Three synced offsets, in ascending order.
    fn offsets(wal_root: &Path) -> [WalOffset; 3] {
        let mut wal = Wal::open(test_wal_config(wal_root)).expect("open");
        let mut sync = |payload: &[u8]| {
            wal.append(ourios_wal::FrameKind::TenantOtlpBatch, payload)
                .expect("append");
            wal.sync().expect("sync")
        };
        [sync(b"first"), sync(b"second"), sync(b"third")]
    }

    /// A report that restored `tenant` at `restored` and replayed up to
    /// `replayed`.
    fn restoring(tenant: &str, restored: WalOffset, replayed: WalOffset) -> RecoveryReport {
        RecoveryReport {
            max_delivered: Some(replayed),
            tenants: vec![recovery::TenantRecovery {
                tenant_id: TenantId::new(tenant),
                outcome: ourios_miner::snapshot::RecoveryOutcome::Restored,
                stale_gap: false,
                horizon: Some(restored),
            }],
            ..RecoveryReport::default()
        }
    }

    /// A miner holding one mined line per tenant, each folded at that
    /// tenant's own replayed frame.
    fn mined(tenants: &[(&str, WalOffset)]) -> MinerCluster {
        let mut miner = MinerCluster::new(MinerConfig::default());
        for (tenant, frame) in tenants {
            let tenant = TenantId::new(*tenant);
            let request = export_request(tenant.as_str(), &["user 1 logged in"]);
            for record in ourios_ingester::receiver::assign(request, &tenant) {
                miner.ingest(&record);
            }
            miner.fold_through(&tenant, ourios_ingester::snapshot_store::high_water(*frame));
        }
        miner
    }

    /// The high-water mark `tenant`'s artefact carries on disk.
    fn disk_high_water(snapshots_root: &Path, tenant: &str) -> Option<WalOffset> {
        let bytes = std::fs::read(snapshots_root.join(format!("{tenant}.snap"))).expect("read");
        let (state, _) = ourios_miner::snapshot::recover(Some(&bytes));
        let high_water = state.expect("decodes").wal_high_water?;
        let segment = high_water.segment.parse().expect("segment uuid");
        Some(WalOffset {
            segment,
            byte: high_water.byte,
        })
    }

    /// Every `*.wal` segment directly under the WAL root.
    fn wal_segments(root: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(root)
            .expect("read the WAL root")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "wal"))
            .collect();
        out.sort();
        out
    }

    /// RFC 0047 §3.3: the graph emitter is attached to the coordinator the
    /// **barrier** holds. Attached to the sweep's clone alone — inside
    /// `spawn_cadences` — every batch a cut published would be missing from
    /// the authorization graph until a compaction re-derived its tuples.
    ///
    /// The cut is run explicitly rather than left to shutdown, so this fails
    /// for one reason only: the barrier's coordinator has no emitter.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_barriers_own_publish_feeds_the_graph() {
        use prost::Message;

        let writes = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let emitter = fake_graph_emitter(Arc::clone(&writes)).await;

        let wal_dir = tempfile::TempDir::new().expect("wal dir");
        let data_dir = tempfile::TempDir::new().expect("data dir");
        let store = Store::local(data_dir.path()).expect("local store");
        let handle = serve(ReceiverConfig {
            grpc_addr: "127.0.0.1:0".parse().expect("addr"),
            grpc_tls: None,
            http_addr: "127.0.0.1:0".parse().expect("addr"),
            http_tls: None,
            wal: WalConfig {
                segment_age_secs: 1, // the WAL's floor; slept past below
                ..test_wal_config(wal_dir.path())
            },
            store,
            promoted: PromotedAttributes::default(),
            auth: AuthResolver::static_only(None),
            graph_emitter: Some(emitter),
            encode_workers: 2,
            miner: MinerConfig::default(),
        })
        .await
        .expect("serve");

        // Given a cut left in the slot by the rotation hook.
        let export = export_request("checkout", &["user 1 logged in"]).encode_to_vec();
        post_otlp_http(handle.http_addr, &export).await;
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        let export = export_request("checkout", &["payment 9 settled"]).encode_to_vec();
        post_otlp_http(handle.http_addr, &export).await;
        assert!(
            handle.barrier.pending_mark().is_some(),
            "the request path filled the slot and no tick has run",
        );

        // When the barrier publishes it, the graph sees the partition's
        // tuples. Every published partition yields the tenant's tool tuples,
        // so this holds without conversation attributes in the records.
        tokio::task::block_in_place(|| handle.barrier.run_pending());
        let saw_tuples = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let seen = writes.lock().expect("lock").join("");
                if seen.contains("tool:checkout/query_logs") {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        handle.shutdown().await.expect("graceful shutdown");
        assert!(
            saw_tuples.is_ok(),
            "the cut's publish wrote tuples; the graph saw {:?}",
            writes.lock().expect("lock"),
        );
    }

    /// RFC 0047 §3.3, issue #834: a partition the **size trigger** takes
    /// feeds the graph too. It used to be written by the encode worker
    /// straight through the sink, which never reached the emitter; it now
    /// goes to the coordinator's publisher (RFC 0052 §3.1), which writes
    /// through the same feed as the cadence and the barrier.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_size_triggered_publish_feeds_the_graph() {
        let writes = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let emitter = fake_graph_emitter(Arc::clone(&writes)).await;
        let data_dir = tempfile::TempDir::new().expect("data dir");
        let store = Store::local(data_dir.path()).expect("local store");
        let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(
            store.clone(),
            AUDIT_SINK_CEILING_EVENTS,
        ));
        let barrier_audit = audit.clone();
        let sink = SharedParquetSink::new(
            ParquetRecordSink::new(
                store,
                FlushConfig {
                    target_bytes: 1, // every record crosses the size target
                    max_buffer_age: Duration::from_secs(86_400),
                    ceiling_bytes: usize::MAX,
                },
            )
            .with_audit_barrier(Box::new(move || barrier_audit.settled())),
        );
        let coordinator = PublishCoordinator::new(sink.clone(), audit).with_graph_emitter(emitter);
        let pool =
            ourios_ingester::encode_pool::EncodePool::with_publisher(coordinator.publisher(), 1);

        // Given a record whose emit crosses the size target.
        pool.submit(vec![rec()]);
        tokio::task::block_in_place(|| {
            pool.quiesce();
            let _outcomes = sink.quiesce_publishes();
        });
        assert_eq!(
            sink.flushes(),
            1,
            "the size trigger published the partition"
        );

        // Then the graph sees the partition's tuples, without any cut.
        let tenant = rec().tenant_id;
        let tool = format!("tool:{}/query_logs", tenant.as_str());
        let saw_tuples = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if writes.lock().expect("lock").join("").contains(&tool) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(
            saw_tuples.is_ok(),
            "the size-triggered publish wrote tuples; the graph saw {:?}",
            writes.lock().expect("lock"),
        );
    }

    /// An emitter bound to a fake `OpenFGA` that records every `/write` body.
    async fn fake_graph_emitter(
        writes: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Arc<ourios_ingester::graph_emitter::GraphEmitter> {
        use ourios_core::auth::openfga::{
            OpenFgaSpec, VisibilityObjectSpec, VisibilitySpec, build_openfga_config,
        };
        use ourios_ingester::graph_emitter::GraphEmitter;

        let api_url = serve_fake_graph(writes).await;
        let config = build_openfga_config(&OpenFgaSpec {
            api_url: Some(api_url),
            store_id: Some("s".to_string()),
            request_timeout_secs: Some("2".to_string()),
            visibility: VisibilitySpec {
                objects: vec![VisibilityObjectSpec {
                    object_type: Some("conversation".to_string()),
                    column: Some("attr.gen_ai.conversation.id".to_string()),
                }],
                ..VisibilitySpec::default()
            },
            ..OpenFgaSpec::default()
        })
        .expect("openfga config");
        Arc::new(
            GraphEmitter::from_config(&config)
                .expect("emitter")
                .expect("conversation bound"),
        )
    }

    /// A fake `OpenFGA` recording every `/write` body (the `graph_emitter`
    /// unit tests' `erase_fake`, write half only).
    async fn serve_fake_graph(writes: Arc<std::sync::Mutex<Vec<String>>>) -> String {
        use axum::Router;
        use axum::extract::State;
        use axum::routing::post;

        async fn write(
            State(writes): State<Arc<std::sync::Mutex<Vec<String>>>>,
            body: axum::body::Bytes,
        ) -> ([(&'static str, &'static str); 1], String) {
            writes
                .lock()
                .expect("lock")
                .push(String::from_utf8_lossy(&body).into_owned());
            ([("content-type", "application/json")], "{}".to_string())
        }

        let app = Router::new()
            .route("/stores/{store}/write", post(write))
            .with_state(writes);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        url
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

    // --- RFC0035.2 flush half, through the REAL rotation path
    // (`rotation_capture_hook` → the cut the barrier runs): a
    // buffered-but-unflushed record ≤ the mark is either published before
    // the stamp, or the stamp is skipped. The ingester-side barrier test
    // covers the drain half + inline-published records; these two arms
    // pin the buffered case against the production hook. ---

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
        ingest_and_quiesce(&pipeline, &["user 1 logged in"]).await;
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
        let rotation_point =
            ingest_and_quiesce(&pipeline, &["user 1 logged in", "user 2 logged in"]).await;
        assert_eq!(sink.buffered_records(), 2, "batch A is buffered, unflushed");

        // Rotation: the hook is now capture-only (RFC 0052 §3.1), so it
        // takes batch A out of the buffers into a cut and returns without
        // touching the store. Nothing is stamped yet — the invariant moves
        // from "by the time `ingest` returns" to "by the time the cut the
        // rotation handed over has run".
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        ingest_and_quiesce(&pipeline, &["payment 9 settled"]).await;
        assert_eq!(
            barrier.pending_mark(),
            Some(rotation_point),
            "the rotation handed the barrier a cut at the rotation point",
        );
        assert_no_snapshot_yet(&snapshots_root, "and stamped nothing on the request path");

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
        assert_snapshot_stamped_at(&snapshots_root, rotation_point);
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

    /// Ingest one batch for `checkout`, wait out its encodes, and return
    /// the turn's own durable offset — what a rotation fired by the next
    /// append would use as its mark.
    async fn ingest_and_quiesce(pipeline: &SharedPipeline, bodies: &[&str]) -> WalOffset {
        pipeline
            .ingest(
                export_request("checkout", bodies),
                ourios_core::tenant::TenantId::new("checkout"),
            )
            .await
            .expect("the batch acks");
        let mark = pipeline.last_durable().expect("durable after the batch");
        pipeline.quiesce_encodes();
        mark
    }

    /// No snapshot artefact has been installed yet.
    fn assert_no_snapshot_yet(snapshots_root: &Path, reason: &str) {
        let written = std::fs::read_dir(snapshots_root).is_ok_and(|mut d| d.next().is_some());
        assert!(!written, "{reason}");
    }

    /// Exactly one snapshot artefact exists and it carries `mark` as its
    /// WAL high-water — the "stamped, and stamped at the rotation point"
    /// half of RFC0035.2's invariant.
    fn assert_snapshot_stamped_at(snapshots_root: &Path, mark: WalOffset) {
        let artefacts =
            ourios_ingester::snapshot_store::load_all(snapshots_root).expect("load snapshots");
        assert_eq!(artefacts.len(), 1, "the rotation snapshot was stamped");
        let state = ourios_miner::snapshot::load_snapshot(&artefacts[0].1).expect("known version");
        let stamped = state.wal_high_water.expect("stamped with a horizon");
        assert_eq!(stamped.segment, mark.segment.to_string());
        assert_eq!(stamped.byte, mark.byte);
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
    /// slow S3 PUT opens. Mutation check: reverting the
    /// `quiesce_publishes` in `Barrier::run_cut` makes the cut stamp
    /// during the window and the mid-window assertion fail
    /// deterministically.
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
        let rotation_point =
            ingest_and_quiesce(&pipeline, &["user 1 logged in", "user 2 logged in"]).await;
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
        ingest_and_quiesce(&pipeline, &["payment 9 settled"]).await;
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
        assert_no_snapshot_yet(
            &snapshots_root,
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
        assert_snapshot_stamped_at(&snapshots_root, rotation_point);
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

        ingest_and_quiesce(&pipeline, &["user 1 logged in"]).await;
        assert_eq!(sink.buffered_records(), 1, "batch A is buffered, unflushed");

        // Sabotage the store: the cut's publish cannot land, so the
        // snapshot must be skipped — stamping would advance the horizon
        // past a record ≤ the mark that reached no Parquet object.
        std::fs::remove_dir_all(&store_root).expect("remove store dir");
        std::fs::write(&store_root, b"not a directory").expect("sabotage store");

        tokio::time::sleep(Duration::from_millis(1_200)).await;
        ingest_and_quiesce(&pipeline, &["payment 9 settled"]).await;

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
        assert_no_snapshot_yet(
            &snapshots_root,
            "the stamp is skipped while a record ≤ the mark reached no Parquet object",
        );
    }
}
