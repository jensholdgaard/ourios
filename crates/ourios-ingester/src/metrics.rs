//! OpenTelemetry instruments for the ingester's subsystems: ingest, the
//! barrier and encode pool, the record and audit sinks, and compaction
//! (`compaction`).
//!
//! Names come from the generated [`ourios_semconv`] constants; the
//! instruments resolve through the **global** meter, per the RFC 0001
//! §6.8 API/SDK split (the SDK + OTLP exporter live in
//! `ourios-telemetry`, configured once by the binary). When no provider
//! is installed the global meter is a no-op, so constructing and
//! recording is always safe.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{
    Counter, Histogram, ObservableGauge, ObservableUpDownCounter, UpDownCounter,
};
use ourios_core::histogram_buckets;
use ourios_semconv as semconv;

use crate::cadence::BarrierEpochs;
use crate::compactor::to_u64;

mod compaction;

pub use compaction::CompactionMetrics;

/// Ingest-path instruments (RFC 0014 §6.3 / `CLAUDE.md` §3.4): throughput
/// plus the WAL-before-ack durability latency. Built once per pipeline on
/// the global `ourios.ingest` meter; recording is a no-op when no provider
/// is installed (RFC 0001 §6.8 API/SDK split). System-aggregate (no
/// tenant/service attributes) so a performance run reads throughput directly.
#[derive(Debug)]
pub struct IngestMetrics {
    records: Counter<u64>,
    batches: Counter<u64>,
    append_duration: Histogram<f64>,
}

impl IngestMetrics {
    /// Build the ingest instruments with their registry units.
    #[must_use]
    pub fn new() -> Self {
        let meter = ourios_core::telemetry::meter("ourios.ingest");
        let records = meter
            .u64_counter(semconv::OURIOS_INGEST_RECORDS)
            .with_unit("{record}")
            .build();
        let batches = meter
            .u64_counter(semconv::OURIOS_INGEST_BATCHES)
            .with_unit("{batch}")
            .build();
        let append_duration = meter
            .f64_histogram(semconv::OURIOS_WAL_APPEND_DURATION)
            .with_unit("s")
            .with_boundaries(histogram_buckets::FAST_DURATION_S.to_vec())
            .build();
        // Seed the attribute-free counters so they are visible before the
        // first batch (collect-on-read); the histogram surfaces on a real
        // sample.
        records.add(0, &[]);
        batches.add(0, &[]);
        Self {
            records,
            batches,
            append_duration,
        }
    }

    /// Record one durably-acknowledged batch: `record_count` log records
    /// made durable in `elapsed` (the append + fsync / WAL-before-ack
    /// latency), of which `severity_out_of_range` carried an out-of-`0..=24`
    /// `SeverityNumber`. Call only on a successful, acked commit.
    ///
    /// Per the OpenTelemetry "recording errors on metrics" convention, the
    /// `ourios.ingest.records` counter carries the standard `error.type`
    /// attribute: in-range records record with it **absent** (success),
    /// out-of-range ones with `error.type = severity_out_of_range`
    /// (RFC 0018 §3.5) — one counter, reason on a low-cardinality attribute,
    /// not a bespoke metric.
    /// `lenient_json` marks a batch whose OTLP/JSON payload only parsed
    /// via the lenient unset-`AnyValue` retry (ourios#549): recorded as
    /// the registry's `ourios.ingest.json.lenient` attribute on the
    /// batches counter — the same one-instrument-plus-attribute shape as
    /// `error.type`, and the operator/dormancy signal for the interim
    /// upstream workaround.
    pub fn record_batch(
        &self,
        record_count: usize,
        severity_out_of_range: usize,
        lenient_json: bool,
        elapsed: Duration,
    ) {
        if lenient_json {
            self.batches.add(
                1,
                &[KeyValue::new(semconv::OURIOS_INGEST_JSON_LENIENT, true)],
            );
        } else {
            self.batches.add(1, &[]);
        }
        let in_range = record_count.saturating_sub(severity_out_of_range);
        if in_range > 0 {
            self.records.add(to_u64(in_range), &[]);
        }
        if severity_out_of_range > 0 {
            self.records.add(
                to_u64(severity_out_of_range),
                &[KeyValue::new(ERROR_TYPE, SEVERITY_OUT_OF_RANGE)],
            );
        }
        self.append_duration.record(elapsed.as_secs_f64(), &[]);
    }

