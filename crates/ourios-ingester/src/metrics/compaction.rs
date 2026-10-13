//! OpenTelemetry instruments for the compaction sweep (RFC 0009 §3.6).
//!
//! Names come from the generated [`ourios_semconv`] constants; the
//! instruments resolve through the **global** meter, per the RFC 0001
//! §6.8 API/SDK split (the SDK + OTLP exporter live in
//! `ourios-telemetry`, configured once by the binary). When no provider
//! is installed the global meter is a no-op, so constructing and
//! recording is always safe.
//!
//! Records the counters and histograms of RFC 0009 §3.6 — per committed
//! partition where the measurement is a partition's, per sweep otherwise:
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
//! guidance), which `record_sweep_outcome` keeps current from the per-tenant
//! candidate/compacted breakdown. This completes the §3.6 metric set.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{
    Counter, Histogram, Meter, ObservableCounter, ObservableUpDownCounter,
};
use ourios_core::histogram_buckets;
use ourios_parquet::MemoryPhase;
use ourios_semconv as semconv;

use crate::compactor::{
    FailedSweep, IngestError, PartitionCommitted, SweepGauges, SweepReport, to_u64,
};

/// Per-tenant current backlog (sealed-but-uncompacted partition count)
/// shared between [`CompactionMetrics::record_sweep_outcome`], which writes the
/// absolute value after each sweep, and the `ourios.compaction.backlog`
/// observable callback, which reads it at collection time.
type BacklogState = Arc<Mutex<HashMap<String, i64>>>;

/// The compaction metric instruments (RFC 0009 §3.6). Build one per
/// process; call [`CompactionMetrics::record_partition`] as each partition
/// commits and [`CompactionMetrics::record_sweep_outcome`] once per sweep.
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
    /// Current per-tenant backlog `record_sweep_outcome` keeps up to date; the
    /// observable counter's callback reads it.
    backlog_state: BacklogState,
    /// Held to keep the observable callback registered with the meter
    /// for this instrument's lifetime (the value is never read directly
    /// — the SDK invokes its callback on collect).
    #[expect(dead_code, reason = "retains the observable-callback registration")]
    backlog: ObservableUpDownCounter<i64>,
    /// `ourios.compaction.partition.input.files`, one sample per partition.
    input_files: Histogram<u64>,
    /// `ourios.compaction.partition.input.size`, both size kinds per partition.
    input_size: Histogram<u64>,
    /// What the running sweep keeps current; the live instruments read it.
    gauges: Arc<SweepGauges>,
    #[expect(dead_code, reason = "retains the observable-callback registrations")]
    live: LiveInstruments,
}

/// The observable instruments over [`SweepGauges`] (#925).
#[derive(Debug)]
#[expect(dead_code, reason = "retains the observable-callback registrations")]
struct LiveInstruments {
    spill_runs: ObservableCounter<u64>,
    spill_size: ObservableCounter<u64>,
    memory_usage: ObservableUpDownCounter<i64>,
    memory_limit: ObservableUpDownCounter<i64>,
    sweep_remaining: ObservableUpDownCounter<i64>,
}

