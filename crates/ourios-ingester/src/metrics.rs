//! OpenTelemetry instruments for the compaction sweep (RFC 0009 §3.6).
//!
//! Names come from the generated [`ourios_semconv`] constants; the
//! instruments resolve through the **global** meter, per the RFC 0001
//! §6.8 API/SDK split (the SDK + OTLP exporter live in
//! `ourios-telemetry`, configured once by the binary). When no provider
//! is installed the global meter is a no-op, so constructing and
//! recording is always safe.
//!
//! This slice records the metric set that the existing
//! [`CompactionOutcome`](ourios_parquet::CompactionOutcome) already
//! exposes — sweeps, partitions, files, rows, orphan files, and sweep
//! duration. `ourios.compaction.io` (needs per-compaction byte counts),
//! `ourios.compaction.backlog`, and `ourios.storage.parquet.file.size`
//! are deferred to a follow-up that plumbs the additional data.

use std::time::Duration;

use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry::{KeyValue, global};
use ourios_semconv as semconv;

use crate::compactor::{IngestError, SweepReport, to_u64};

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

        // Only the attribute-free counters; `sweeps` carries a required
        // `result` attribute, so it is not zero-seeded here.
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

        if let Ok(report) = result {
            self.partitions
                .add(to_u64(report.partitions_compacted), &[]);
            self.files.add(report.files_compacted, &[]);
            self.rows.add(report.rows_compacted, &[]);
            self.orphan_files.add(to_u64(report.gc_failures), &[]);
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
    use opentelemetry_sdk::metrics::data::{
        AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
    };

    use super::*;

    // Collected metric names across the in-memory export.
    fn collected_names(rms: &[ResourceMetrics]) -> Vec<String> {
        rms.iter()
            .flat_map(ResourceMetrics::scope_metrics)
            .flat_map(ScopeMetrics::metrics)
            .map(|metric| metric.name().to_string())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn record_sweep_exports_the_compaction_metric_set() {
        // Arrange — an in-memory provider, then the instruments (built
        // after, so they resolve against it).
        let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");
        let metrics = CompactionMetrics::new();
        let report = SweepReport {
            tenants_scanned: 2,
            partitions_compacted: 3,
            files_compacted: 7,
            rows_compacted: 100,
            gc_failures: 1,
            errors: Vec::new(),
        };

        // Act
        metrics.record_sweep(&Ok(report), Duration::from_millis(50));
        guard.force_flush().expect("force_flush succeeds");

        // Assert — every recorded instrument is in the exported stream.
        let rms = exporter.get_finished_metrics().expect("metrics exported");
        let names = collected_names(&rms);
        for expected in [
            semconv::OURIOS_COMPACTION_SWEEPS,
            semconv::OURIOS_COMPACTION_PARTITIONS,
            semconv::OURIOS_COMPACTION_FILES,
            semconv::OURIOS_COMPACTION_ROWS,
            semconv::OURIOS_COMPACTION_ORPHAN_FILES,
            semconv::OURIOS_COMPACTION_DURATION,
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "exported stream missing {expected}, got {names:?}",
            );
        }

        // …and every `sweeps` datapoint carries the *required*
        // `ourios.compaction.result` attribute (the round-1 seed bug
        // emitted an attribute-less point), with the committed sweep
        // classified as such.
        let sweeps = rms
            .iter()
            .flat_map(ResourceMetrics::scope_metrics)
            .flat_map(ScopeMetrics::metrics)
            .find(|metric| metric.name() == semconv::OURIOS_COMPACTION_SWEEPS)
            .expect("sweeps metric present");
        let AggregatedMetrics::U64(MetricData::Sum(sum)) = sweeps.data() else {
            panic!("sweeps should be a u64 sum, got {:?}", sweeps.data());
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
}
