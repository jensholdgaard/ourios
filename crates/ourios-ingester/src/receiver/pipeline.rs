//! The ingest pipeline — the §6.5 WAL-before-ack business-logic layer.
//!
//! Composes the pieces the earlier slices built (decode → fan-out) with
//! durability and templating: a decoded `ExportLogsServiceRequest` is
//! fanned out per tenant, the whole export is appended as one
//! `FrameKind::OtlpBatch` frame and **fsync'd before any ack**
//! (`CLAUDE.md` §3.4 / RFC0003.1), and only then are the records handed
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
use ourios_miner::cluster::MinerCluster;
use ourios_wal::{FrameKind, Wal, WalOffset};
use prost::Message;

use crate::metrics::IngestMetrics;
use crate::receiver::commit::CommitCoordinator;
use crate::receiver::tenant::{TenantResolutionError, TenantRule, fan_out};

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

/// The durability sink the pipeline appends to and fsyncs through.
///
/// Abstracted (rather than a concrete `Wal`) so the WAL-before-ack
/// ordering can be exercised with a spy that records / counts the
/// `append`/`sync` calls and can be made to fail (RFC0003.1/.12,
/// RFC0008.8 per §8). The only production implementation is [`Wal`].
///
/// `Send` so it can live behind the coordinator's `Mutex<Box<dyn Journal>>`
/// as shared state in the async HTTP/gRPC listeners.
pub trait Journal: Send {
    /// Append one `OtlpBatch` frame carrying `payload` (not yet durable).
    ///
    /// # Errors
    ///
    /// [`ReceiveError::WalAppend`] on a persistence failure.
    fn append_batch(&mut self, payload: &[u8]) -> Result<(), ReceiveError>;

    /// Fsync — appended frames are durable when this returns `Ok`,
    /// yielding the durable high-water offset. The real WAL always has
    /// one (RFC 0008 §6.3); the group-commit coordinator compares it
    /// against waiters and the snapshot writer records it as the
    /// snapshot's WAL high-water mark (RFC 0001 §6.9).
    ///
    /// # Errors
    ///
    /// [`ReceiveError::WalSync`] on an fsync failure.
    fn sync(&mut self) -> Result<WalOffset, ReceiveError>;

    /// The WAL's current unflushed-bytes counter (RFC 0008 §6.8), read
    /// by the coordinator's segment-fill early cut ("until the segment
    /// fills", §3.4). Cheap — an in-memory counter, no syscall.
    fn unflushed_bytes(&self) -> u64;
}

impl Journal for Wal {
    fn append_batch(&mut self, payload: &[u8]) -> Result<(), ReceiveError> {
        Wal::append(self, FrameKind::OtlpBatch, payload)
            .map(|_| ())
            .map_err(ReceiveError::WalAppend)
    }

    fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
        Wal::sync(self).map_err(ReceiveError::WalSync)
    }

    fn unflushed_bytes(&self) -> u64 {
        self.metrics().unflushed_bytes
    }
}

/// The ingester's WAL-before-ack ingest path. Owns the group-commit
/// [`CommitCoordinator`] (which owns the durability [`Journal`]), the
/// per-process `MinerCluster` (behind a mutex — the coordinator lets
/// requests run concurrently, so the miner needs its own serialization),
/// and the tenant-derivation `TenantRule`.
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