impl LiveInstruments {
    /// Register the live instruments on `meter`, each reading `gauges` at
    /// collect time. `memory.limit` reports only the phases with a budget
    /// (see [`MemoryPhase`]), once a compaction has published it.
    fn register(meter: &Meter, gauges: &Arc<SweepGauges>) -> Self {
        let read = Arc::clone(gauges);
        let spill_runs = meter
            .u64_observable_counter(semconv::OURIOS_COMPACTION_SPILL_RUNS)
            .with_description(semconv::OURIOS_COMPACTION_SPILL_RUNS_DESCRIPTION)
            .with_unit(semconv::OURIOS_COMPACTION_SPILL_RUNS_UNIT)
            .with_callback(move |observer| observer.observe(read.compaction().spill_runs(), &[]))
            .build();
        let read = Arc::clone(gauges);
        let spill_size = meter
            .u64_observable_counter(semconv::OURIOS_COMPACTION_SPILL_SIZE)
            .with_description(semconv::OURIOS_COMPACTION_SPILL_SIZE_DESCRIPTION)
            .with_unit(semconv::OURIOS_COMPACTION_SPILL_SIZE_UNIT)
            .with_callback(move |observer| observer.observe(read.compaction().spill_bytes(), &[]))
            .build();
        let read = Arc::clone(gauges);
        let memory_usage = meter
            .i64_observable_up_down_counter(semconv::OURIOS_COMPACTION_MEMORY_USAGE)
            .with_description(semconv::OURIOS_COMPACTION_MEMORY_USAGE_DESCRIPTION)
            .with_unit(semconv::OURIOS_COMPACTION_MEMORY_USAGE_UNIT)
            .with_callback(move |observer| {
                for phase in MemoryPhase::ALL {
                    let usage = read.compaction().usage(phase);
                    observer.observe(saturating_i64(usage), &phase_attribute(phase));
                }
            })
            .build();
        let read = Arc::clone(gauges);
        let memory_limit = meter
            .i64_observable_up_down_counter(semconv::OURIOS_COMPACTION_MEMORY_LIMIT)
            .with_description(semconv::OURIOS_COMPACTION_MEMORY_LIMIT_DESCRIPTION)
            .with_unit(semconv::OURIOS_COMPACTION_MEMORY_LIMIT_UNIT)
            .with_callback(move |observer| {
                for phase in MemoryPhase::ALL {
                    if let Some(limit) = read.compaction().limit(phase) {
                        observer.observe(saturating_i64(limit), &phase_attribute(phase));
                    }
                }
            })
            .build();
        let read = Arc::clone(gauges);
        let sweep_remaining = meter
            .i64_observable_up_down_counter(semconv::OURIOS_COMPACTION_SWEEP_REMAINING)
            .with_description(semconv::OURIOS_COMPACTION_SWEEP_REMAINING_DESCRIPTION)
            .with_unit(semconv::OURIOS_COMPACTION_SWEEP_REMAINING_UNIT)
            .with_callback(move |observer| observer.observe(saturating_i64(read.remaining()), &[]))
            .build();
        Self {
            spill_runs,
            spill_size,
            memory_usage,
            memory_limit,
            sweep_remaining,
        }
    }
}

/// Bucket boundaries (bytes) for `ourios.storage.parquet.file.size`, the
/// RFC 0009 §3.6 H4 detector: it alerts when more than 5 % of files are
/// under 128 MiB, and the target band is 256 MiB–2 GiB inclusive, so all
/// three are edges. Explicit buckets are `(previous, edge]`, so a strict
/// or inclusive lower threshold sits one byte below its value.
const FILE_SIZE_BUCKETS: [f64; 12] = [
    65_536.0,        // 64 KiB
    262_144.0,       // 256 KiB
    1_048_576.0,     // 1 MiB
    4_194_304.0,     // 4 MiB
    16_777_216.0,    // 16 MiB
    67_108_864.0,    // 64 MiB
    134_217_727.0,   // 128 MiB − 1: the H4 alert counts files < 128 MiB
    268_435_455.0,   // 256 MiB − 1: a 256 MiB file is in the target band
    536_870_912.0,   // 512 MiB
    1_073_741_824.0, // 1 GiB
    2_147_483_648.0, // 2 GiB: the band's inclusive ceiling, already `(1 GiB, 2 GiB]`
    4_294_967_296.0, // 4 GiB
];

