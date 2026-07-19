//! OpenTelemetry instruments for the compaction sweep (RFC 0009 §3.6).
//!
//! Names come from the generated [`ourios_semconv`] constants; the
//! instruments resolve through the **global** meter, per the RFC 0001
//! §6.8 API/SDK split (the SDK + OTLP exporter live in
//! `ourios-telemetry`, configured once by the binary). When no provider
//! is installed the global meter is a no-op, so constructing and
//! recording is always safe.
//!
//! Records the per-sweep counters and histograms of RFC 0009 §3.6:
//! sweeps, partitions, files, rows, orphan files, sweep duration,
//! `ourios.compaction.io` (bytes read / written), and the
//! `ourios.storage.parquet.file.size` H4 detector (a per-tenant
//! distribution of consolidated output sizes — the signal behind the
//! "alert when > 5 % of files < 128 MiB" rule). All byte volumes come
//! from [`CompactionOutcome`](ourios_parquet::CompactionOutcome) via
//! the [`SweepReport`]. `ourios.compaction.backlog` — the
//! sealed-but-uncompacted lag — is an **observable** (async)
//! `UpDownCounter`: its callback reports each tenant's *absolute*
//! current backlog at collect time (OpenTelemetry additive-non-monotonic
//! guidance), which `record_sweep` keeps current from the per-tenant
//! candidate/compacted breakdown. This completes the §3.6 metric set.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry::metrics::{Counter, Histogram, ObservableUpDownCounter, UpDownCounter};
use opentelemetry::{KeyValue, global};
use ourios_semconv as semconv;

use crate::compactor::{IngestError, SweepReport, to_u64};

/// Per-tenant current backlog (sealed-but-uncompacted partition count)
/// shared between [`CompactionMetrics::record_sweep`], which writes the
/// absolute value after each sweep, and the `ourios.compaction.backlog`
/// observable callback, which reads it at collection time.
type BacklogState = Arc<Mutex<HashMap<String, i64>>>;

/// The compaction metric instruments (RFC 0009 §3.6). Build one per
/// process and call [`CompactionMetrics::record_sweep`] once per sweep.
#[derive(Debug)]
pub struct CompactionMetrics {
    sweeps: Counter<u64>,
    partitions: Counter<u64>,
    files: Counter<u64>,
    rows: Counter<u64>,
    orphan_files: Counter<u64>,
    duration: Histogram<f64>,
    io: Counter<u64>,
    file_size: Histogram<u64>,
    /// Current per-tenant backlog `record_sweep` keeps up to date; the
    /// observable counter's callback reads it.
    backlog_state: BacklogState,
    /// Held to keep the observable callback registered with the meter
    /// for this instrument's lifetime (the value is never read directly
    /// — the SDK invokes its callback on collect).
    #[expect(dead_code, reason = "retains the observable-callback registration")]
    backlog: ObservableUpDownCounter<i64>,
}