/// No `Debug`: `MinerCluster` holds the per-tenant Drain trees and does
/// not implement it.
pub struct IngestPipeline {
    coordinator: Arc<CommitCoordinator>,
    miner: Mutex<MinerCluster>,
    rule: TenantRule,
    /// The durable high-water mark after the most recent acked batch (or
    /// the startup seed). Behind a mutex: concurrent acks update it, and
    /// the rotation-detection read-then-write must see a consistent value.
    last_durable: Mutex<Option<WalOffset>>,
    rotation_hook: Mutex<Option<RotationHook>>,
    /// Ingest throughput + WAL-before-ack latency instruments (RFC 0014
    /// §6.3), recorded on durably-acked batches — plus the RFC 0026 §3.4
    /// rejection counts (`error.type` on the batches counter), recorded
    /// on the pre-WAL denial paths.
    metrics: IngestMetrics,
    /// RFC 0035 §3.1: the concurrent encode pool. When present (every
    /// production wiring — the server role, the soak, the Parquet-sink
    /// integration tests), the gated section runs
    /// `MinerCluster::ingest_mined` (id assignment + audit only) and
    /// hands each batch's mined records to the pool for the sink emit.
    /// `None` — pipelines whose miner has no [`crate::record_sink::SharedParquetSink`]
    /// to hand a pool (spy/no-op-sink test pipelines) — keeps the fully
    /// synchronous `miner.ingest` path.
    encode_pool: Option<crate::encode_pool::EncodePool>,
    /// RFC 0026 §3.4: the sink for `ingest_denied` audit events. Behind a
    /// mutex — denials are the cold path.
    ///
    /// **Best-effort durability, deliberately.** The server wires the
    /// buffering audit sink, so a crash before its next cadence flush can
    /// drop denial events — and unlike template events they have no WAL
    /// replay to recover from (the denied batch never reached the WAL,
    /// which is the §3.2 point). The durable alerting signal is the
    /// `error.type = permission_denied` counter; the event is forensic
    /// detail. Making denials synchronously durable would put an fsync on
    /// the rejection path — a write-amplification lever for any
    /// authenticated-but-misconfigured (or hostile) sender.
    denial_audit: Mutex<Option<Box<dyn ourios_core::audit::AuditSink + Send>>>,
}

impl IngestPipeline {
    /// Build a pipeline over a group-commit `coordinator`, a
    /// `MinerCluster`, and a tenant-derivation rule.
    #[must_use]
    pub fn new(coordinator: Arc<CommitCoordinator>, miner: MinerCluster, rule: TenantRule) -> Self {
        Self {
            coordinator,
            miner: Mutex::new(miner),
            rule,
            last_durable: Mutex::new(None),
            rotation_hook: Mutex::new(None),
            encode_pool: None,
            metrics: IngestMetrics::new(),
            denial_audit: Mutex::new(None),
        }
    }

    /// Enable the RFC 0035 §3.1 ordered/concurrent ingest split: the
    /// gated section runs only Drain match + template-id assignment
    /// (+ audit), and the Parquet-sink emit runs on `pool`. The pool's
    /// sink must be the same sink the miner was built with, so the
    /// no-pool fallback, the flush triggers, and the rotation/shutdown
    /// drains all see one buffer.
    #[must_use]
    pub fn with_encode_pool(mut self, pool: crate::encode_pool::EncodePool) -> Self {
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
        *self
            .denial_audit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
        self
    }

    /// Record an authentication rejection on `ourios.ingest.batches`
    /// (`error.type = unauthenticated`, RFC 0026 §3.4) — called by the
    /// transports, which own the 401 surface.
    pub fn record_unauthenticated(&self) {
        self.metrics
            .record_rejected_batch(crate::metrics::ERROR_TYPE_UNAUTHENTICATED);
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
        *self.lock_hook() = Some(hook);
        self
    }

    /// Seed the durable high-water mark from startup recovery
    /// (`RecoveryReport::max_delivered`). Without the seed, a process
    /// that serves zero requests writes shutdown snapshots with no
    /// high-water mark, forcing the next start to discard them and
    /// full-replay (RFC 0001 §6.9). A later commit offset supersedes the
    /// seed.
    #[must_use]
    pub fn with_last_durable(self, offset: Option<WalOffset>) -> Self {
        *self.lock_last_durable() = offset;
        self
    }