    /// Record one rejected request on the existing `ourios.ingest.batches`
    /// counter with `error.type` (`unauthenticated` | `permission_denied`)
    /// — the RFC 0026 §3.4 recording-errors convention: no new metric
    /// name, the reason on a low-cardinality attribute.
    pub fn record_rejected_batch(&self, error_type: &'static str) {
        self.batches
            .add(1, &[KeyValue::new(ERROR_TYPE, error_type)]);
    }
}

// RFC 0051: `AuthMetrics` and the shared `error.type` class values
// moved to `ourios-serving` with the resolver; re-imported so the
// ingest-side rejection tagging stays on the same value space.
pub use ourios_serving::metrics::{
    ERROR_TYPE_PERMISSION_DENIED, ERROR_TYPE_UNAUTHENTICATED, ERROR_TYPE_UPSTREAM_UNAVAILABLE,
};

/// The OpenTelemetry-standard `error.type` attribute key (semconv, stable).
/// Deliberately **not** in the Ourios weaver registry — it is an upstream
/// OpenTelemetry attribute used here per the "recording errors on metrics"
/// convention, not an Ourios-coined name.
pub(crate) const ERROR_TYPE: &str = "error.type";
/// The domain-specific `error.type` value for an out-of-`0..=24`
/// `SeverityNumber` (RFC 0018 §3.5). `error.type`'s value space is open.
const SEVERITY_OUT_OF_RANGE: &str = "severity_out_of_range";
/// The `error.type` value for a cadence sweep step that panicked (#791),
/// and for an RFC 0052 §3.2 housekeeping tick that panicked.
///
/// The housekeeping case is survivable: the task takes the next tick, the
/// checkpoint is untouched and the next pass re-plans what the failed one
/// left uncommitted, so it is only a stall in reclamation. The panic
/// message on stderr names which of the two it was.
///
/// A count from the sweep means the **age/cadence** flush trigger is dead for the
/// life of the process: the sweep stops on a panic, because continuing would
/// repeat #796's data-loss window every tick.
///
/// The size and ceiling triggers keep working, so a busy partition still
/// flushes on its own. What is lost is the age drain, which is the only
/// trigger a *low-volume* partition ever reaches — so those partitions wait
/// for WAL rotation or shutdown, which is hazard #4's small-file and
/// staleness problem rather than a total stop.
///
/// Either way this is not a rate to watch: a single occurrence is the alert.
///
/// **Whether a restart is safe depends on where the panic landed, and the
/// counter cannot tell you.** This counts a panic anywhere in the step. One
/// raised before `drain_aged` has taken anything — in the miner or the drain
/// itself — loses nothing: the records are still buffered and still in the
/// WAL, and a restart replays normally.
///
/// One raised *after* the drain, in the publish, is the #796 window: those
/// batches are out of the sink and in the WAL only. `shutdown` then runs the
/// snapshot barrier, which reads the emptied buffers as fully drained and can
/// stamp a WAL high-water mark across them, after which recovery suppresses
/// them. So the loss window opens with that panic and a graceful shutdown is
/// what closes it.
///
/// The panic message on stderr is what distinguishes the two, which is why it
/// matters that the default hook still prints it. Treat this count as "check
/// where it panicked before restarting", not as confirmed loss — and until
/// #796 lands, accept that for the publish case no restart both restores the
/// cadence and preserves those records.
pub(crate) const CADENCE_PANIC: &str = "cadence_panic";

