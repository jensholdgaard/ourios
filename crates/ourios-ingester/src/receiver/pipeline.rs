//! The ingest pipeline — the §6.5 WAL-before-ack business-logic layer.
//!
//! Composes the pieces the earlier slices built (decode → fan-out) with
//! durability and templating: a decoded `ExportLogsServiceRequest` is
//! fanned out per tenant, the whole export is appended as one
//! `FrameKind::OtlpBatch` frame and **fsync'd before any ack**
//! (`AGENTS.md` §3.4 / RFC0003.1), and only then are the records handed
//! to the miner (§6.5 step ordering). An empty batch takes the
//! fast path: success with no WAL write (RFC0003.12).
//!
//! Durability is delegated to the [`crate::receiver::commit::CommitCoordinator`]:
//! the per-request `append` + windowed group `sync` (RFC0008.8 batched
//! fsync) live there, so concurrent requests fold into one fsync per
//! window while each still acks only after a covering `sync` succeeds.
//! The pipeline owns the miner (behind a mutex) and the §6.9 rotation
//! hook; the coordinator owns the single-writer WAL.
//!
//! The live gRPC/HTTP transports wrap this layer; they hand it a decoded
//! request and map its `Result` to the transport-level response.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_core::otlp::OtlpLogRecord;
use ourios_core::record::MinedRecord;
use ourios_core::sync::{lock_recover, read_recover, write_recover};
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::MinerCluster;
use ourios_wal::WalOffset;
use tracing::Instrument as _;

pub use crate::receiver::admission::Export;
use crate::receiver::admission::{Admission, Framed};
use crate::receiver::commit::{CommitCoordinator, CommitOutcome};

pub(super) use super::error::IngestFailure;
pub use super::error::ReceiveError;
pub(crate) use super::error::{SpanError, span_error};
pub use super::journal::Journal;

/// The §6.9 rotation-cadence callback: receives the miner as it
/// stands and the rotation-point high-water mark. See
/// [`IngestPipeline::with_rotation_hook`].
pub type RotationHook = Box<dyn FnMut(&MinerCluster, WalOffset) + Send>;

/// The ingest pipeline shared across a listener's requests. Concurrency
/// is handled by the pipeline's own inner locks (the group-commit
/// coordinator serializes the single-writer WAL; the miner sits behind a
/// mutex), so the shared handle is a plain `Arc` — no outer mutex, so
/// concurrent requests batch their fsyncs (RFC0008.8) instead of
/// serializing end to end. Used by both the HTTP and gRPC transports.
pub type SharedPipeline = Arc<IngestPipeline>;

/// The ingester's WAL-before-ack ingest path. Owns the group-commit
/// [`CommitCoordinator`] (which owns the durability [`Journal`]), the
/// per-process `MinerCluster` (behind a mutex — the coordinator lets
/// requests run concurrently, so the miner needs its own serialization).
/// The tenant is chosen out of band per request (RFC 0046 §3.1) and handed
/// in by the transport; the pipeline never derives one.
///
/// Releases the in-order miner hand-off to `seq + 1` on drop — so the
/// gate advances on *every* exit from the post-durability region,
/// including a panic in the miner path (which would otherwise leave the
/// gate stuck and deadlock all later ingests).
struct IngestGateGuard<'a> {
    coordinator: &'a CommitCoordinator,
    seq: u64,
}

impl Drop for IngestGateGuard<'_> {
    fn drop(&mut self) {
        self.coordinator.complete_ingest(self.seq);
    }
}

/// Where the pipeline's durable high-water mark came from (RFC 0052
/// §3.7). Two variants rather than an offset plus a flag: only one of
/// them may be checkpointed, and a caller holding a bare `WalOffset`
/// cannot tell which it has.
#[derive(Clone, Copy, Debug)]
enum DurableMark {
    /// Recovery's seed — the highest offset replay *delivered*. It
    /// stamps the shutdown snapshot's high-water and is never a mark.
    Replayed(WalOffset),
    /// A turn's own frame offset, acknowledged in this process.
    Acknowledged(WalOffset),
}

impl DurableMark {
    fn offset(self) -> WalOffset {
        match self {
            Self::Replayed(offset) | Self::Acknowledged(offset) => offset,
        }
    }
}