/// A registry metric's `(name, description, unit)` constants.
type MetricDef = (&'static str, &'static str, &'static str);

/// A `u64` counter for `def`.
fn counter(meter: &Meter, (name, description, unit): MetricDef) -> Counter<u64> {
    meter
        .u64_counter(name)
        .with_description(description)
        .with_unit(unit)
        .build()
}

/// A `u64` histogram for `def`, bucketed at `boundaries`.
fn histogram(
    meter: &Meter,
    (name, description, unit): MetricDef,
    boundaries: &[f64],
) -> Histogram<u64> {
    meter
        .u64_histogram(name)
        .with_description(description)
        .with_unit(unit)
        .with_boundaries(boundaries.to_vec())
        .build()
}

/// `ourios.compaction.backlog`, an **observable** (async) `UpDownCounter`: its
/// callback reports the *absolute* current per-tenant backlog at collect
/// time (OpenTelemetry additive-non-monotonic guidance). Reporting an absolute value
/// — not a per-sweep delta — is what keeps it from drifting when a
/// candidate errors one sweep and clears the next. `record_sweep_outcome`
/// keeps the returned state current; the callback only reads it.
fn backlog(meter: &Meter) -> (BacklogState, ObservableUpDownCounter<i64>) {
    let backlog_state: BacklogState = Arc::new(Mutex::new(HashMap::new()));
    let callback_state = Arc::clone(&backlog_state);
    let backlog = meter
        .i64_observable_up_down_counter(semconv::OURIOS_COMPACTION_BACKLOG)
        .with_description(semconv::OURIOS_COMPACTION_BACKLOG_DESCRIPTION)
        .with_unit(semconv::OURIOS_COMPACTION_BACKLOG_UNIT)
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
    (backlog_state, backlog)
}

fn phase_attribute(phase: MemoryPhase) -> [KeyValue; 1] {
    [KeyValue::new(
        semconv::OURIOS_COMPACTION_PHASE,
        phase.as_str(),
    )]
}

fn saturating_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

impl CompactionMetrics {
    /// Build the instruments on the `ourios.compaction` meter, with the
    /// UCUM units the registry (`metrics.yaml` in the shared `ourios-semconv` repo)
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
        Self::from_meter(&ourios_core::telemetry::meter("ourios.compaction"))
    }

    /// [`Self::new`] on `meter` rather than the global one.
    pub(crate) fn from_meter(meter: &Meter) -> Self {
        let partitions = counter(
            meter,
            (
                semconv::OURIOS_COMPACTION_PARTITIONS,
                semconv::OURIOS_COMPACTION_PARTITIONS_DESCRIPTION,
                semconv::OURIOS_COMPACTION_PARTITIONS_UNIT,
            ),
        );
        let files = counter(
            meter,
            (
                semconv::OURIOS_COMPACTION_FILES,
                semconv::OURIOS_COMPACTION_FILES_DESCRIPTION,
                semconv::OURIOS_COMPACTION_FILES_UNIT,
            ),
        );
        let rows = counter(
            meter,
            (
                semconv::OURIOS_COMPACTION_ROWS,
                semconv::OURIOS_COMPACTION_ROWS_DESCRIPTION,
                semconv::OURIOS_COMPACTION_ROWS_UNIT,
            ),
        );
        let orphan_files = counter(
            meter,
            (
                semconv::OURIOS_COMPACTION_ORPHAN_FILES,
                semconv::OURIOS_COMPACTION_ORPHAN_FILES_DESCRIPTION,
                semconv::OURIOS_COMPACTION_ORPHAN_FILES_UNIT,
            ),
        );
        // Only the attribute-free counters; `sweeps` and `io` carry
        // required attributes (`result` / `io.direction`) and `duration`
        // / `file_size` are histograms, so none are zero-seeded here —
        // they surface on the first sweep (`io` always, with a 0-byte
        // read/write point; the histograms only on real samples).
        for counter in [&partitions, &files, &rows, &orphan_files] {
            counter.add(0, &[]);
        }
        let (backlog_state, backlog) = backlog(meter);
        let gauges = Arc::new(SweepGauges::default());
        let live = LiveInstruments::register(meter, &gauges);

        Self {
            sweeps: counter(
                meter,
                (
                    semconv::OURIOS_COMPACTION_SWEEPS,
                    semconv::OURIOS_COMPACTION_SWEEPS_DESCRIPTION,
                    semconv::OURIOS_COMPACTION_SWEEPS_UNIT,
                ),
            ),
            partitions,
            files,
            rows,
            orphan_files,
            duration: meter
                .f64_histogram(semconv::OURIOS_COMPACTION_DURATION)
                .with_description(semconv::OURIOS_COMPACTION_DURATION_DESCRIPTION)
                .with_unit(semconv::OURIOS_COMPACTION_DURATION_UNIT)
                .with_boundaries(histogram_buckets::LONG_DURATION_S.to_vec())
                .build(),
            io: counter(
                meter,
                (
                    semconv::OURIOS_COMPACTION_IO,
                    semconv::OURIOS_COMPACTION_IO_DESCRIPTION,
                    semconv::OURIOS_COMPACTION_IO_UNIT,
                ),
            ),
            file_size: histogram(
                meter,
                (
                    semconv::OURIOS_STORAGE_PARQUET_FILE_SIZE,
                    semconv::OURIOS_STORAGE_PARQUET_FILE_SIZE_DESCRIPTION,
                    semconv::OURIOS_STORAGE_PARQUET_FILE_SIZE_UNIT,
                ),
                &FILE_SIZE_BUCKETS,
            ),
            backlog_state,
            backlog,
            input_files: histogram(
                meter,
                (
                    semconv::OURIOS_COMPACTION_PARTITION_INPUT_FILES,
                    semconv::OURIOS_COMPACTION_PARTITION_INPUT_FILES_DESCRIPTION,
                    semconv::OURIOS_COMPACTION_PARTITION_INPUT_FILES_UNIT,
                ),
                &histogram_buckets::FILE_COUNT,
            ),
            input_size: histogram(
                meter,
                (
                    semconv::OURIOS_COMPACTION_PARTITION_INPUT_SIZE,
                    semconv::OURIOS_COMPACTION_PARTITION_INPUT_SIZE_DESCRIPTION,
                    semconv::OURIOS_COMPACTION_PARTITION_INPUT_SIZE_UNIT,
                ),
                &histogram_buckets::SIZE_BYTES,
            ),
            gauges,
            live,
        }
    }

    /// What a sweep keeps current for the live instruments to read.
    #[must_use]
    pub fn gauges(&self) -> &SweepGauges {
        &self.gauges
    }

    /// Record one committed partition rewrite, the moment its manifest
    /// commits: `partitions`, `files`, `rows`, both `io` directions, and
    /// the H4 `file.size` sample. Recorded per partition, not per sweep, so
    /// a sweep working through a large backlog shows its progress while it
    /// runs.
    pub fn record_partition(&self, commit: &PartitionCommitted<'_>) {
        self.partitions.add(1, &[]);
        self.files.add(commit.files, &[]);
        self.rows.add(commit.rows, &[]);
        self.record_io(commit.bytes_read, commit.bytes_written);
        self.record_file_size(commit.tenant, commit.bytes_written);
        self.record_input(commit);
    }

    /// The partition's input shape: its file count, and its size both as
    /// stored and as the RFC 0036 §3.2 budget estimates it decoded.
    fn record_input(&self, commit: &PartitionCommitted<'_>) {
        self.input_files.record(commit.files, &[]);
        for (kind, bytes) in [
            ("encoded", commit.bytes_read),
            ("decoded_estimate", commit.decoded_bytes_estimate),
        ] {
            self.input_size.record(
                bytes,
                &[KeyValue::new(semconv::OURIOS_COMPACTION_SIZE_KIND, kind)],
            );
        }
    }

    /// Record one sweep's outcome and wall-clock `elapsed`, for a sweep
    /// whose committed partitions were each recorded through
    /// [`Self::record_partition`]: `sweeps`, `duration`, `orphan.files`,
    /// and the backlog. A sweep cut short by a fatal error still records
    /// what its partial report holds.
    pub fn record_sweep_outcome(
        &self,
        result: &Result<SweepReport, FailedSweep>,
        elapsed: Duration,
    ) {
        match result {
            Ok(report) => self.record_end(false, Some(report), elapsed),
            Err(failed) => self.record_end(true, failed.partial.as_deref(), elapsed),
        }
    }

    /// `sweeps`, `duration`, `orphan.files` and the backlog for a sweep that
    /// `failed` fatally or not, from its `report` when it has one. The
    /// `ourios.compaction.result` attribute classifies the sweep: `error`
    /// if it failed or any tenant/partition failed, else `committed` if
    /// anything was consolidated, else `noop`.
    fn record_end(&self, failed: bool, report: Option<&SweepReport>, elapsed: Duration) {
        let outcome = match report {
            _ if failed => "error",
            Some(report) if !report.errors.is_empty() => "error",
            Some(report) if report.partitions_compacted > 0 => "committed",
            _ => "noop",
        };
        let attrs = [KeyValue::new(semconv::OURIOS_COMPACTION_RESULT, outcome)];

        self.sweeps.add(1, &attrs);
        self.duration.record(elapsed.as_secs_f64(), &attrs);
        // Both directions surface on every sweep, a no-op one included.
        self.record_io(0, 0);

        // A sweep that failed before scanning the store yields no
        // per-tenant data, so the backlog map is deliberately left as-is:
        // the last-known lag is more honest than clearing to nothing (the
        // partitions didn't compact, so the backlog hasn't shrunk), and
        // the failure itself surfaces via `sweeps{result="error"}`.
        if let Some(report) = report {
            self.orphan_files.add(to_u64(report.gc_failures), &[]);
            self.record_backlog(report);
        }
    }

    /// Rebuild each tenant's *absolute* backlog (candidates the sweep found
    /// minus those it compacted) for the observable counter's callback.
    /// Absolute (not a delta), so a tenant that clears its lag reports 0
    /// next sweep rather than the value drifting up over time. A full
    /// `clear()` first means a tenant that no longer appears (data removed,
    /// or planning errored this sweep) stops being reported rather than
    /// emitting a stale value forever; `run_sweep` scans every tenant each
    /// pass, so the surviving set is current.
    fn record_backlog(&self, report: &SweepReport) {
        let mut backlog = self
            .backlog_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        backlog.clear();
        for t in &report.per_tenant {
            // candidates_found ≥ partitions_compacted (you can't compact
            // more than you found), so the lag is non-negative.
            let lag = i64::try_from(t.candidates_found.saturating_sub(t.partitions_compacted))
                .unwrap_or(i64::MAX);
            backlog.insert(t.tenant.clone(), lag);
        }
    }

    /// Record one whole sweep from its final report: the per-partition
    /// counters as the report's totals, then [`Self::record_sweep_outcome`].
    /// For a caller that did not record the sweep's partitions as they
    /// committed.
    pub fn record_sweep(&self, result: &Result<SweepReport, IngestError>, elapsed: Duration) {
        if let Ok(report) = result {
            self.partitions
                .add(to_u64(report.partitions_compacted), &[]);
            self.files.add(report.files_compacted, &[]);
            self.rows.add(report.rows_compacted, &[]);
            // The write volume is the sum of the consolidated output sizes
            // (saturating, matching `run_sweep`'s read accumulation).
            let bytes_written = report
                .compacted_files
                .iter()
                .fold(0_u64, |acc, f| acc.saturating_add(f.bytes));
            self.record_io(report.bytes_read, bytes_written);
            for file in &report.compacted_files {
                self.record_file_size(&file.tenant, file.bytes);
            }
        }
        self.record_end(result.is_err(), result.as_ref().ok(), elapsed);
    }

    /// Bytes moved, split by direction.
    fn record_io(&self, read: u64, written: u64) {
        self.io
            .add(read, &[KeyValue::new(semconv::OURIOS_IO_DIRECTION, "read")]);
        self.io.add(
            written,
            &[KeyValue::new(semconv::OURIOS_IO_DIRECTION, "write")],
        );
    }

    /// The H4 detector: one per-tenant sample per consolidated file, so
    /// the "> 5 % of files < 128 MiB" rule is a derived alert over this
    /// distribution (RFC 0009 §3.6). A `0` size is a best-effort `stat`
    /// failure (`file_len`), not a real file — skipped so the small-file
    /// distribution isn't skewed by a bogus zero-byte sample.
    fn record_file_size(&self, tenant: &str, bytes: u64) {
        if bytes > 0 {
            self.file_size.record(
                bytes,
                &[KeyValue::new(semconv::OURIOS_TENANT, tenant.to_string())],
            );
        }
    }
}

