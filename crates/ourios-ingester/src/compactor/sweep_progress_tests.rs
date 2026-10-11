//! #925 sweep-progress and partition-input metrics: `sweep.remaining`
//! counting down through a sweep, the per-tenant counting pass behind it,
//! and each compacted partition's input shape.

use std::sync::Arc;

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use ourios_core::audit::{AuditEvent, AuditSink};
use ourios_parquet::CompactionPolicy;

use super::tests::{store_at, target, u64_sum, write_sealed_candidate, write_sealed_hours};
use super::{Countdown, SweepGauges, count_candidates, now_unix_nanos, sweep_recorded};
use crate::metrics::CompactionMetrics;

/// Reads `sweep.remaining` through a fresh collection at every event the
/// sweep emits.
struct RemainingSink {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
    readings: Arc<std::sync::Mutex<Vec<i64>>>,
}

/// The `i64` sum datapoints of `name` the exporter last collected, keyed by
/// their `ourios.compaction.phase` (empty when the metric has none).
fn i64_points(rms: &[ResourceMetrics], name: &str) -> Vec<(String, i64)> {
    rms.iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .filter(|m| m.name() == name)
        .flat_map(|m| match m.data() {
            AggregatedMetrics::I64(MetricData::Sum(sum)) => sum
                .data_points()
                .map(|dp| {
                    let phase = dp
                        .attributes()
                        .find(|kv| kv.key.as_str() == ourios_semconv::OURIOS_COMPACTION_PHASE)
                        .map(|kv| kv.value.as_str().to_string())
                        .unwrap_or_default();
                    (phase, dp.value())
                })
                .collect::<Vec<_>>(),
            other => panic!("{name} is not an i64 sum: {other:?}"),
        })
        .collect()
}

impl AuditSink for RemainingSink {
    fn emit(&mut self, _: AuditEvent) {
        self.exporter.reset();
        self.provider.force_flush().expect("flush");
        let rms = self.exporter.get_finished_metrics().expect("collect");
        let remaining = i64_points(&rms, ourios_semconv::OURIOS_COMPACTION_SWEEP_REMAINING);
        self.readings
            .lock()
            .expect("readings")
            .extend(remaining.into_iter().map(|(_, value)| value));
    }
}

/// `(count, sum)` of the u64 histogram `name`'s datapoint whose attributes
/// include `attribute`, if any.
fn histogram(
    rms: &[ResourceMetrics],
    name: &str,
    attribute: Option<(&str, &str)>,
) -> Option<(u64, u64)> {
    rms.iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .filter(|m| m.name() == name)
        .find_map(|m| match m.data() {
            AggregatedMetrics::U64(MetricData::Histogram(hist)) => hist
                .data_points()
                .find(|dp| {
                    attribute.is_none_or(|(key, value)| {
                        dp.attributes()
                            .any(|kv| kv.key.as_str() == key && kv.value.as_str() == value)
                    })
                })
                .map(|dp| (dp.count(), dp.sum())),
            other => panic!("{name} is not a u64 histogram: {other:?}"),
        })
}