/// No `Debug`: `MinerCluster` holds the per-tenant Drain trees and does
/// not implement it.
pub struct IngestPipeline {
    coordinator: Arc<CommitCoordinator>,
    miner: Mutex<MinerCluster>,
    /// RFC 0052 §3.1's **barrier exclusion**. Held shared across the
    /// span that matters — the miner work, `pool.submit`, and the
    /// `last_durable` update — by every ingest turn and by the age
    /// sweep's drain; taken exclusively by a cut capture, and for
    /// nothing else.
    ///
    /// Strictly wider than the miner lock and strictly narrower than the
    /// ingest gate, so it does not change ingest ordering. The miner
    /// lock alone is not enough: `ingest_bound` releases it *before*
    /// `pool.submit` and before advancing `last_durable`, so a timer
    /// holding only that lock could quiesce the pool and then have an
    /// already-past-the-lock ingest submit an encode and advance the
    /// mark underneath it.
    ///
    /// **Acquisition order is fixed**: exclusion, then miner lock, then
    /// the `last_durable` mutex. Taking the miner lock first and then
    /// waiting for the exclusion would deadlock against a capture
    /// holding the exclusion and waiting for the miner.
    ingest_bound: std::sync::RwLock<()>,
    /// The cadence state (§3.1). Adopted from the encode pool's sink
    /// when one is installed, so every party to a cut — the pool's batch
    /// guards, the sink's publish guards, the barrier task — reports
    /// into one latch.
    epochs: Arc<crate::cadence::BarrierEpochs>,
    /// The durable high-water mark after the most recent acked batch (or
    /// the startup seed). Behind a mutex: concurrent acks update it, and
    /// the rotation-detection read-then-write must see a consistent value.
    last_durable: Mutex<Option<DurableMark>>,
    rotation_hook: Mutex<Option<RotationHook>>,
    /// The binding check, the frame and the batch metrics, shared with
    /// the replicated path.
    admission: Admission,
    /// RFC 0035 §3.1: the concurrent encode pool. When present (every
    /// production wiring — the server role, the soak, the Parquet-sink
    /// integration tests), the gated section runs
    /// `MinerCluster::ingest_mined` (id assignment + audit only) and
    /// hands each batch's mined records to the pool for the sink emit.
    /// `None` — pipelines whose miner has no [`crate::record_sink::SharedParquetSink`]
    /// to hand a pool (spy/no-op-sink test pipelines) — keeps the fully
    /// synchronous `miner.ingest` path.
    encode_pool: Option<crate::encode_pool::EncodePool>,
}

impl IngestPipeline {
    /// Build a pipeline over a group-commit `coordinator` and a
    /// `MinerCluster`.
    #[must_use]
    pub fn new(coordinator: Arc<CommitCoordinator>, miner: MinerCluster) -> Self {
        Self {
            coordinator,
            miner: Mutex::new(miner),
            ingest_bound: std::sync::RwLock::new(()),
            epochs: Arc::new(crate::cadence::BarrierEpochs::new()),
            last_durable: Mutex::new(None),
            rotation_hook: Mutex::new(None),
            encode_pool: None,
            admission: Admission::new(),
        }
    }

    /// The cadence state every guard in this pipeline reports into
    /// (RFC 0052 §3.1).
    #[must_use]
    pub fn epochs(&self) -> Arc<crate::cadence::BarrierEpochs> {
        Arc::clone(&self.epochs)
    }

    /// Take the barrier exclusion in **shared** mode, then run `f`.
    ///
    /// The age sweep's drain uses this rather than [`Self::with_miner`]:
    /// taken inside `with_miner` the sweep would hold the miner lock
    /// while waiting for the shared exclusion, and a capture holding the
    /// exclusive lock would be waiting for the miner — both stuck. Held
    /// only across the drain and the in-flight registration, never
    /// across the off-lock publish (§3.1).
    pub fn with_bound_miner<R>(&self, f: impl FnOnce(&MinerCluster) -> R) -> R {
        let _bound = self.share_bound();
        f(&self.lock_miner())
    }