impl CompactionMetrics {
    /// Build the instruments on the `ourios.compaction` meter, with the
    /// UCUM units the registry (`semconv/registry/metrics.yaml`)
    /// declares for each.
    ///
    /// The attribute-free counters are seeded with a zero measurement so
    /// they are visible to the exporter before the first sweep records
    /// anything (RFC 0001 §6.8 collect-on-read). `sweeps` and `duration`
    /// are **not** seeded: their `ourios.compaction.result` attribute is
    /// *required* (a `add(0, &[])` would emit a point missing it), and a
    /// histogram `record(0)` would distort the distribution — both
    /// surface on the first sweep with a real `result`.
    #[must_use]
    pub fn new() -> Self {
        let meter = global::meter("ourios.compaction");
        let sweeps = meter
            .u64_counter(semconv::OURIOS_COMPACTION_SWEEPS)
            .with_unit("{sweep}")
            .build();
        let partitions = meter
            .u64_counter(semconv::OURIOS_COMPACTION_PARTITIONS)
            .with_unit("{partition}")
            .build();
        let files = meter
            .u64_counter(semconv::OURIOS_COMPACTION_FILES)
            .with_unit("{file}")
            .build();
        let rows = meter
            .u64_counter(semconv::OURIOS_COMPACTION_ROWS)
            .with_unit("{row}")
            .build();
        let orphan_files = meter
            .u64_counter(semconv::OURIOS_COMPACTION_ORPHAN_FILES)
            .with_unit("{file}")
            .build();
        let duration = meter
            .f64_histogram(semconv::OURIOS_COMPACTION_DURATION)
            .with_unit("s")
            .build();
        let io = meter
            .u64_counter(semconv::OURIOS_COMPACTION_IO)
            .with_unit("By")
            .build();
        let file_size = meter
            .u64_histogram(semconv::OURIOS_STORAGE_PARQUET_FILE_SIZE)
            .with_unit("By")
            .build();

        // `backlog` is an **observable** (async) UpDownCounter: its
        // callback reports the *absolute* current per-tenant backlog at
        // collect time (OTel additive-non-monotonic guidance). Reporting
        // an absolute value — not a per-sweep delta — is what keeps it
        // from drifting when a candidate errors one sweep and clears the
        // next. `record_sweep` keeps `backlog_state` current; the
        // callback only reads it.
        let backlog_state: BacklogState = Arc::new(Mutex::new(HashMap::new()));
        let callback_state = Arc::clone(&backlog_state);
        let backlog = meter
            .i64_observable_up_down_counter(semconv::OURIOS_COMPACTION_BACKLOG)
            .with_unit("{partition}")
            .with_callback(move |observer| {
                // Recover a poisoned lock rather than panic — a metrics
                // collection callback must never bring the process down.
                let backlog = callback_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                for (tenant, count) in &*backlog {
                    observer.observe(
                        *count,
                        &[KeyValue::new(semconv::OURIOS_TENANT, tenant.clone())],
                    );
                }
            })
            .build();

        // Only the attribute-free counters; `sweeps` and `io` carry
        // required attributes (`result` / `io.direction`) and `duration`
        // / `file_size` are histograms, so none are zero-seeded here —
        // they surface on the first sweep (`io` always, with a 0-byte
        // read/write point; the histograms only on real samples).
        for counter in [&partitions, &files, &rows, &orphan_files] {
            counter.add(0, &[]);
        }

        Self {
            sweeps,
            partitions,
            files,
            rows,
            orphan_files,
            duration,
            io,
            file_size,
            backlog_state,
            backlog,
        }
    }