impl Default for CompactionMetrics {
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

    // Explicit buckets are `(previous, edge]` and samples are whole bytes.
    #[test]
    fn file_size_buckets_put_the_h4_thresholds_on_edges() {
        const MIB: f64 = 1_048_576.0;
        let has_edge = |edge: f64| FILE_SIZE_BUCKETS.contains(&edge);
        // RFC 0009 §3.6: the H4 alert counts files strictly under 128 MiB.
        assert!(has_edge(128.0 * MIB - 1.0));
        // RFC 0009 §3.6: a 256 MiB file is inside the 256 MiB–2 GiB band.
        assert!(has_edge(256.0 * MIB - 1.0));
        // RFC 0009 §3.6: a 2 GiB file is inside the band, too.
        assert!(has_edge(2_048.0 * MIB));
    }

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

    // A two-tenant sweep: acme lags, beta clears its backlog, and a ghost
    // tenant's 0-byte output stands in for a failed `stat`.
    fn sample_report() -> SweepReport {
        SweepReport {
            erasures_deferred: Vec::new(),
            erasures: Vec::new(),
            graph_tuples_emitted: 0,
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
        }
    }

    // Every compaction instrument the sweep recorded is in the export.
    fn assert_instruments_exported(rms: &[ResourceMetrics]) {
        let names = collected_names(rms);
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
    }

