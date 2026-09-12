//! A panicked cadence-sweep step is countable, and distinguishable from a
//! store error on the same counter (#791).
//!
//! Before this, a panic in the age sweep's step retired the flush cadence
//! for the life of the process with no log, no counter, and a `JoinHandle`
//! that still joined cleanly — so the only eventual symptom was an OOM
//! kill with no trail back to the cause.
//!
//! Its own test binary, like `perf_metrics.rs`: `SinkMetrics` resolves
//! through the **global** meter, and two global-installing tests in one
//! binary would race.

use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics, SumDataPoint,
};
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_parquet::store::Store;
use ourios_semconv as semconv;
use std::time::Duration;

/// Sum of the named u64 counter's datapoints, optionally restricted to
/// those carrying `attribute = value`.
fn counter_sum(rms: &[ResourceMetrics], name: &str, attribute: Option<(&str, &str)>) -> u64 {
    let metric = rms
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .find(|m| m.name() == name)
        .unwrap_or_else(|| panic!("metric {name} missing from the exported stream"));
    let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
        panic!("{name} should be a u64 sum");
    };
    sum.data_points()
        .filter(|dp| match attribute {
            None => true,
            Some((key, value)) => dp
                .attributes()
                .any(|kv| kv.key.as_str() == key && kv.value.as_str() == value),
        })
        .map(SumDataPoint::value)
        .sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_cadence_panic_is_counted_and_tagged_apart_from_a_store_error() {
    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");

    // A real `SharedParquetSink`, so what is asserted is the wiring the age
    // sweep calls rather than `SinkMetrics` in isolation. The bucket must
    // outlive the sink: dropping it removes the directory the store points at.
    let bucket = tempfile::TempDir::new().expect("bucket dir");
    let sink = SharedParquetSink::new(ParquetRecordSink::new(
        Store::local(bucket.path()).expect("local store"),
        FlushConfig {
            target_bytes: usize::MAX,
            max_buffer_age: Duration::from_secs(86_400),
            ceiling_bytes: usize::MAX,
        },
    ));

    sink.record_cadence_panic();
    guard.force_flush().expect("force_flush");

    let rms = exporter.get_finished_metrics().expect("metrics exported");
    // The dimension is the whole point: a dead age sweep and a transient
    // store failure share this counter, and an operator has to be able to
    // alert on one without the other.
    assert_eq!(
        counter_sum(
            &rms,
            semconv::OURIOS_SINK_FLUSH_ERRORS,
            Some(("error.type", "cadence_panic")),
        ),
        1,
        "the cadence panic is counted as error.type=cadence_panic",
    );
    assert_eq!(
        counter_sum(&rms, semconv::OURIOS_SINK_FLUSH_ERRORS, None),
        1,
        "and is the only flush error recorded here",
    );
}