    /// Record one sweep's outcome and wall-clock `elapsed`. The
    /// `ourios.compaction.result` attribute classifies the sweep:
    /// `error` if any tenant/partition failed (or the sweep itself
    /// failed to scan the store), else `committed` if anything was
    /// consolidated, else `noop`.
    pub fn record_sweep(&self, result: &Result<SweepReport, IngestError>, elapsed: Duration) {
        let outcome = match result {
            Ok(report) if !report.errors.is_empty() => "error",
            Ok(report) if report.partitions_compacted > 0 => "committed",
            Ok(_) => "noop",
            Err(_) => "error",
        };
        let attrs = [KeyValue::new(semconv::OURIOS_COMPACTION_RESULT, outcome)];

        self.sweeps.add(1, &attrs);
        self.duration.record(elapsed.as_secs_f64(), &attrs);

        // A fatal sweep error (couldn't even scan the store) yields no
        // per-tenant data, so the backlog map is deliberately left as-is:
        // the last-known lag is more honest than clearing to nothing (the
        // partitions didn't compact, so the backlog hasn't shrunk), and
        // the failure itself surfaces via `sweeps{result="error"}`.
        if let Ok(report) = result {
            self.partitions
                .add(to_u64(report.partitions_compacted), &[]);
            self.files.add(report.files_compacted, &[]);
            self.rows.add(report.rows_compacted, &[]);
            self.orphan_files.add(to_u64(report.gc_failures), &[]);

            // Bytes moved this sweep, split by direction; the write
            // volume is the sum of the consolidated output sizes
            // (saturating, matching `run_sweep`'s read accumulation).
            let bytes_written = report
                .compacted_files
                .iter()
                .fold(0_u64, |acc, f| acc.saturating_add(f.bytes));
            self.io.add(
                report.bytes_read,
                &[KeyValue::new(semconv::OURIOS_IO_DIRECTION, "read")],
            );
            self.io.add(
                bytes_written,
                &[KeyValue::new(semconv::OURIOS_IO_DIRECTION, "write")],
            );

            // The H4 detector: one per-tenant sample per consolidated
            // file, so the "> 5 % of files < 128 MiB" rule is a derived
            // alert over this distribution (RFC 0009 §3.6). A `0` size is
            // a best-effort `stat` failure (`file_len`), not a real
            // file — skip it so the small-file distribution isn't skewed
            // by a bogus zero-byte sample.
            for file in report.compacted_files.iter().filter(|f| f.bytes > 0) {
                self.file_size.record(
                    file.bytes,
                    &[KeyValue::new(semconv::OURIOS_TENANT, file.tenant.clone())],
                );
            }

            // Rebuild each tenant's *absolute* backlog (candidates the
            // sweep found minus those it compacted) for the observable
            // counter's callback. Absolute (not a delta), so a tenant
            // that clears its lag reports 0 next sweep rather than the
            // value drifting up over time. A full `clear()` first means a
            // tenant that no longer appears (data removed, or planning
            // errored this sweep) stops being reported rather than
            // emitting a stale value forever; `run_sweep` scans every
            // tenant each pass, so the surviving set is current.
            let mut backlog = self
                .backlog_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            backlog.clear();
            for t in &report.per_tenant {
                // candidates_found ≥ partitions_compacted (you can't
                // compact more than you found), so the lag is non-negative.
                let lag = i64::try_from(t.candidates_found.saturating_sub(t.partitions_compacted))
                    .unwrap_or(i64::MAX);
                backlog.insert(t.tenant.clone(), lag);
            }
        }
    }
}

impl Default for CompactionMetrics {
    fn default() -> Self {
        Self::new()
    }
}

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
        let meter = global::meter("ourios.ingest");
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

/// The `error.type` value for a missing/malformed/unknown bearer
/// (RFC 0026 §3.4).
pub const ERROR_TYPE_UNAUTHENTICATED: &str = "unauthenticated";
/// The `error.type` value for an authenticated cross-tenant rejection
/// (RFC 0026 §3.4).
pub const ERROR_TYPE_PERMISSION_DENIED: &str = "permission_denied";

/// The OpenTelemetry-standard `error.type` attribute key (semconv, stable).
/// Deliberately **not** in the Ourios weaver registry — it is an upstream
/// OpenTelemetry attribute used here per the "recording errors on metrics"
/// convention, not an Ourios-coined name.
const ERROR_TYPE: &str = "error.type";
/// The domain-specific `error.type` value for an out-of-`0..=24`
/// `SeverityNumber` (RFC 0018 §3.5). `error.type`'s value space is open.
const SEVERITY_OUT_OF_RANGE: &str = "severity_out_of_range";

impl Default for IngestMetrics {
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
        let meter = global::meter("ourios.sink");
        let flush_duration = meter
            .f64_histogram(semconv::OURIOS_SINK_FLUSH_DURATION)
            .with_unit("s")
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
        let meter = global::meter("ourios.audit_sink");
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
    use std::collections::BTreeMap;

    use opentelemetry_sdk::metrics::data::{
        AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
    };

    use super::*;
    use crate::compactor::{CompactedFile, TenantSweep};

    // Collected metric names across the in-memory export.
    fn collected_names(rms: &[ResourceMetrics]) -> Vec<String> {
        rms.iter()
            .flat_map(ResourceMetrics::scope_metrics)
            .flat_map(ScopeMetrics::metrics)
            .map(|metric| metric.name().to_string())
            .collect()
    }