/// #925 — `sweep.remaining` counts a multi-tenant sweep's candidates down
/// as each is processed and is zero once the sweep ends; every compacted
/// partition records its input file count and both input sizes; the
/// budgets are exported per phase and no usage outlives the sweep.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sweep_remaining_counts_down_and_each_partition_records_its_input() {
    // Arrange
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_sealed_hours(&store, 3);
    write_sealed_candidate(&store, "b");
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .build();
    let metrics = Arc::new(CompactionMetrics::from_meter(
        &provider.meter_with_scope(ourios_core::telemetry::scope("ourios.compaction")),
    ));
    let readings = Arc::default();
    let sink = RemainingSink {
        provider: provider.clone(),
        exporter: exporter.clone(),
        readings: Arc::clone(&readings),
    };

    // Act
    let (result, _, _) = sweep_recorded(target(&store), Box::new(sink), Some(metrics)).await;
    exporter.reset();
    provider.force_flush().expect("flush");
    let rms = exporter.get_finished_metrics().expect("collect");

    // Assert
    let report = result.expect("sweep");
    assert_eq!(report.partitions_compacted, 4, "{report:?}");
    let remaining = readings.lock().expect("readings").clone();
    assert_eq!(
        remaining,
        vec![4, 3, 2, 1],
        "one candidate fewer per commit"
    );
    let name = ourios_semconv::OURIOS_COMPACTION_SWEEP_REMAINING;
    assert_eq!(i64_points(&rms, name), vec![(String::new(), 0)]);
    let input = |name, attribute| histogram(&rms, name, attribute).expect(name);
    let files = ourios_semconv::OURIOS_COMPACTION_PARTITION_INPUT_FILES;
    let size = ourios_semconv::OURIOS_COMPACTION_PARTITION_INPUT_SIZE;
    ourios_telemetry::histogram_guard::assert_explicit_buckets(&rms, &[files, size]);
    assert_eq!(input(files, None), (4, 8), "two input files per partition");
    let kind = ourios_semconv::OURIOS_COMPACTION_SIZE_KIND;
    let (encoded, decoded) = (
        input(size, Some((kind, "encoded"))),
        input(size, Some((kind, "decoded_estimate"))),
    );
    assert_eq!(
        (encoded.0, decoded.0, encoded.1, decoded.1 > 0),
        (4, 4, report.bytes_read, true),
        "both sizes per partition; encoded is the bytes read",
    );
    let mut limits = i64_points(&rms, ourios_semconv::OURIOS_COMPACTION_MEMORY_LIMIT);
    limits.sort();
    let phases: Vec<&str> = limits.iter().map(|(phase, _)| phase.as_str()).collect();
    assert_eq!(phases, ["merge", "sort", "write"], "the budgeted phases");
    assert!(limits.iter().all(|&(_, limit)| limit > 0), "{limits:?}");
    let usage = i64_points(&rms, ourios_semconv::OURIOS_COMPACTION_MEMORY_USAGE);
    assert_eq!(usage.len(), 4, "every phase: {usage:?}");
    assert!(usage.iter().all(|&(_, bytes)| bytes == 0), "{usage:?}");
    assert_eq!(
        (
            u64_sum(&rms, ourios_semconv::OURIOS_COMPACTION_SPILL_RUNS),
            u64_sum(&rms, ourios_semconv::OURIOS_COMPACTION_SPILL_SIZE),
        ),
        (0, 0),
        "a partition within the budget never spills",
    );
}

/// #925 — the counting pass keeps one number per tenant, never the
/// candidates: tenant `a` has three sealed candidates, `b` one, and a
/// tenant with no data none.
#[test]
fn the_counting_pass_keeps_only_per_tenant_counts() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_sealed_hours(&store, 3);
    write_sealed_candidate(&store, "b");
    let tenants = ["a", "b", "none"].map(str::to_string);

    let counts: Vec<usize> = count_candidates(
        &store,
        &tenants,
        now_unix_nanos(),
        &CompactionPolicy::default(),
    );

    assert_eq!(counts, vec![3, 1, 0]);
}

/// Drive a countdown over `counted` per-tenant counts: each tenant is
/// planned with its `planned` candidates and ticks once per candidate.
/// Returns `remaining` at the start, after every step, and once dropped.
fn countdown_readings(counted: Vec<usize>, planned: &[usize]) -> Vec<u64> {
    let gauges = SweepGauges::default();
    let mut readings = Vec::new();
    {
        let mut countdown = Countdown::start(Some(&gauges), || counted);
        readings.push(gauges.remaining());
        for &candidates in planned {
            countdown.begin_tenant(candidates);
            readings.push(gauges.remaining());
            for _ in 0..candidates {
                countdown.tick();
                readings.push(gauges.remaining());
            }
        }
    }
    readings.push(gauges.remaining());
    readings
}

/// The countdown starts at the counted total, gives up candidates a
/// tenant no longer has when it is planned, ticks down per candidate, and
/// is zero once dropped.
#[test]
fn the_countdown_drops_candidates_a_replanned_tenant_no_longer_has() {
    let readings = countdown_readings(vec![2, 3], &[2, 1]);

    assert_eq!(readings, vec![5, 5, 4, 3, 1, 0, 0]);
}

/// A tenant planned with more candidates than counted never raises the
/// count: the uncounted ones still compact, and the count floors at zero.
#[test]
fn the_countdown_never_rises_when_a_tenant_finds_more_than_counted() {
    let readings = countdown_readings(vec![1, 1], &[3, 1]);

    assert!(
        readings.windows(2).all(|pair| pair[1] <= pair[0]),
        "{readings:?}"
    );
    assert_eq!(readings, vec![2, 2, 1, 0, 0, 0, 0, 0]);
}

/// Without gauges the counting pass never runs.
#[test]
fn an_unmeasured_sweep_skips_the_counting_pass() {
    let mut counted = false;

    let _countdown = Countdown::start(None, || {
        counted = true;
        Vec::new()
    });

    assert!(!counted);
}
