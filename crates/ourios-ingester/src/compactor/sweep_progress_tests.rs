//! #925 sweep-progress and partition-input metrics: `sweep.remaining`
//! counting down through a sweep, the one planning pass behind it, and each
//! compacted partition's input shape.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use ourios_core::audit::{AuditEvent, AuditSink};
use ourios_parquet::CompactionPolicy;

use super::tests::{
    HOUR, TS0, store_at, target, u64_sum, write_file, write_sealed_candidate, write_sealed_hours,
};
use super::{Countdown, SweepGauges, now_unix_nanos, plan_tenants, sweep_recorded};
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

/// `(count, sum)` over the u64 histogram `name`'s datapoints whose
/// attributes include `attribute`, summed across the tenants they are split
/// by (#1016), if any.
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
                .filter(|dp| {
                    attribute.is_none_or(|(key, value)| {
                        dp.attributes()
                            .any(|kv| kv.key.as_str() == key && kv.value.as_str() == value)
                    })
                })
                .map(|dp| (dp.count(), dp.sum()))
                .reduce(|(c, s), (dc, ds)| (c + dc, s + ds)),
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

/// #1019 — the sweep plans each tenant once: tenant `a` has three sealed
/// candidates, `b` one, and a tenant with no data none.
#[test]
fn the_planning_pass_plans_each_tenant_once() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_sealed_hours(&store, 3);
    write_sealed_candidate(&store, "b");
    let tenants = ["a", "b", "none"].map(str::to_string);

    let plans = plan_tenants(
        &store,
        &tenants,
        now_unix_nanos(),
        &CompactionPolicy::default(),
    );

    let counts: Vec<(&str, usize)> = plans
        .iter()
        .map(|(tenant, planned)| (*tenant, planned.as_ref().expect("planned").len()))
        .collect();
    assert_eq!(counts, vec![("a", 3), ("b", 1), ("none", 0)]);
}

/// The countdown starts at the planned total, ticks down per candidate,
/// never below zero, and is zero once dropped.
#[test]
fn the_countdown_ticks_from_the_plan_to_zero() {
    let gauges = SweepGauges::default();
    let mut readings = Vec::new();
    {
        let countdown = Countdown::start(Some(&gauges), 2);
        readings.push(gauges.remaining());
        for _ in 0..3 {
            countdown.tick();
            readings.push(gauges.remaining());
        }
    }
    readings.push(gauges.remaining());

    assert_eq!(readings, vec![2, 1, 0, 0, 0]);
}

/// Counts the store's reads and listings, passing every call through.
#[derive(Debug)]
struct CountingStore {
    inner: Arc<dyn object_store::ObjectStore>,
    reads: Arc<AtomicUsize>,
    lists: Arc<AtomicUsize>,
}

impl std::fmt::Display for CountingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CountingStore({})", self.inner)
    }
}

#[async_trait::async_trait]
impl object_store::ObjectStore for CountingStore {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &object_store::path::Path,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &object_store::path::Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<
            'static,
            object_store::Result<object_store::path::Path>,
        >,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.lists.fetch_add(1, Ordering::Relaxed);
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.lists.fetch_add(1, Ordering::Relaxed);
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// `(reads, listings)` one sweep makes over three settled hour partitions
/// (one file each, so nothing to compact), measured or not.
async fn settled_sweep_requests(measured: bool) -> (usize, usize) {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    for hour in 0..3 {
        write_file(&store, "a", 1, TS0 + hour * HOUR);
    }
    let (reads, lists) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let store = store.wrap_backend(|inner| {
        Arc::new(CountingStore {
            inner,
            reads: Arc::clone(&reads),
            lists: Arc::clone(&lists),
        })
    });
    let metrics = measured.then(|| {
        let provider = SdkMeterProvider::builder().build();
        Arc::new(CompactionMetrics::from_meter(&provider.meter_with_scope(
            ourios_core::telemetry::scope("ourios.compaction"),
        )))
    });
    let sink = Box::new(ourios_core::audit::NoOpAuditSink);

    let (result, _, _) = sweep_recorded(target(&store), sink, metrics).await;

    assert_eq!(result.expect("sweep").partitions_compacted, 0);
    (reads.load(Ordering::Relaxed), lists.load(Ordering::Relaxed))
}

/// #1019 — measuring `sweep.remaining` costs no extra requests: a measured
/// sweep reads and lists exactly what an unmeasured one does, because both
/// plan each tenant once. Before, the measured sweep planned every tenant a
/// second time just to count, doubling the dominant cost of a sweep with
/// nothing to do.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_measured_sweep_plans_each_partition_once() {
    let unmeasured = settled_sweep_requests(false).await;
    let measured = settled_sweep_requests(true).await;

    assert!(
        unmeasured.1 > 0,
        "planning lists the tenant: {unmeasured:?}"
    );
    assert_eq!(
        unmeasured.0, 0,
        "a settled partition costs no read (RFC0009.8): {unmeasured:?}"
    );
    assert_eq!(measured, unmeasured);
}