    /// Take the barrier exclusion in **exclusive** mode — a cut capture,
    /// and nothing else.
    pub(crate) fn exclude_ingest(&self) -> std::sync::RwLockWriteGuard<'_, ()> {
        write_recover(&self.ingest_bound)
    }

    fn share_bound(&self) -> std::sync::RwLockReadGuard<'_, ()> {
        read_recover(&self.ingest_bound)
    }

    /// Enable the RFC 0035 §3.1 ordered/concurrent ingest split: the
    /// gated section runs only Drain match + template-id assignment
    /// (+ audit), and the Parquet-sink emit runs on `pool`. The pool's
    /// sink must be the same sink the miner was built with, so the
    /// no-pool fallback, the flush triggers, and the rotation/shutdown
    /// drains all see one buffer.
    #[must_use]
    pub fn with_encode_pool(mut self, pool: crate::encode_pool::EncodePool) -> Self {
        // One latch across the pool's batch guards, the sink's publish
        // guards and this pipeline's cuts (RFC 0052 §3.1): the sink owns
        // it, the pool adopted it, and the pipeline adopts the pool's.
        self.epochs = pool.epochs();
        self.encode_pool = Some(pool);
        self
    }

    /// Drain the concurrent encode pool — blocks until every submitted
    /// record has reached the record sink. The drain half of the §3.1
    /// encode-drain-and-flush barrier: callers MUST run this before any
    /// flush/snapshot that claims to cover previously-acked records
    /// (the rotation path runs it internally; the server's shutdown and
    /// the soak's end-of-load measurement call it here). A no-op
    /// without a pool.
    pub fn quiesce_encodes(&self) {
        if let Some(pool) = &self.encode_pool {
            pool.quiesce();
        }
    }

    /// Install the RFC 0026 §3.4 denial audit sink: every tenant-binding
    /// rejection emits an `ingest_denied` event through it (the token's
    /// audit label + the offending tenant — never a token value).
    #[must_use]
    pub fn with_denial_audit_sink(
        self,
        sink: Box<dyn ourios_core::audit::AuditSink + Send>,
    ) -> Self {
        self.admission.set_denial_audit_sink(sink);
        self
    }

    /// Install the §6.9 rotation-cadence hook: called once per
    /// detected WAL segment rotation with the miner as it stands
    /// and the **rotation-point high-water mark** — the last
    /// durable offset in the just-closed segment. The hook runs
    /// *before* the rotating batch's records reach the miner, so a
    /// snapshot it takes reflects exactly the frames at or below
    /// that mark. The caller (the server role) wires this to the
    /// per-tenant snapshot writer; failures inside the hook are the
    /// hook's to handle — a snapshot is a rebuildable cache, never
    /// worth failing the ack over.
    #[must_use]
    pub fn with_rotation_hook(self, hook: RotationHook) -> Self {
        self.set_rotation_hook(hook);
        self
    }

    /// [`Self::with_rotation_hook`] after construction — the barrier
    /// owns the hook and is built from the same coordinator this
    /// pipeline holds, so it cannot exist before the pipeline does.
    pub fn set_rotation_hook(&self, hook: RotationHook) {
        *self.lock_hook() = Some(hook);
    }

    /// Seed the durable high-water mark from startup recovery
    /// (`RecoveryReport::max_delivered`). Without the seed, a process
    /// that serves zero requests writes shutdown snapshots with no
    /// high-water mark, forcing the next start to discard them and
    /// full-replay (RFC 0001 §6.9). A later commit offset supersedes the
    /// seed.
    #[must_use]
    pub fn with_last_durable(self, offset: Option<WalOffset>) -> Self {
        *self.lock_last_durable() = offset.map(DurableMark::Replayed);
        self
    }

    /// [`Self::ingest_bound`] without an authenticated binding — the open
    /// mode (RFC 0026 §3.1) and pre-auth call sites, byte-for-byte today's
    /// behavior.
    ///
    /// # Errors
    ///
    /// As [`Self::ingest_bound`], minus the binding rejection.
    pub async fn ingest(
        &self,
        request: ExportLogsServiceRequest,
        tenant: TenantId,
    ) -> Result<usize, ReceiveError> {
        self.ingest_bound(Export {
            request,
            tenant,
            binding: None,
            lenient_json: false,
        })
        .await
    }

    /// Ingest one decoded export under its out-of-band `tenant` (RFC 0046
    /// §3.1) per the §6.5 sequence: enforce the RFC 0026 §3.2 tenant
    /// binding (when the export carries one), materialise every record under
    /// `tenant`, append the export as a single `TenantOtlpBatch` frame,
    /// **fsync** (batched via the group-commit coordinator), then hand the
    /// records to the miner, then ack. Returns the number of records
    /// ingested (`0` for the empty fast path).
    ///
    /// The covering fsync completes before this returns `Ok`, so the
    /// caller never acks a batch that isn't durable (`[§3.4]`). `&self`
    /// (not `&mut`): concurrency is the inner locks' job, so concurrent
    /// requests batch into one window's fsync (RFC0008.8).
    ///
    /// # Errors
    ///
    /// - [`ReceiveError::TenantDenied`] if `tenant` falls outside the
    ///   binding's set — the whole batch is rejected before any WAL write,
    ///   with no partial success (RFC 0026 §3.2).
    /// - [`ReceiveError::WalAppend`] / [`ReceiveError::WalSync`] if
    ///   persistence fails; the batch is **not** acked.
    pub async fn ingest_bound(&self, export: Export<'_>) -> Result<usize, ReceiveError> {
        self.ingest_reported(export, span_error).await
    }

    /// [`Self::ingest_bound`], with the transport saying how a failure
    /// marks the `ingest logs` server span: `span_error` returns the
    /// `error.type` under that transport's convention, or `None` when its
    /// status is not a server error there (HTTP: only a `5xx`; gRPC: only
    /// the codes the gRPC server convention lists).
    ///
    /// # Errors
    ///
    /// As [`Self::ingest_bound`].
    // RFC 0038: one span per OTLP `Export` batch — the correct coarse
    // granularity (never per record; the miner/encode/sink loops below stay
    // span-free). Opened *inside* this callee, not at the `tokio::spawn` site
    // in the gRPC/HTTP handlers, so the span is created within the spawned task
    // (span context does not cross `tokio::spawn` — RFC0038.3). `skip_all`
    // keeps the whole `ExportLogsServiceRequest` off the span.
    #[tracing::instrument(
        skip_all,
        name = "ingest logs",
        fields(
            otel.kind = "server",
            ourios.tenant = export.tenant.as_str(),
            ourios.ingest.batch.record_count = log_record_count(&export.request),
            ourios.ingest.batch.size = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        )
    )]
    pub(crate) async fn ingest_reported(
        &self,
        export: Export<'_>,
        span_error: SpanError,
    ) -> Result<usize, ReceiveError> {
        let outcome = self.ingest_batch(export).await;
        if let Err(error) = &outcome
            && let Some(error_type) = span_error(error)
        {
            ourios_core::telemetry::record_span_error(&tracing::Span::current(), &error_type);
        }
        outcome
    }

    /// The WAL group-commit as the batch's `commit wal` child span. An
    /// internal span, so any failed commit marks it `Error`, an oversize
    /// frame included; the batch span above applies the server-span rule.
    async fn commit_traced(&self, payload: &[u8]) -> CommitOutcome {
        let span = tracing::info_span!(
            "commit wal",
            otel.kind = "internal",
            otel.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        );
        let outcome = self
            .coordinator
            .commit(payload)
            .instrument(span.clone())
            .await;
        if let Err(e) = &outcome.result {
            ourios_core::telemetry::record_span_error(
                &span,
                IngestFailure::classify(e).error_type(),
            );
        }
        outcome
    }

    /// [`Self::ingest_bound`]'s work, inside its span.
    async fn ingest_batch(&self, export: Export<'_>) -> Result<usize, ReceiveError> {
        let Export {
            request,
            tenant,
            binding,
            lenient_json,
        } = export;
        // RFC 0026 §3.2: authz precedes every other ingest step — a denied
        // batch does no encode, materialisation, or WAL work.
        self.admission.enforce_binding(&tenant, binding)?;
        // Steps 1–3: every record under the selected tenant (RFC 0046 §3.2),
        // and the frame.
        let framed = Framed::of(request, &tenant)?;
        let records = &framed.records;

        // Empty fast path (RFC0003.12): no records → success, no WAL
        // frame, miner untouched.
        if records.is_empty() {
            return Ok(0);
        }

        // Steps 3–4: append the export as one TenantOtlpBatch frame and await
        // its (batched) fsync — concurrent requests fold into one window's
        // fsync (RFC0008.8). The frame's append `seq` orders the miner
        // hand-off below.
        let commit_start = Instant::now();
        // RFC 0038: a child span for the WAL group-commit — the one genuinely
        // I/O-bound, latency-bearing step (a durationful op is a span, not an
        // event). Nests under the batch span; `.instrument` (not an entered
        // guard) carries it across the await cleanly.
        let outcome = self.commit_traced(&framed.payload).await;
        // The WAL-before-ack latency: time until the batch is durable
        // (includes the group-commit window + fsync). Recorded below only
        // on a successful, acked commit.
        let append_elapsed = commit_start.elapsed();
        let Some(seq) = outcome.seq else {
            // The append itself failed: no sequence consumed, so this
            // request is not part of the WAL order and never reaches the
            // miner gate. Not acked (§3.4). (`result` is the append error;
            // the `Ok` arm is unreachable without a seq — map it to 0.)
            return outcome.result.map(|_| 0);
        };

        // Step 5: hand the records to the miner **in WAL-append order**.
        // The fsyncs batched concurrently, but the miner must see records
        // in `seq` order — template ids are assigned first-seen, so an
        // out-of-order live tree wouldn't match a WAL-order replay
        // (snapshot-restore §3.5.3). `await_ingest_turn` blocks until every
        // lower seq has finished here, so this whole region runs
        // single-file in append order; `complete_ingest` releases the next
        // seq and MUST run even on a sync failure (which ingests nothing)
        // so the gate never stalls.
        self.coordinator.await_ingest_turn(seq).await;
        // Release the gate to `seq + 1` on *every* exit from here —
        // including a panic in the miner path (the miner has `expect`s on
        // its invariants). Without this a panic would leave the gate stuck
        // at `seq` and deadlock all later ingests; the guard's `Drop` runs
        // during unwinding.
        let _gate = IngestGateGuard {
            coordinator: &self.coordinator,
            seq,
        };
        // RFC 0052 §3.1: the mark is this turn's **own** frame offset,
        // never the sync's reported EOF — a flush spanning two turns
        // reports an EOF that already covers the later frame, whose own
        // turn has not run, and a checkpoint at that EOF would stamp
        // across a frame nothing had mined. `own` is `Some` whenever
        // `seq` is, so the `else` arm is unreachable; treating it as a
        // sync failure keeps the invariant rather than inventing a mark.
        let ack = match (outcome.result, outcome.own) {
            (Ok(_), None) => Err(ReceiveError::WalSync(ourios_wal::SyncError::Io {
                op: "group-commit sync",
                source: std::io::Error::other("commit reported no frame offset"),
            })),
            (Ok(_), Some(now)) => {
                // RFC 0052 §3.1: the barrier exclusion spans the miner
                // work, the pool submission and the `last_durable`
                // update — the whole region a cut must not straddle.
                // Taken **before** the miner lock, and never requested
                // while holding it.
                let _bound = self.share_bound();
                let mined = self.fold_turn(&tenant, records, now);
                if let Some(pool) = &self.encode_pool {
                    // Still under the ingest gate (`_gate` drops at
                    // return): every frame ≤ this seq submits its encodes
                    // before any later seq reaches the rotation check
                    // above — the ordering that makes the whole-pool
                    // drain cover the mark. A full queue blocks here
                    // (bounded backpressure) on a runtime worker, like
                    // the hook's own blocking store I/O on this path.
                    pool.submit(mined);
                }
                // Only successful commits advance the durable mark, so the
                // snapshot high-water never passes a failed sync — its tail
                // replay re-covers those frames (no §3.5.3 divergence).
                *self.lock_last_durable() = Some(DurableMark::Acknowledged(now));
                // Throughput + WAL-before-ack latency for this acked batch.
                self.admission.metrics().record_batch(&framed.accepted(
                    &tenant,
                    lenient_json,
                    append_elapsed,
                ));
                Ok(records.len())
            }
            // Sync failed: the frame is not durable and not acked; it
            // reaches neither the miner nor the durable mark.
            (Err(e), _) => Err(e),
        };
        // `_gate` releases the hand-off to `seq + 1` as it drops here.
        ack
    }

    /// One acknowledged turn's miner step, under the exclusion the caller
    /// holds: the rotation check, the mining, and the fold of this
    /// tenant's horizon to `now`.
    fn fold_turn(
        &self,
        tenant: &TenantId,
        records: &[OtlpLogRecord],
        now: WalOffset,
    ) -> Vec<MinedRecord> {
        // §6.9 rotation cadence: a segment change since the prior
        // (now strictly-previous) durable mark means the WAL rotated
        // under this batch; fire the hook with the rotation-point
        // high-water mark BEFORE this batch's records reach the miner,
        // so a snapshot it takes reflects exactly the frames at or below
        // that mark. In-order, so `before` is exactly the preceding
        // seq's offset and no higher seq has ingested yet.
        //
        // The mark stays typed to here (RFC 0052 §3.7): the hook is a
        // barrier capture that checkpoints at `prev`, and a replay seed
        // is proof of *delivery*, not of a completed group sync.
        // Rotating on one — the first acknowledged turn after a restart
        // landing in a new segment, because the reopened one was full or
        // had aged out — would let the WAL forget a frame no turn in
        // this process acknowledged. The barrier's own timer covers that
        // replayed tail at the next cut, under a mark that is one.
        let before = *self.lock_last_durable();
        let mut miner = self.lock_miner();
        if let Some(DurableMark::Acknowledged(prev)) = before
            && prev.segment != now.segment
        {
            self.rotate_for_segment_change(&miner, prev);
        }
        let encodes = self.mine_batch_ordered(&mut miner, records);
        // RFC 0052 §3.1: before `last_durable` moves and under the
        // exclusion, so a cut never sees a horizon ahead of its mark or
        // of the state it snapshots.
        miner.fold_through(tenant, crate::snapshot_store::high_water(now));
        encodes
    }

    /// The RFC 0035 §3.1 **ordered phase**, verbatim from the pre-split
    /// `ingest_bound` body: id assignment + audit under the ingest gate
    /// and the miner lock; the sink emit is deferred to the concurrent
    /// pool (the caller submits the returned records). Without a pool,
    /// mining emits inline and returns nothing to submit.
    fn mine_batch_ordered(
        &self,
        miner: &mut MinerCluster,
        records: &[OtlpLogRecord],
    ) -> Vec<MinedRecord> {
        if let Some(pool) = &self.encode_pool {
            let mut out = Vec::with_capacity(records.len());
            // The whole batch is already WAL-durable, so a
            // miner panic mid-batch must not drop the records
            // mined before it — pre-split they had been
            // emitted inline as they were mined; deferring
            // the emit must not widen that failure into
            // whole-batch loss-until-restart-replay. Submit
            // what was mined, then let the panic continue
            // (the gate guard still releases, and `ingest`'s
            // caller still fails the request).
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                for record in records {
                    let (_, rec) = miner.ingest_mined(record);
                    out.extend(rec);
                }
            }));
            if let Err(panic) = outcome {
                pool.submit(out);
                std::panic::resume_unwind(panic);
            }
            out
        } else {
            for record in records {
                miner.ingest(record);
            }
            Vec::new()
        }
    }

    /// The §6.9 rotation-cadence step, verbatim from the pre-split
    /// `ingest_bound` body: fires only on a segment change, and runs
    /// **under the miner lock and the ingest gate** — the caller holds
    /// both, which is what makes the whole-pool drain cover the mark.
    fn rotate_for_segment_change(&self, miner: &MinerCluster, prev: WalOffset) {
        // RFC 0035 §3.1 encode-drain-and-flush barrier,
        // drain half. The rotation hook stamps
        // `wal_high_water = prev`, which asserts every
        // frame ≤ prev is durably captured — so no encode
        // for a frame ≤ prev may still be in flight when
        // it runs. Draining the *whole* pool over-covers
        // that mark soundly: submission happens under
        // this same in-order gate (below), so every
        // frame < this seq has already submitted its
        // encodes, no frame ≥ this seq has, and thus
        // every in-flight encode is for a frame ≤ prev.
        // The flush half is the hook's own `flush_all` +
        // its snapshot-only-if-fully-drained gate (the
        // server's `flush_then_snapshot`): after this
        // drain, every record ≤ prev has completed its
        // emit, so any of them still *buffered* is in the
        // sink for `flush_all` to take, and the
        // high-water is stamped only when both sinks
        // fully drained. The one class this pool barrier
        // does not cover — a record ≤ prev that an
        // age-sweep drained out of the buffers and is
        // writing off-lock while the hook runs (neither
        // buffered nor yet durable) — is the *publish*
        // half of the barrier (issue #578): each sweep
        // drain holds the sink's in-flight publish guard,
        // and `flush_then_snapshot` quiesces those
        // publishes under this same miner lock before it
        // flushes and stamps. The class-by-class
        // coverage argument lives there. A crash at any
        // point before the stamp loses only buffered
        // Parquet, which WAL replay above the (previous)
        // high-water re-mines (the §6.9 posture: the WAL
        // is the durability of record; a snapshot is a
        // rebuildable cache).
        //
        // The drain can wait out seconds of queued
        // encodes and the hook does blocking store I/O,
        // all on a runtime worker — `block_in_place`
        // lets the runtime relocate other tasks off
        // this worker meanwhile. Flavor-guarded:
        // `block_in_place` panics on a current-thread
        // runtime (the crash fixtures run one; they
        // never rotate, but a panic here would turn a
        // rotation into an outage).
        let drain_and_hook = || {
            self.quiesce_encodes();
            self.fire_rotation_hook(miner, prev);
        };
        if tokio::runtime::Handle::current().runtime_flavor()
            == tokio::runtime::RuntimeFlavor::MultiThread
        {
            tokio::task::block_in_place(drain_and_hook);
        } else {
            drain_and_hook();
        }
    }

    /// Fire the rotation hook over the current miner, swallowing a panic.
    fn fire_rotation_hook(&self, miner: &MinerCluster, mark: WalOffset) {
        let mut hook = self.lock_hook();
        let Some(hook) = hook.as_mut() else {
            return;
        };
        // A hook panic must not unwind `ingest`: the batch is already
        // durable and the unwind would poison the shared miner mutex,
        // halting all future ingestion over a best-effort cache write.
        // `AssertUnwindSafe` is sound here — the hook sees the miner
        // through `&MinerCluster`, so no pipeline mutation can be torn
        // mid-panic; the hook's own captures are its to keep consistent
        // (it stays installed and is only ever invoked best-effort).
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            hook(miner, mark);
        }));
        if outcome.is_err() {
            eprintln!(
                "rotation hook panicked; continuing — the snapshot \
                 is a rebuildable cache (recovery falls back to the WAL)"
            );
        }
    }

    /// Run `f` against the pipeline's miner under the miner lock — for
    /// the shutdown-snapshot path (which needs `&MinerCluster`) and
    /// tests.
    pub fn with_miner<R>(&self, f: impl FnOnce(&MinerCluster) -> R) -> R {
        f(&self.lock_miner())
    }

    /// The journal's durable high-water offset after the most recent
    /// acked batch, when known — what a snapshot taken now records as
    /// its WAL high-water mark (RFC 0001 §6.9).
    #[must_use]
    pub fn last_durable(&self) -> Option<WalOffset> {
        self.lock_last_durable().map(DurableMark::offset)
    }

    /// The mark a cut may stamp: the highest offset a turn **in this
    /// process** acknowledged.
    ///
    /// RFC 0052 §3.7: recovery's seed is the highest offset *replay
    /// delivered*, which is not proof of acknowledgement — a frame's
    /// bytes can survive a crash whose group sync never completed, and
    /// §3.1's rule is that such a frame is never a mark. The seed still
    /// stamps the shutdown snapshot's high-water (RFC 0001 §6.9), which
    /// is a different consumer: a snapshot only bounds replay depth,
    /// while a checkpoint lets the WAL forget. So a node that has served
    /// nothing since its restart has no mark, and its first successful
    /// turn establishes one.
    #[must_use]
    pub fn acknowledged_durable(&self) -> Option<WalOffset> {
        match *self.lock_last_durable() {
            Some(DurableMark::Acknowledged(offset)) => Some(offset),
            Some(DurableMark::Replayed(_)) | None => None,
        }
    }

    fn lock_miner(&self) -> std::sync::MutexGuard<'_, MinerCluster> {
        lock_recover(&self.miner)
    }

    fn lock_last_durable(&self) -> std::sync::MutexGuard<'_, Option<DurableMark>> {
        lock_recover(&self.last_durable)
    }

    fn lock_hook(&self) -> std::sync::MutexGuard<'_, Option<RotationHook>> {
        lock_recover(&self.rotation_hook)
    }
}