    // The exported metric `name`'s aggregated data.
    fn metric_data<'a>(rms: &'a [ResourceMetrics], name: &str) -> &'a AggregatedMetrics {
        rms.iter()
            .flat_map(ResourceMetrics::scope_metrics)
            .flat_map(ScopeMetrics::metrics)
            .find(|m| m.name() == name)
            .unwrap_or_else(|| panic!("metric {name} missing from the exported stream"))
            .data()
    }

    // Every `sweeps` point carries the *required* `result` attribute (the
    // round-1 seed bug emitted an attribute-less point), and the committed
    // sweep is classified as such.
    fn assert_sweeps_classified(rms: &[ResourceMetrics]) {
        let AggregatedMetrics::U64(MetricData::Sum(sum)) =
            metric_data(rms, semconv::OURIOS_COMPACTION_SWEEPS)
        else {
            panic!("sweeps should be a u64 sum");
        };
        assert!(sum.data_points().count() > 0, "a sweep was recorded");
        assert!(
            sum.data_points().all(|dp| dp
                .attributes()
                .any(|kv| kv.key.as_str() == semconv::OURIOS_COMPACTION_RESULT)),
            "every sweeps datapoint must carry the required result attribute",
        );
        assert!(
            sum.data_points().any(|dp| dp.attributes().any(|kv| {
                kv.key.as_str() == semconv::OURIOS_COMPACTION_RESULT
                    && kv.value.as_str() == "committed"
            })),
            "the committed sweep is classified result=committed",
        );
    }

    // `io` carries one read point and one write point, each tagged with
    // its direction; returns the (read, write) byte volumes.
    fn io_volumes(rms: &[ResourceMetrics]) -> (u64, u64) {
        let AggregatedMetrics::U64(MetricData::Sum(io)) =
            metric_data(rms, semconv::OURIOS_COMPACTION_IO)
        else {
            panic!("io should be a u64 sum");
        };
        let direction = |dir: &str| -> u64 {
            io.data_points()
                .find(|dp| {
                    dp.attributes().any(|kv| {
                        kv.key.as_str() == semconv::OURIOS_IO_DIRECTION && kv.value.as_str() == dir
                    })
                })
                .unwrap_or_else(|| panic!("io is missing the {dir} direction"))
                .value()
        };
        (direction("read"), direction("write"))
    }

    // The H4 detector. `expected` is the list of recorded `(tenant, bytes)`
    // samples; OTel aggregates by attribute set, so several files for one
    // tenant collapse into a single datapoint with `count` = file count and
    // `sum` = Σ bytes. Asserts exactly that per-tenant aggregate, and that
    // no extra tenants appear (so 0-byte / skipped samples stay absent).
    fn assert_file_size_histogram(rms: &[ResourceMetrics], expected: &[(&str, u64)]) {
        let AggregatedMetrics::U64(MetricData::Histogram(hist)) =
            metric_data(rms, semconv::OURIOS_STORAGE_PARQUET_FILE_SIZE)
        else {
            panic!("file.size should be a u64 histogram");
        };
        // Fold the expected samples into per-tenant (count, sum) aggregates.
        let mut per_tenant: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
        for &(tenant, bytes) in expected {
            let entry = per_tenant.entry(tenant).or_default();
            entry.0 += 1;
            entry.1 = entry.1.saturating_add(bytes);
        }
        assert_eq!(
            hist.data_points().count(),
            per_tenant.len(),
            "one datapoint per tenant, no extras",
        );
        for (tenant, (count, sum)) in per_tenant {
            let dp = hist
                .data_points()
                .find(|dp| {
                    dp.attributes().any(|kv| {
                        kv.key.as_str() == semconv::OURIOS_TENANT && kv.value.as_str() == tenant
                    })
                })
                .unwrap_or_else(|| panic!("file.size is missing tenant {tenant}"));
            assert_eq!(dp.count(), count, "{tenant}: file count");
            assert_eq!(dp.sum(), sum, "{tenant}: Σ byte sizes");
        }
    }

    // The backlog observable UpDownCounter aggregates as an i64 Sum; each
    // tenant's datapoint reports its absolute current lag.
    fn assert_backlog(rms: &[ResourceMetrics], expected: &[(&str, i64)]) {
        let AggregatedMetrics::I64(MetricData::Sum(sum)) =
            metric_data(rms, semconv::OURIOS_COMPACTION_BACKLOG)
        else {
            panic!("backlog should be an i64 sum (observable UpDownCounter)");
        };
        assert_eq!(
            sum.data_points().count(),
            expected.len(),
            "one backlog datapoint per expected tenant, no extras",
        );
        for &(tenant, lag) in expected {
            let dp = sum
                .data_points()
                .find(|dp| {
                    dp.attributes().any(|kv| {
                        kv.key.as_str() == semconv::OURIOS_TENANT && kv.value.as_str() == tenant
                    })
                })
                .unwrap_or_else(|| panic!("backlog is missing tenant {tenant}"));
            assert_eq!(dp.value(), lag, "{tenant}: absolute backlog");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn record_sweep_exports_the_compaction_metric_set() {
        // Arrange — an in-memory provider, then the instruments (built
        // after, so they resolve against it).
        let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");
        let metrics = CompactionMetrics::new();
        let report = SweepReport {
            tenants_scanned: 2,
            partitions_compacted: 2,
            files_compacted: 7,
            rows_compacted: 100,
            gc_failures: 1,
            orphans_reclaimed: 0,
            errors: Vec::new(),
            compaction_events: Vec::new(),
            bytes_read: 4096,
            // acme: two consolidated files (OTel merges them into one
            // per-tenant datapoint); beta: one; ghost: a 0-byte sample
            // standing in for a best-effort `stat` failure (must be
            // dropped from the H4 histogram).
            compacted_files: vec![
                CompactedFile {
                    tenant: "acme".to_string(),
                    bytes: 1024,
                },
                CompactedFile {
                    tenant: "beta".to_string(),
                    bytes: 2048,
                },
                CompactedFile {
                    tenant: "acme".to_string(),
                    bytes: 512,
                },
                CompactedFile {
                    tenant: "ghost".to_string(),
                    bytes: 0,
                },
            ],
            // acme found 3 candidates but compacted 1 → backlog 2 (lag);
            // beta compacted all 2 → backlog 0 (cleared).
            per_tenant: vec![
                TenantSweep {
                    tenant: "acme".to_string(),
                    candidates_found: 3,
                    partitions_compacted: 1,
                },
                TenantSweep {
                    tenant: "beta".to_string(),
                    candidates_found: 2,
                    partitions_compacted: 2,
                },
            ],
        };

        // Act
        metrics.record_sweep(&Ok(report), Duration::from_millis(50));
        guard.force_flush().expect("force_flush succeeds");

        // Assert — every recorded instrument is in the exported stream.
        // (All assertions share this test's single in-memory provider:
        // `init_in_memory` installs the *global* meter, so two such tests
        // in one binary would race — one test, one provider.)
        let rms = exporter.get_finished_metrics().expect("metrics exported");
        let names = collected_names(&rms);
        for expected in [
            semconv::OURIOS_COMPACTION_SWEEPS,
            semconv::OURIOS_COMPACTION_PARTITIONS,
            semconv::OURIOS_COMPACTION_FILES,
            semconv::OURIOS_COMPACTION_ROWS,
            semconv::OURIOS_COMPACTION_ORPHAN_FILES,
            semconv::OURIOS_COMPACTION_DURATION,
            semconv::OURIOS_COMPACTION_IO,
            semconv::OURIOS_STORAGE_PARQUET_FILE_SIZE,
            semconv::OURIOS_COMPACTION_BACKLOG,
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "exported stream missing {expected}, got {names:?}",
            );
        }

        assert_sweeps_classified(&rms);

        // `io` splits the sweep's bytes by direction; the write volume is
        // the sum of every consolidated output (1024 + 2048 + 512 + 0).
        assert_eq!(io_volumes(&rms), (4096, 3584), "io read / write volumes");

        // The H4 detector aggregates per tenant: acme's two files merge
        // into one datapoint (count 2, Σ 1536), beta has one, and the
        // 0-byte ghost sample is dropped (no `ghost` datapoint).
        assert_file_size_histogram(&rms, &[("acme", 1024), ("acme", 512), ("beta", 2048)]);

        // The backlog reports each tenant's absolute lag: acme found 3
        // candidates but compacted 1 → 2; beta compacted both → 0.
        assert_backlog(&rms, &[("acme", 2), ("beta", 0)]);
    }
}