impl Default for IngestMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// One reading shared by two observable instruments that must agree.
///
/// The SDK runs one callback per instrument, each exactly once per
/// collection, and has no multi-instrument callback, so the pair shares
/// a slot: whichever callback runs first takes a fresh reading and parks
/// it, and the other consumes that same reading. Each collection
/// therefore reports one reading on both instruments, whatever order
/// the callbacks run in.
#[derive(Debug)]
pub(crate) struct PairedReading<T> {
    parked: Mutex<Option<T>>,
}

impl<T> Default for PairedReading<T> {
    fn default() -> Self {
        Self {
            parked: Mutex::new(None),
        }
    }
}

impl<T: Copy> PairedReading<T> {
    pub(crate) fn read(&self, fresh: impl Fn() -> T) -> T {
        let mut parked = self
            .parked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(reading) = parked.take() {
            return reading;
        }
        let reading = fresh();
        *parked = Some(reading);
        reading
    }
}

/// RFC 0052 §3.1's publication-barrier instruments (#833): what each cut
/// and capture decided, the checkpoint and snapshot writes the barrier
/// attempts, and the epoch pair that says whether the `cadence_failed`
/// latch is set. Built once per barrier on the global `ourios.ingest`
/// meter.
#[derive(Debug)]
pub struct BarrierMetrics {
    cuts: Counter<u64>,
    captures: Counter<u64>,
    checkpoint_writes: Counter<u64>,
    snapshot_writes: Counter<u64>,
    #[expect(dead_code, reason = "retains the observable-callback registrations")]
    epochs: [ObservableGauge<u64>; 2],
}

impl BarrierMetrics {
    /// Build the instruments; the epoch gauges read `epochs` at collect
    /// time.
    #[must_use]
    pub fn new(epochs: &Arc<BarrierEpochs>) -> Self {
        let meter = ourios_core::telemetry::meter("ourios.ingest");
        let cuts = meter
            .u64_counter(semconv::OURIOS_INGEST_BARRIER_CUTS)
            .with_unit("{cut}")
            .build();
        let captures = meter
            .u64_counter(semconv::OURIOS_INGEST_BARRIER_CAPTURES)
            .with_unit("{capture}")
            .build();
        let checkpoint_writes = meter
            .u64_counter(semconv::OURIOS_INGEST_BARRIER_CHECKPOINT_WRITES)
            .with_unit("{write}")
            .build();
        let snapshot_writes = meter
            .u64_counter(semconv::OURIOS_INGEST_BARRIER_SNAPSHOT_WRITES)
            .with_unit("{write}")
            .build();
        // Weak, so a barrier that is gone stops reporting rather than
        // being kept alive by the meter provider. One paired reading per
        // collection, the latch read before the epoch it can only trail,
        // so the pair never shows a failed epoch above the current one.
        let reading = Arc::new(PairedReading::default());
        let read = {
            let epochs = Arc::downgrade(epochs);
            move || {
                epochs.upgrade().map(|epochs| {
                    let failed = epochs.capture().failed_epoch();
                    (
                        failed.map(|f| u64::from(f.get())),
                        u64::from(epochs.current().get()),
                    )
                })
            }
        };
        let (paired, fresh) = (Arc::clone(&reading), read.clone());
        let epoch = meter
            .u64_observable_gauge(semconv::OURIOS_INGEST_BARRIER_EPOCH)
            .with_unit("{epoch}")
            .with_callback(move |observer| {
                if let Some((_, current)) = paired.read(&fresh) {
                    observer.observe(current, &[]);
                }
            })
            .build();
        let failed_epoch = meter
            .u64_observable_gauge(semconv::OURIOS_INGEST_BARRIER_FAILED_EPOCH)
            .with_unit("{epoch}")
            .with_callback(move |observer| {
                if let Some((Some(failed), _)) = reading.read(&read) {
                    observer.observe(failed, &[]);
                }
            })
            .build();
        Self {
            cuts,
            captures,
            checkpoint_writes,
            snapshot_writes,
            epochs: [epoch, failed_epoch],
        }
    }