/// Log records across every `ResourceLogs` and `ScopeLogs` of an export,
/// counted before the request is consumed so the `ingest logs` span carries
/// it from its start (RFC 0038 §3.5).
pub(crate) fn log_record_count(request: &ExportLogsServiceRequest) -> i64 {
    span_count(
        request
            .resource_logs
            .iter()
            .flat_map(|resource| &resource.scope_logs)
            .map(|scope| scope.log_records.len())
            .sum(),
    )
}

/// A count as a span attribute value: `i64`, since tracing-opentelemetry
/// renders a `u64` field as a string.
pub(super) fn span_count(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::*;
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use ourios_config::MinerConfig;

    /// Persists nothing; `sync` reports the configured offset and counts
    /// its calls, so a test can assert WAL-before-ack ordering and
    /// per-batch (not per-record) fsync.
    struct FixedSyncJournal {
        offset: WalOffset,
        appends: Arc<AtomicU64>,
        syncs: Arc<AtomicU64>,
    }

    impl Journal for FixedSyncJournal {
        fn append_batch(&mut self, _payload: &[u8]) -> Result<WalOffset, ReceiveError> {
            self.appends.fetch_add(1, Ordering::SeqCst);
            Ok(self.offset)
        }

        fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
            self.syncs.fetch_add(1, Ordering::SeqCst);
            Ok(self.offset)
        }

        fn unflushed_bytes(&self) -> u64 {
            0
        }
    }

    fn pipeline_with(
        sync_offset: WalOffset,
        appends: Arc<AtomicU64>,
        syncs: Arc<AtomicU64>,
    ) -> IngestPipeline {
        let coordinator = CommitCoordinator::new(
            Box::new(FixedSyncJournal {
                offset: sync_offset,
                appends,
                syncs,
            }),
            Duration::from_millis(5),
            u64::MAX,
        );
        IngestPipeline::new(coordinator, MinerCluster::new(MinerConfig::default()))
    }

    fn pipeline(sync_offset: WalOffset) -> IngestPipeline {
        pipeline_with(
            sync_offset,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        )
    }

    fn offset(byte: u64) -> WalOffset {
        WalOffset {
            segment: uuid::Uuid::from_u128(1),
            byte,
        }
    }

    fn string_value(s: &str) -> AnyValue {
        AnyValue {
            value: Some(Value::StringValue(s.to_owned())),
        }
    }

    fn request() -> ExportLogsServiceRequest {
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![KeyValue {
                        key: "service.name".to_owned(),
                        value: Some(string_value("checkout")),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                scope_logs: vec![ScopeLogs {
                    log_records: vec![LogRecord {
                        body: Some(string_value("user 1 logged in")),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_synced_batch_advances_the_durable_mark() {
        let seed = offset(10);
        let synced = offset(64);
        let pipeline = pipeline(synced).with_last_durable(Some(seed));

        assert_eq!(pipeline.last_durable(), Some(seed));
        pipeline
            .ingest(request(), ourios_core::tenant::TenantId::new("checkout"))
            .await
            .expect("ingest");
        assert_eq!(pipeline.last_durable(), Some(synced));
    }

    /// `append_batch` reports each frame's own offset from a queue, so a
    /// segment change can be staged mid-sequence. RFC 0052 §3.1 makes
    /// that offset — not the sync's EOF — the rotation-detection input,
    /// so the queue is drained per append and `sync` echoes the last one.
    struct SequenceJournal {
        offsets: Mutex<Vec<WalOffset>>,
        last: Mutex<Option<WalOffset>>,
    }

    impl Journal for SequenceJournal {
        fn append_batch(&mut self, _payload: &[u8]) -> Result<WalOffset, ReceiveError> {
            let next = self.offsets.lock().expect("offsets").remove(0);
            *self.last.lock().expect("last") = Some(next);
            Ok(next)
        }

        fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
            self.last.lock().expect("last").ok_or_else(|| {
                ReceiveError::WalSync(ourios_wal::SyncError::Io {
                    op: "fdatasync",
                    source: std::io::Error::other("nothing appended"),
                })
            })
        }

        fn unflushed_bytes(&self) -> u64 {
            0
        }
    }

    fn sequence_pipeline(offsets: Vec<WalOffset>, hook: RotationHook) -> IngestPipeline {
        let coordinator = CommitCoordinator::new(
            Box::new(SequenceJournal {
                offsets: Mutex::new(offsets),
                last: Mutex::new(None),
            }),
            Duration::from_millis(5),
            u64::MAX,
        );
        IngestPipeline::new(coordinator, MinerCluster::new(MinerConfig::default()))
            .with_rotation_hook(hook)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rotation_hook_fires_once_with_the_old_segments_last_durable_offset() {
        let in_first = WalOffset {
            segment: uuid::Uuid::from_u128(1),
            byte: 100,
        };
        let in_second = WalOffset {
            segment: uuid::Uuid::from_u128(2),
            byte: 40,
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let pipeline = sequence_pipeline(
            vec![in_first, in_second, in_second],
            Box::new(move |miner, mark| {
                // Capture the miner's template count at hook time: the
                // rotating batch must not have reached it yet.
                let count = miner.template_count(&ourios_core::tenant::TenantId::new("checkout"));
                seen.lock().expect("lock").push((mark, count));
            }),
        );

        // Batch 1: no previous durable offset — never a rotation.
        pipeline
            .ingest(request(), ourios_core::tenant::TenantId::new("checkout"))
            .await
            .expect("batch 1");
        assert!(calls.lock().expect("lock").is_empty());

        // Batch 2: segment changed — the hook fires once with the OLD
        // segment's last durable offset, before batch 2 hits the miner
        // (the template count is still batch 1's).
        pipeline
            .ingest(request(), ourios_core::tenant::TenantId::new("checkout"))
            .await
            .expect("batch 2");
        assert_eq!(*calls.lock().expect("lock"), vec![(in_first, 1)]);

        // Batch 3: same segment — no further firing.
        pipeline
            .ingest(request(), ourios_core::tenant::TenantId::new("checkout"))
            .await
            .expect("batch 3");
        assert_eq!(calls.lock().expect("lock").len(), 1);
    }

    /// RFC 0052 §3.1: every acknowledged turn folds its own tenant's
    /// horizon to the turn's own frame, and no other tenant's.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_acknowledged_turn_folds_only_its_own_tenant() {
        use ourios_core::tenant::TenantId;

        let pipeline = sequence_pipeline(
            vec![offset(10), offset(20), offset(30)],
            Box::new(|_, _| {}),
        );
        for tenant in ["idle", "busy", "busy"] {
            pipeline
                .ingest(request(), TenantId::new(tenant))
                .await
                .expect("ingest");
        }

        let folded = |tenant: &str| {
            pipeline.with_miner(|miner| {
                miner
                    .folded_horizon(&TenantId::new(tenant))
                    .and_then(crate::snapshot_store::offset_of)
            })
        };
        assert_eq!(folded("idle"), Some(offset(10)));
        assert_eq!(folded("busy"), Some(offset(30)));
    }

    /// RFC 0052 §3.7: a replay seed is never a checkpoint mark, and the
    /// rotation hook is a barrier capture that checkpoints at the mark it
    /// is handed — so the seed must not reach it.
    ///
    /// The trigger is narrow and entirely ordinary: the process restarts,
    /// and the first acknowledged turn after replay lands in a new
    /// segment because the reopened one was full or had aged out. Firing
    /// there would let the WAL forget frames whose group sync may never
    /// have completed. The next turn's own offset *is* a mark, and the
    /// hook fires then — the rotation is deferred, not lost.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_replay_seed_never_reaches_the_rotation_hook() {
        let replayed = WalOffset {
            segment: uuid::Uuid::from_u128(1),
            byte: 100,
        };
        let in_second = WalOffset {
            segment: uuid::Uuid::from_u128(2),
            byte: 40,
        };
        let in_third = WalOffset {
            segment: uuid::Uuid::from_u128(3),
            byte: 12,
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let pipeline = sequence_pipeline(
            vec![in_second, in_third],
            Box::new(move |_, mark| seen.lock().expect("lock").push(mark)),
        )
        .with_last_durable(Some(replayed));

        // Batch 1 lands in a different segment than the seed — a segment
        // change, but the prior mark is `Replayed`, so no capture.
        pipeline
            .ingest(request(), ourios_core::tenant::TenantId::new("checkout"))
            .await
            .expect("batch 1");
        assert!(
            calls.lock().expect("lock").is_empty(),
            "the seed's segment change must not fire a capture that would \
             checkpoint at a frame no turn in this process acknowledged",
        );

        // Batch 2 rotates again, and now the prior mark is batch 1's own
        // acknowledged offset — a real mark, so the hook fires at it.
        pipeline
            .ingest(request(), ourios_core::tenant::TenantId::new("checkout"))
            .await
            .expect("batch 2");
        assert_eq!(
            *calls.lock().expect("lock"),
            vec![in_second],
            "and the next rotation carries the acknowledged offset, never the seed",
        );
    }

    /// The rotation branch on a `current_thread` runtime takes the
    /// inline path (`block_in_place` would panic there — the flavor
    /// guard is what this pins): the hook still fires exactly once
    /// with the old segment's offset.
    #[tokio::test(flavor = "current_thread")]
    async fn rotation_completes_inline_on_a_current_thread_runtime() {
        let in_first = WalOffset {
            segment: uuid::Uuid::from_u128(1),
            byte: 100,
        };
        let in_second = WalOffset {
            segment: uuid::Uuid::from_u128(2),
            byte: 40,
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let pipeline = sequence_pipeline(
            vec![in_first, in_second],
            Box::new(move |_, mark| seen.lock().expect("lock").push(mark)),
        );
        pipeline
            .ingest(request(), ourios_core::tenant::TenantId::new("checkout"))
            .await
            .expect("batch 1");
        pipeline
            .ingest(request(), ourios_core::tenant::TenantId::new("checkout"))
            .await
            .expect("batch 2");
        assert_eq!(*calls.lock().expect("lock"), vec![in_first]);
    }

    /// A panicking hook must not unwind `ingest`: the batch is
    /// already durable, and the unwind would poison the shared
    /// miner mutex and halt all future ingestion over a
    /// best-effort cache write.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rotation_hook_panic_does_not_fail_the_ingest() {
        let in_first = WalOffset {
            segment: uuid::Uuid::from_u128(1),
            byte: 100,
        };
        let in_second = WalOffset {
            segment: uuid::Uuid::from_u128(2),
            byte: 40,
        };
        let pipeline = sequence_pipeline(
            vec![in_first, in_second, in_second],
            Box::new(|_, _| panic!("snapshot writer blew up")),
        );

        pipeline
            .ingest(request(), ourios_core::tenant::TenantId::new("checkout"))
            .await
            .expect("batch 1");
        // Batch 2 rotates and the hook panics — the ingest still acks
        // and the records still reach the miner.
        assert_eq!(
            pipeline
                .ingest(request(), ourios_core::tenant::TenantId::new("checkout"))
                .await
                .expect("batch 2 acks"),
            1
        );
        assert_eq!(
            pipeline.with_miner(|m| {
                m.template_count(&ourios_core::tenant::TenantId::new("checkout"))
            }),
            1,
            "the rotating batch's records reached the miner despite the panic",
        );
        // The pipeline stays usable afterwards.
        assert_eq!(
            pipeline
                .ingest(request(), ourios_core::tenant::TenantId::new("checkout"))
                .await
                .expect("batch 3 acks"),
            1
        );
    }
}