    /// [`Self::ingest_bound`] without an authenticated binding — the open
    /// mode (RFC 0026 §3.1) and pre-auth call sites, byte-for-byte today's
    /// behavior.
    ///
    /// # Errors
    ///
    /// As [`Self::ingest_bound`], minus the binding rejection.
    pub async fn ingest(&self, request: ExportLogsServiceRequest) -> Result<usize, ReceiveError> {
        self.ingest_bound(request, None, false).await
    }

    /// Ingest one decoded export per the §6.5 sequence: enforce the
    /// RFC 0026 §3.2 tenant binding (when `binding` is present), fan out,
    /// append the export as a single `OtlpBatch` frame, **fsync** (batched
    /// via the group-commit coordinator), then hand the records to the
    /// miner, then ack. Returns the number of records ingested (`0` for
    /// the empty fast path).
    ///
    /// The covering fsync completes before this returns `Ok`, so the
    /// caller never acks a batch that isn't durable (`[§3.4]`). `&self`
    /// (not `&mut`): concurrency is the inner locks' job, so concurrent
    /// requests batch into one window's fsync (RFC0008.8).
    ///
    /// # Errors
    ///
    /// - [`ReceiveError::TenantDenied`] if any `ResourceLogs` group's
    ///   derived tenant falls outside the binding's set — the whole batch
    ///   is rejected before any WAL write, with no partial success
    ///   (RFC 0026 §3.2).
    /// - [`ReceiveError::TenantResolution`] if any `ResourceLogs` fails
    ///   tenant resolution — the whole batch is rejected before any WAL
    ///   write (RFC0003.4).
    /// - [`ReceiveError::WalAppend`] / [`ReceiveError::WalSync`] if
    ///   persistence fails; the batch is **not** acked.
    pub async fn ingest_bound(
        &self,
        request: ExportLogsServiceRequest,
        binding: Option<&super::auth::AuthBinding>,
        lenient_json: bool,
    ) -> Result<usize, ReceiveError> {
        // RFC 0026 §3.2: authz precedes every other ingest step — a denied
        // batch does no encode, fan-out, or WAL work. §3.4: the denial counts on
        // `ourios.ingest.batches` (`error.type = permission_denied`)
        // and emits the audit event.
        if let Some(binding) = binding
            && let Err(e) = super::auth::check_binding(&request, &self.rule, binding)
        {
            if let ReceiveError::TenantDenied { token_name, tenant } = &e {
                self.metrics
                    .record_rejected_batch(crate::metrics::ERROR_TYPE_PERMISSION_DENIED);
                let mut sink = self
                    .denial_audit
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(sink) = sink.as_mut() {
                    sink.emit(ourios_core::audit::AuditEvent {
                        tenant_id: tenant.clone(),
                        timestamp: std::time::SystemTime::now(),
                        payload: ourios_core::audit::AuditPayload::IngestDenied {
                            token_name: token_name.clone(),
                        },
                    });
                }
            }
            return Err(e);
        }
        // Encode before fan-out consumes the request: the WAL frame is a
        // protobuf `ExportLogsServiceRequest` (§6.5 step 3). Byte-equality
        // to the wire isn't required — recoverability is.
        let payload = request.encode_to_vec();

        // Steps 1–2: fan out per tenant. An unresolvable Resource rejects
        // the entire batch here, before any WAL write (RFC0003.4).
        let records = fan_out(request, &self.rule)?;

        // Empty fast path (RFC0003.12): no records → success, no WAL
        // frame, miner untouched.
        if records.is_empty() {
            return Ok(0);
        }

        // Steps 3–4: append the export as one OtlpBatch frame and await
        // its (batched) fsync — concurrent requests fold into one window's
        // fsync (RFC0008.8). The frame's append `seq` orders the miner
        // hand-off below.
        let commit_start = Instant::now();
        let outcome = self.coordinator.commit(&payload).await;
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
        let ack = match outcome.result {
            Ok(now) => {
                // §6.9 rotation cadence: a segment change since the prior
                // (now strictly-previous) durable mark means the WAL
                // rotated under this batch; fire the hook with the
                // rotation-point high-water mark BEFORE this batch's
                // records reach the miner, so a snapshot it takes reflects
                // exactly the frames at or below that mark. In-order, so
                // `before` is exactly the preceding seq's offset and no
                // higher seq has ingested yet.
                let before = *self.lock_last_durable();
                let mined = {
                    let mut miner = self.lock_miner();
                    if let Some(prev) = before
                        && prev.segment != now.segment
                    {
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
                        // drain the sink buffers hold exactly the frames
                        // ≤ prev, `flush_all` takes every partition, and
                        // the high-water is stamped only when both sinks
                        // fully drained — so a record the mark claims
                        // durable is either in the store or the mark was
                        // never written. A crash at any point before the
                        // stamp loses only buffered Parquet, which WAL
                        // replay above the (previous) high-water re-mines
                        // (the §6.9 posture: the WAL is the durability of
                        // record; a snapshot is a rebuildable cache).
                        self.quiesce_encodes();
                        self.fire_rotation_hook(&miner, prev);
                    }
                    if self.encode_pool.is_some() {
                        // Ordered phase (RFC 0035 §3.1): id assignment +
                        // audit under the gate; the sink emit is deferred
                        // to the concurrent pool below.
                        let mut out = Vec::with_capacity(records.len());
                        for record in &records {
                            let (_, rec) = miner.ingest_mined(record);
                            out.extend(rec);
                        }
                        out
                    } else {
                        for record in &records {
                            miner.ingest(record);
                        }
                        Vec::new()
                    }
                };
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
                *self.lock_last_durable() = Some(now);
                // Throughput + WAL-before-ack latency for this acked batch.
                // RFC 0018 §3.5: tag out-of-range-severity records on the
                // ingest counter via `error.type` (post-materialise on the
                // preserved `u8`; the rare non-`u8` extremes narrowed to 0
                // aren't separately attributed — accepted limitation).
                let severity_out_of_range = records
                    .iter()
                    .filter(|r| super::materialize::severity_is_out_of_range(r.severity_number))
                    .count();
                self.metrics.record_batch(
                    records.len(),
                    severity_out_of_range,
                    lenient_json,
                    append_elapsed,
                );
                Ok(records.len())
            }
            // Sync failed: the frame is not durable and not acked; it
            // reaches neither the miner nor the durable mark.
            Err(e) => Err(e),
        };
        // `_gate` releases the hand-off to `seq + 1` as it drops here.
        ack
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
        *self.lock_last_durable()
    }

