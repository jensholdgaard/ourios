//! The ingest + sink performance instruments (RFC 0014 §6.3) export under
//! their registry names, with the `flush.trigger` dimension.
//!
//! `IngestMetrics` / `SinkMetrics` resolve through the **global** meter, so
//! this lives in its own test binary (own global provider) rather than the
//! lib's compaction-metrics test, which installs its own global — two
//! global-installing tests in one binary would race.

use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
};
use ourios_ingester::metrics::{IngestMetrics, SinkMetrics};
use ourios_semconv as semconv;
use std::time::Duration;

fn names(rms: &[ResourceMetrics]) -> Vec<String> {
    rms.iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .map(|m| m.name().to_string())
        .collect()
}

fn data<'a>(rms: &'a [ResourceMetrics], name: &str) -> &'a AggregatedMetrics {
    rms.iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .find(|m| m.name() == name)
        .unwrap_or_else(|| panic!("metric {name} missing from the exported stream"))
        .data()
}

/// Sum of a u64 counter's datapoints whose `trigger` attribute equals `want`
/// (or all datapoints when `want` is `None`).
fn u64_sum(rms: &[ResourceMetrics], name: &str, want: Option<&str>) -> u64 {
    let AggregatedMetrics::U64(MetricData::Sum(sum)) = data(rms, name) else {
        panic!("{name} should be a u64 sum");
    };
    sum.data_points()
        .filter(|dp| match want {
            None => true,
            Some(t) => dp.attributes().any(|kv| {
                kv.key.as_str() == semconv::OURIOS_SINK_FLUSH_TRIGGER && kv.value.as_str() == t
            }),
        })
        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
        .sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn ingest_and_sink_metrics_export_under_their_registry_names() {
    // Arrange — install an in-memory global provider, then build the
    // instruments so they resolve against it.
    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");
    let ingest = IngestMetrics::new();
    let sink = SinkMetrics::new();

    // Act — record a representative slice of the perf signals.
    ingest.record_batch(10, 0, Duration::from_millis(2));
    sink.record_flush("size", 5, Duration::from_millis(7));
    sink.record_flush("rotation", 3, Duration::from_millis(9));
    sink.record_flush_error();
    sink.record_derive_error();
    sink.add_buffered(100);
    sink.add_buffered(-40);
    guard.force_flush().expect("force_flush");

    // Assert — every new instrument is in the exported stream.
    let rms = exporter.get_finished_metrics().expect("metrics exported");
    let collected = names(&rms);
    for expected in [
        semconv::OURIOS_INGEST_RECORDS,
        semconv::OURIOS_INGEST_BATCHES,
        semconv::OURIOS_WAL_APPEND_DURATION,
        semconv::OURIOS_SINK_FLUSH_DURATION,
        semconv::OURIOS_SINK_FLUSH_RECORDS,
        semconv::OURIOS_SINK_FLUSH_ERRORS,
        semconv::OURIOS_SINK_DERIVE_ERRORS,
        semconv::OURIOS_SINK_BUFFER_USAGE,
    ] {
        assert!(
            collected.iter().any(|n| n == expected),
            "exported stream missing {expected}, got {collected:?}",
        );
    }

    // Throughput counters carry the recorded totals.
    assert_eq!(u64_sum(&rms, semconv::OURIOS_INGEST_RECORDS, None), 10);
    assert_eq!(u64_sum(&rms, semconv::OURIOS_INGEST_BATCHES, None), 1);

    // `flush.records` splits by the required `trigger` dimension.
    assert_eq!(
        u64_sum(&rms, semconv::OURIOS_SINK_FLUSH_RECORDS, Some("size")),
        5,
        "size-triggered flush rows",
    );
    assert_eq!(
        u64_sum(&rms, semconv::OURIOS_SINK_FLUSH_RECORDS, Some("rotation")),
        3,
        "rotation-triggered flush rows",
    );
    assert!(
        u64_sum(&rms, semconv::OURIOS_SINK_FLUSH_RECORDS, Some("age")) == 0,
        "no age-triggered flush was recorded",
    );

    // Errors counted; buffer usage nets the two deltas.
    assert_eq!(u64_sum(&rms, semconv::OURIOS_SINK_FLUSH_ERRORS, None), 1);
    assert_eq!(u64_sum(&rms, semconv::OURIOS_SINK_DERIVE_ERRORS, None), 1);
    let AggregatedMetrics::I64(MetricData::Sum(usage)) =
        data(&rms, semconv::OURIOS_SINK_BUFFER_USAGE)
    else {
        panic!("buffer.usage should be an i64 sum (UpDownCounter)");
    };
    assert_eq!(
        usage
            .data_points()
            .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
            .sum::<i64>(),
        60,
        "buffer.usage nets +100 -40",
    );

    // The flush.duration histogram's count is the flush count (2).
    let AggregatedMetrics::F64(MetricData::Histogram(hist)) =
        data(&rms, semconv::OURIOS_SINK_FLUSH_DURATION)
    else {
        panic!("flush.duration should be an f64 histogram");
    };
    assert_eq!(
        hist.data_points()
            .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::count)
            .sum::<u64>(),
        2,
        "two flushes recorded (size + rotation)",
    );
}