    /// One cut's decision, as `ourios.ingest.barrier.cut.outcome`.
    pub fn record_cut(&self, outcome: &'static str) {
        self.cuts.add(
            1,
            &[KeyValue::new(
                semconv::OURIOS_INGEST_BARRIER_CUT_OUTCOME,
                outcome,
            )],
        );
    }

    /// One capture's effect on the pending slot.
    pub fn record_capture(&self, outcome: &'static str) {
        self.captures.add(
            1,
            &[KeyValue::new(
                semconv::OURIOS_INGEST_BARRIER_CAPTURE_OUTCOME,
                outcome,
            )],
        );
    }

    /// One checkpoint write: `error_type` is `None` on success.
    pub fn record_checkpoint_write(&self, error_type: Option<&'static str>) {
        match error_type {
            None => self.checkpoint_writes.add(1, &[]),
            Some(class) => self
                .checkpoint_writes
                .add(1, &[KeyValue::new(ERROR_TYPE, class)]),
        }
    }

    /// One tenant's snapshot write: `error_type` is `None` on success.
    pub fn record_snapshot_write(&self, tenant: &str, error_type: Option<&'static str>) {
        let tenant = KeyValue::new(semconv::OURIOS_TENANT, tenant.to_owned());
        match error_type {
            None => self.snapshot_writes.add(1, &[tenant]),
            Some(class) => self
                .snapshot_writes
                .add(1, &[tenant, KeyValue::new(ERROR_TYPE, class)]),
        }
    }
}

/// Encode-pool instruments (RFC 0035 §6.3): the concurrent encode
/// queue's current depth. Built once per pool on the global
/// `ourios.ingest` meter; recording is a no-op when no provider is
/// installed (RFC 0001 §6.8 API/SDK split).
#[derive(Debug)]
pub struct EncodePoolMetrics {
    queue_depth: UpDownCounter<i64>,
}

impl EncodePoolMetrics {
    /// Build the pool instruments with their registry units.
    #[must_use]
    pub fn new() -> Self {
        let meter = ourios_core::telemetry::meter("ourios.ingest");
        let queue_depth = meter
            .i64_up_down_counter(semconv::OURIOS_INGEST_ENCODE_QUEUE_DEPTH)
            .with_unit("{batch}")
            .build();
        // Seed so the gauge is visible before the first submitted batch
        // (collect-on-read).
        queue_depth.add(0, &[]);
        Self { queue_depth }
    }

    /// A batch entered the encode queue.
    pub fn batch_submitted(&self) {
        self.queue_depth.add(1, &[]);
    }

    /// A batch left the queue (fully emitted to the record sink, or
    /// undone because the pool was already shutting down).
    pub fn batch_completed(&self) {
        self.queue_depth.add(-1, &[]);
    }
}

impl Default for EncodePoolMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Record-sink instruments (RFC 0014 §6.3): flush throughput + latency by
/// trigger, flush/derive errors, and current buffer occupancy. Built once
/// per sink on the global `ourios.sink` meter.
#[derive(Debug)]
pub struct SinkMetrics {
    flush_duration: Histogram<f64>,
    flush_records: Counter<u64>,
    flush_errors: Counter<u64>,
    derive_errors: Counter<u64>,
    buffer_usage: UpDownCounter<i64>,
}