    fn lock_miner(&self) -> std::sync::MutexGuard<'_, MinerCluster> {
        self.miner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_last_durable(&self) -> std::sync::MutexGuard<'_, Option<WalOffset>> {
        self.last_durable
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_hook(&self) -> std::sync::MutexGuard<'_, Option<RotationHook>> {
        self.rotation_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Failure ingesting an export. Tenant-resolution failures reject the
/// whole batch (RFC0003.4); WAL failures mean the batch is not acked
/// (`[§3.4]`).
#[derive(Debug)]
#[non_exhaustive]
pub enum ReceiveError {
    /// A `ResourceLogs` group's Resource did not resolve to a tenant.
    TenantResolution(TenantResolutionError),
    /// A group's derived tenant falls outside the authenticated token's
    /// allowed set — the whole batch is denied before any WAL work
    /// (RFC 0026 §3.2, `PERMISSION_DENIED` / 403).
    TenantDenied {
        /// The rejecting token's audit/metric label (never the value) —
        /// what the §3.4 rejection audit event will carry.
        token_name: String,
        /// The offending derived tenant.
        tenant: ourios_core::tenant::TenantId,
    },
    /// Appending the `OtlpBatch` frame to the WAL failed.
    WalAppend(ourios_wal::AppendError),
    /// Fsyncing the WAL failed — the batch must not be acked.
    WalSync(ourios_wal::SyncError),
}

impl From<TenantResolutionError> for ReceiveError {
    fn from(e: TenantResolutionError) -> Self {
        Self::TenantResolution(e)
    }
}

impl std::fmt::Display for ReceiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // `TenantResolutionError`'s own Display already leads with
            // "tenant resolution failed: …"; delegate, don't re-prefix.
            Self::TenantResolution(e) => write!(f, "{e}"),
            // The wire message names the tenant, not the token: the caller
            // knows its own credential, and the token's label belongs to
            // the operator's audit surface (RFC 0026 §3.4), not the
            // client's error text.
            Self::TenantDenied { tenant, .. } => write!(
                f,
                "tenant `{}` is outside the authenticated token's allowed \
                 tenant set (RFC 0026 §3.2)",
                tenant.as_str(),
            ),
            Self::WalAppend(e) => write!(f, "{e}"),
            Self::WalSync(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReceiveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TenantResolution(e) => Some(e),
            Self::TenantDenied { .. } => None,
            Self::WalAppend(e) => Some(e),
            Self::WalSync(e) => Some(e),
        }
    }
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
        fn append_batch(&mut self, _payload: &[u8]) -> Result<(), ReceiveError> {
            self.appends.fetch_add(1, Ordering::SeqCst);
            Ok(())
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
        IngestPipeline::new(
            coordinator,
            MinerCluster::new(MinerConfig::default()),
            TenantRule::service_name(),
        )
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
        pipeline.ingest(request()).await.expect("ingest");
        assert_eq!(pipeline.last_durable(), Some(synced));
    }

    /// `sync` reports offsets from a queue, so a segment change can be
    /// staged mid-sequence.
    struct SequenceJournal {
        offsets: Mutex<Vec<WalOffset>>,
    }

    impl Journal for SequenceJournal {
        fn append_batch(&mut self, _payload: &[u8]) -> Result<(), ReceiveError> {
            Ok(())
        }

        fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
            Ok(self.offsets.lock().expect("offsets").remove(0))
        }

        fn unflushed_bytes(&self) -> u64 {
            0
        }
    }

    fn sequence_pipeline(offsets: Vec<WalOffset>, hook: RotationHook) -> IngestPipeline {
        let coordinator = CommitCoordinator::new(
            Box::new(SequenceJournal {
                offsets: Mutex::new(offsets),
            }),
            Duration::from_millis(5),
            u64::MAX,
        );
        IngestPipeline::new(
            coordinator,
            MinerCluster::new(MinerConfig::default()),
            TenantRule::service_name(),
        )
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
        pipeline.ingest(request()).await.expect("batch 1");
        assert!(calls.lock().expect("lock").is_empty());

        // Batch 2: segment changed — the hook fires once with the OLD
        // segment's last durable offset, before batch 2 hits the miner
        // (the template count is still batch 1's).
        pipeline.ingest(request()).await.expect("batch 2");
        assert_eq!(*calls.lock().expect("lock"), vec![(in_first, 1)]);

        // Batch 3: same segment — no further firing.
        pipeline.ingest(request()).await.expect("batch 3");
        assert_eq!(calls.lock().expect("lock").len(), 1);
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

        pipeline.ingest(request()).await.expect("batch 1");
        // Batch 2 rotates and the hook panics — the ingest still acks
        // and the records still reach the miner.
        assert_eq!(pipeline.ingest(request()).await.expect("batch 2 acks"), 1);
        assert_eq!(
            pipeline.with_miner(|m| {
                m.template_count(&ourios_core::tenant::TenantId::new("checkout"))
            }),
            1,
            "the rotating batch's records reached the miner despite the panic",
        );
        // The pipeline stays usable afterwards.
        assert_eq!(pipeline.ingest(request()).await.expect("batch 3 acks"), 1);
    }
}