    /// Compaction instruments exported with the registry description and
    /// unit, through both the `counter` and `histogram` helpers.
    const REGISTERED: [ourios_telemetry::metadata_guard::Registered; 4] = [
        (
            semconv::OURIOS_COMPACTION_PARTITIONS,
            semconv::OURIOS_COMPACTION_PARTITIONS_DESCRIPTION,
            semconv::OURIOS_COMPACTION_PARTITIONS_UNIT,
        ),
        (
            semconv::OURIOS_COMPACTION_SWEEPS,
            semconv::OURIOS_COMPACTION_SWEEPS_DESCRIPTION,
            semconv::OURIOS_COMPACTION_SWEEPS_UNIT,
        ),
        (
            semconv::OURIOS_COMPACTION_IO,
            semconv::OURIOS_COMPACTION_IO_DESCRIPTION,
            semconv::OURIOS_COMPACTION_IO_UNIT,
        ),
        (
            semconv::OURIOS_STORAGE_PARQUET_FILE_SIZE,
            semconv::OURIOS_STORAGE_PARQUET_FILE_SIZE_DESCRIPTION,
            semconv::OURIOS_STORAGE_PARQUET_FILE_SIZE_UNIT,
        ),
    ];

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn record_sweep_exports_the_compaction_metric_set() {
        // Arrange — an in-memory provider, then the instruments (built
        // after, so they resolve against it).
        let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");
        let metrics = CompactionMetrics::new();
        let report = sample_report();

        // Act
        metrics.record_sweep(&Ok(report), Duration::from_millis(50));
        guard.force_flush().expect("force_flush succeeds");

        // Assert — every recorded instrument is in the exported stream.
        // (All assertions share this test's single in-memory provider:
        // `init_in_memory` installs the *global* meter, so two such tests
        // in one binary would race — one test, one provider.)
        let rms = exporter.get_finished_metrics().expect("metrics exported");
        assert_instruments_exported(&rms);
        ourios_telemetry::histogram_guard::assert_explicit_buckets(
            &rms,
            &[
                semconv::OURIOS_COMPACTION_DURATION,
                semconv::OURIOS_STORAGE_PARQUET_FILE_SIZE,
            ],
        );
        ourios_telemetry::metadata_guard::assert_registry_metadata(&rms, &REGISTERED);

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
