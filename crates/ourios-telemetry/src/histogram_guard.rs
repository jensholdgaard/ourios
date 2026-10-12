//! The #949 guard: no exported histogram may fall back to the SDK's
//! default buckets.
//!
//! Without `with_boundaries` the SDK buckets every histogram at
//! `[0, 5, 10, …, 10000]`, shaped for milliseconds; a seconds-scale
//! duration or a 0–1 ratio then lands in one bucket and every percentile
//! over it is meaningless. Each crate's metric test drives its histograms
//! through [`crate::init_in_memory`] and hands the export to
//! [`assert_explicit_buckets`].

use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, HistogramDataPoint, Metric, MetricData, ResourceMetrics, ScopeMetrics,
};

/// The specification's default explicit-bucket boundaries (Metrics SDK,
/// "Explicit Bucket Histogram Aggregation"), which the Rust SDK applies
/// when an instrument declares none.
pub const SDK_DEFAULT_BOUNDARIES: [f64; 15] = [
    0.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 250.0, 500.0, 750.0, 1_000.0, 2_500.0, 5_000.0,
    7_500.0, 10_000.0,
];

/// Assert that every histogram in `rms` declares its own boundaries, and
/// that each name in `expected` is among them, so a test cannot pass by
/// exporting no histogram at all.
///
/// # Panics
///
/// When an `expected` name is not exported as a histogram, or any exported
/// histogram data point carries [`SDK_DEFAULT_BOUNDARIES`].
pub fn assert_explicit_buckets(rms: &[ResourceMetrics], expected: &[&str]) {
    let histograms: Vec<(&str, Vec<Vec<f64>>)> = rms
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .filter_map(|metric| histogram_bounds(metric).map(|bounds| (metric.name(), bounds)))
        .collect();

    for name in expected {
        assert!(
            histograms.iter().any(|(exported, _)| exported == name),
            "histogram `{name}` was not exported; got {:?}",
            histograms.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        );
    }
    let defaulted: Vec<&str> = histograms
        .iter()
        .filter(|(_, points)| {
            points
                .iter()
                .any(|bounds| bounds[..] == SDK_DEFAULT_BOUNDARIES[..])
        })
        .map(|(name, _)| *name)
        .collect();
    assert!(
        defaulted.is_empty(),
        "histograms on the SDK default buckets (declare `with_boundaries`): {defaulted:?}",
    );
}

/// Each data point's boundaries, or `None` when `metric` is no histogram.
fn histogram_bounds(metric: &Metric) -> Option<Vec<Vec<f64>>> {
    fn points<T>(data: &MetricData<T>) -> Option<Vec<Vec<f64>>> {
        match data {
            MetricData::Histogram(hist) => Some(
                hist.data_points()
                    .map(|dp: &HistogramDataPoint<T>| dp.bounds().collect())
                    .collect(),
            ),
            _ => None,
        }
    }
    match metric.data() {
        AggregatedMetrics::F64(data) => points(data),
        AggregatedMetrics::U64(data) => points(data),
        AggregatedMetrics::I64(data) => points(data),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};

    fn export(declare: bool) -> Vec<ResourceMetrics> {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();
        let meter = provider.meter_with_scope(crate::scope("guard-test"));
        let builder = meter.f64_histogram("test.duration").with_unit("s");
        let histogram = if declare {
            builder.with_boundaries(vec![0.1, 1.0]).build()
        } else {
            builder.build()
        };
        histogram.record(0.5, &[]);
        provider.force_flush().expect("force_flush succeeds");
        exporter.get_finished_metrics().expect("metrics exported")
    }

    #[test]
    fn declared_boundaries_pass() {
        assert_explicit_buckets(&export(true), &["test.duration"]);
    }

    #[test]
    #[should_panic(expected = "SDK default buckets")]
    fn a_histogram_on_the_sdk_default_fails() {
        assert_explicit_buckets(&export(false), &["test.duration"]);
    }

    #[test]
    #[should_panic(expected = "was not exported")]
    fn a_missing_expected_histogram_fails() {
        assert_explicit_buckets(&export(true), &["test.absent"]);
    }
}