impl SinkMetrics {
    /// Build the sink instruments with their registry units.
    #[must_use]
    pub fn new() -> Self {
        let meter = ourios_core::telemetry::meter("ourios.sink");
        let flush_duration = meter
            .f64_histogram(semconv::OURIOS_SINK_FLUSH_DURATION)
            .with_unit("s")
            .with_boundaries(histogram_buckets::DURATION_S.to_vec())
            .build();
        let flush_records = meter
            .u64_counter(semconv::OURIOS_SINK_FLUSH_RECORDS)
            .with_unit("{record}")
            .build();
        let flush_errors = meter
            .u64_counter(semconv::OURIOS_SINK_FLUSH_ERRORS)
            .with_unit("{error}")
            .build();
        let derive_errors = meter
            .u64_counter(semconv::OURIOS_SINK_DERIVE_ERRORS)
            .with_unit("{error}")
            .build();
        let buffer_usage = meter
            .i64_up_down_counter(semconv::OURIOS_SINK_BUFFER_USAGE)
            .with_unit("By")
            .build();
        // Seed the attribute-free instruments so they're visible before the
        // first flush; `flush_duration` / `flush_records` carry the required
        // `trigger` attribute and surface on the first real flush.
        flush_errors.add(0, &[]);
        derive_errors.add(0, &[]);
        buffer_usage.add(0, &[]);
        Self {
            flush_duration,
            flush_records,
            flush_errors,
            derive_errors,
            buffer_usage,
        }
    }

    /// A successful partition flush of `record_count` rows, caused by
    /// `trigger` (`size` | `age` | `rotation` | `ceiling`), taking `elapsed`.
    pub fn record_flush(&self, trigger: &'static str, record_count: usize, elapsed: Duration) {
        let attrs = [KeyValue::new(semconv::OURIOS_SINK_FLUSH_TRIGGER, trigger)];
        self.flush_duration.record(elapsed.as_secs_f64(), &attrs);
        self.flush_records.add(to_u64(record_count), &attrs);
    }

    /// A failed partition flush (encode or store error); the buffer is
    /// retained and retried (the WAL is the durability of record).
    /// `error_type` carries the permanent-rejection class for
    /// quarantines (RFC 0025 §3.3, the `error.type` convention);
    /// transient store failures pass `None`.
    pub fn record_flush_error(&self, error_type: Option<&'static str>) {
        match error_type {
            Some(t) => self.flush_errors.add(1, &[KeyValue::new(ERROR_TYPE, t)]),
            None => self.flush_errors.add(1, &[]),
        }
    }

    /// A record dropped from the sink because its partition key could not
    /// be derived (the WAL still holds it).
    pub fn record_derive_error(&self) {
        self.derive_errors.add(1, &[]);
    }

    /// Adjust the buffered-bytes gauge by `delta` — positive when a record
    /// is appended, negative when a partition flushes.
    pub fn add_buffered(&self, delta: i64) {
        self.buffer_usage.add(delta, &[]);
    }
}

impl Default for SinkMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// `ourios.audit_sink.flush.outcome` value for a transient (store-I/O) flush
/// failure: the batch is retained and retried (the WAL is the durability of
/// record).
pub const FLUSH_OUTCOME_TRANSIENT: &str = "transient";
/// `ourios.audit_sink.flush.outcome` value for a permanent (content / encode)
/// flush failure: the batch is dropped so one malformed event can't wedge the
/// partition behind it (issue #302).
pub const FLUSH_OUTCOME_PERMANENT: &str = "permanent";

/// Audit-sink instruments (issue #302 / `CLAUDE.md` §6.3): the miner's
/// template-audit write path, mirroring [`SinkMetrics`]. Built once per sink on
/// the global `ourios.audit_sink` meter; recording is a no-op when no provider
/// is installed (RFC 0001 §6.8 API/SDK split).
#[derive(Debug)]
pub struct AuditSinkMetrics {
    flushes: Counter<u64>,
    flush_events: Counter<u64>,
    flush_errors: Counter<u64>,
    derive_errors: Counter<u64>,
    /// Absolute current buffered-event count, kept up to date by the sink and
    /// read by the `buffer.usage` observable's callback at collect time — the
    /// `ourios.compaction.backlog` pattern (an observable gauge of the absolute
    /// value avoids the drift a per-event delta `UpDownCounter` would accrue).
    buffered_state: Arc<AtomicI64>,
    #[expect(dead_code, reason = "retains the observable-callback registration")]
    buffer_usage: ObservableUpDownCounter<i64>,
}

impl AuditSinkMetrics {
    /// Build the audit-sink instruments with their registry units.
    #[must_use]
    pub fn new() -> Self {
        let meter = ourios_core::telemetry::meter("ourios.audit_sink");
        let flushes = meter
            .u64_counter(semconv::OURIOS_AUDIT_SINK_FLUSHES)
            .with_unit("{flush}")
            .build();
        let flush_events = meter
            .u64_counter(semconv::OURIOS_AUDIT_SINK_FLUSH_EVENTS)
            .with_unit("{event}")
            .build();
        let flush_errors = meter
            .u64_counter(semconv::OURIOS_AUDIT_SINK_FLUSH_ERRORS)
            .with_unit("{error}")
            .build();
        let derive_errors = meter
            .u64_counter(semconv::OURIOS_AUDIT_SINK_DERIVE_ERRORS)
            .with_unit("{error}")
            .build();

        // `buffer.usage` is an **observable** UpDownCounter reporting the
        // absolute buffered-event count at collect time (the backlog pattern).
        let buffered_state = Arc::new(AtomicI64::new(0));
        let callback_state = Arc::clone(&buffered_state);
        let buffer_usage = meter
            .i64_observable_up_down_counter(semconv::OURIOS_AUDIT_SINK_BUFFER_USAGE)
            .with_unit("{event}")
            .with_callback(move |observer| {
                observer.observe(callback_state.load(Ordering::Relaxed), &[]);
            })
            .build();

        // Seed the attribute-free counters so they're visible before the first
        // flush (collect-on-read); `flush_errors` carries the required
        // `flush.outcome` attribute and surfaces on the first real error.
        flushes.add(0, &[]);
        flush_events.add(0, &[]);
        derive_errors.add(0, &[]);
        Self {
            flushes,
            flush_events,
            flush_errors,
            derive_errors,
            buffered_state,
            buffer_usage,
        }
    }

    /// `partitions` successful partition flushes carrying `events` events in
    /// total (one call settles a whole flush pass).
    pub fn record_flush(&self, partitions: u64, events: u64) {
        self.flushes.add(partitions, &[]);
        self.flush_events.add(events, &[]);
    }

    /// A failed partition flush, classified by `outcome`
    /// ([`FLUSH_OUTCOME_TRANSIENT`] → retained + retried, or
    /// [`FLUSH_OUTCOME_PERMANENT`] → dropped).
    pub fn record_flush_error(&self, outcome: &'static str) {
        self.flush_errors.add(
            1,
            &[KeyValue::new(
                semconv::OURIOS_AUDIT_SINK_FLUSH_OUTCOME,
                outcome,
            )],
        );
    }

    /// An event dropped because its partition key could not be derived (a
    /// pre-epoch / overflowing timestamp).
    pub fn record_derive_error(&self) {
        self.derive_errors.add(1, &[]);
    }

    /// Update the observable buffer gauge to the absolute `events` now buffered.
    pub fn set_buffered(&self, events: usize) {
        self.buffered_state
            .store(i64::try_from(events).unwrap_or(i64::MAX), Ordering::Relaxed);
    }
}

impl Default for AuditSinkMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two callbacks in one collection see one reading, whichever runs
    /// first, even when the source moves between them; the next
    /// collection reads afresh.
    #[test]
    fn a_paired_reading_is_shared_by_both_callbacks_of_a_collection() {
        let source = std::sync::atomic::AtomicU64::new(1);
        let fresh = || source.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let paired = PairedReading::default();
        assert_eq!(paired.read(fresh), 1, "the first callback reads");
        assert_eq!(paired.read(fresh), 1, "the second sees the same reading");
        assert_eq!(paired.read(fresh), 2, "the next collection reads afresh");
        assert_eq!(paired.read(fresh), 2);
    }
}
