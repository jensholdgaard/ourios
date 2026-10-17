//! RFC 0001 §6.8 — per-tenant zero points on the lazily-created
//! counters, and the confidence buckets near 1.0. Each test builds its
//! own provider, so none touches the process-global one.

use std::collections::BTreeMap;

use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
};
use opentelemetry_sdk::metrics::{
    InMemoryMetricExporter, InMemoryMetricExporterBuilder, SdkMeterProvider, Temporality,
};

use ourios_core::tenant::TenantId;
use ourios_semconv as semconv;

use super::gauges::GaugeRegistry;
use super::{CONFIDENCE_BUCKETS, METER_NAME, MinerMetrics, PARSE_FAILURE_REASONS};

const COUNTERS: [&str; 4] = [
    semconv::OURIOS_MINER_MERGES,
    semconv::OURIOS_MINER_PARSE_FAILURES,
    semconv::OURIOS_MINER_PARAMS_OVERFLOW,
    semconv::OURIOS_MINER_TEMPLATE_VERSION_CHANGES,
];

struct Harness {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
    metrics: MinerMetrics,
}

impl Harness {
    fn new(temporality: Temporality) -> Self {
        let exporter = InMemoryMetricExporterBuilder::new()
            .with_temporality(temporality)
            .build();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();
        let metrics = MinerMetrics::with_registry(
            &provider.meter_with_scope(ourios_core::telemetry::scope(METER_NAME)),
            &GaugeRegistry::default(),
        );
        Self {
            provider,
            exporter,
            metrics,
        }
    }

    fn collect(&self) -> Vec<ResourceMetrics> {
        self.exporter.reset();
        self.provider.force_flush().unwrap();
        self.exporter.get_finished_metrics().unwrap()
    }

    /// Every counter point, by metric name and then by the point's
    /// sorted `key=value` attributes.
    fn counters(&self) -> BTreeMap<String, BTreeMap<String, u64>> {
        let mut out: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
        for metric in self
            .collect()
            .iter()
            .flat_map(ResourceMetrics::scope_metrics)
            .flat_map(ScopeMetrics::metrics)
            .filter(|metric| COUNTERS.contains(&metric.name()))
        {
            let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
                panic!("{} is a u64 sum", metric.name());
            };
            let points = out.entry(metric.name().to_owned()).or_default();
            for dp in sum.data_points() {
                points.insert(attrs_key(dp.attributes()), dp.value());
            }
        }
        out
    }
}

fn attrs_key<'a>(attrs: impl Iterator<Item = &'a opentelemetry::KeyValue>) -> String {
    let mut pairs: Vec<String> = attrs
        .map(|kv| format!("{}={}", kv.key, kv.value.as_str()))
        .collect();
    pairs.sort();
    pairs.join(",")
}

/// The zero points RFC 0001 §6.8 names for `tenant`.
fn zero_points(tenant: &str) -> BTreeMap<String, BTreeMap<String, u64>> {
    let t = format!("{}={tenant}", semconv::OURIOS_TENANT);
    let with = |key: &str, value: &str| {
        let mut pair = [format!("{key}={value}"), t.clone()];
        pair.sort();
        (pair.join(","), 0)
    };
    BTreeMap::from([
        (
            semconv::OURIOS_MINER_MERGES.to_owned(),
            ["template_widened", "template_type_expanded"]
                .into_iter()
                .map(|change| with(semconv::OURIOS_MINER_TEMPLATE_CHANGE, change))
                .collect(),
        ),
        (
            semconv::OURIOS_MINER_PARSE_FAILURES.to_owned(),
            PARSE_FAILURE_REASONS
                .into_iter()
                .map(|reason| with(semconv::OURIOS_MINER_PARSE_FAILURE_REASON, reason))
                .collect(),
        ),
        (
            semconv::OURIOS_MINER_PARAMS_OVERFLOW.to_owned(),
            BTreeMap::from([(t.clone(), 0)]),
        ),
        (
            semconv::OURIOS_MINER_TEMPLATE_VERSION_CHANGES.to_owned(),
            BTreeMap::from([(t.clone(), 0)]),
        ),
    ])
}

#[test]
fn a_tenants_first_line_records_zero_on_each_counter() {
    let harness = Harness::new(Temporality::Cumulative);
    harness
        .metrics
        .record_line_denominator(&TenantId::new("acme"), Some("checkout"));

    assert_eq!(harness.counters(), zero_points("acme"));
}

#[test]
fn the_parse_failure_reasons_are_the_registry_enum() {
    assert_eq!(
        PARSE_FAILURE_REASONS,
        [
            "below_floor",
            "empty_line",
            "tokenizer_failure",
            "degenerate_widening",
            "line_too_long",
            "template_ceiling",
            "id_reservation_failed",
        ]
    );
}

/// Under delta temporality a series only reappears when something was
/// recorded on it since the last collection.
#[test]
fn zero_points_are_recorded_once_per_tenant() {
    let harness = Harness::new(Temporality::Delta);
    let acme = TenantId::new("acme");
    harness.metrics.record_line_denominator(&acme, None);
    assert_eq!(harness.counters(), zero_points("acme"));

    harness.metrics.record_line_denominator(&acme, None);
    harness
        .metrics
        .record_line_denominator(&acme, Some("checkout"));
    assert_eq!(harness.counters(), BTreeMap::new(), "acme is not new");

    harness
        .metrics
        .record_line_denominator(&TenantId::new("globex"), None);
    assert_eq!(harness.counters(), zero_points("globex"));
}

#[test]
fn a_real_merge_counts_on_its_members_zero_series() {
    let harness = Harness::new(Temporality::Cumulative);
    let acme = TenantId::new("acme");
    harness.metrics.record_line_denominator(&acme, None);
    harness.metrics.record_merge(&acme, "template_widened");

    let mut expected = zero_points("acme");
    for (name, points) in &mut expected {
        if name == semconv::OURIOS_MINER_MERGES {
            for (attrs, value) in points.iter_mut() {
                if attrs.contains("template_widened") {
                    *value = 1;
                }
            }
        }
        if name == semconv::OURIOS_MINER_TEMPLATE_VERSION_CHANGES {
            for value in points.values_mut() {
                *value = 1;
            }
        }
    }
    assert_eq!(harness.counters(), expected);
}

/// `0.99` and `0.999` split the values just under the 1.0 decision boundary.
#[test]
fn the_buckets_resolve_values_just_under_the_decision_boundary() {
    let harness = Harness::new(Temporality::Cumulative);
    let acme = TenantId::new("acme");
    harness.metrics.record_line(&acme, None, 1.0);
    harness.metrics.record_line(&acme, None, 0.995);

    let collected = harness.collect();
    let histogram = collected
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .find(|metric| metric.name() == semconv::OURIOS_MINER_CONFIDENCE)
        .expect("the confidence histogram");
    let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = histogram.data() else {
        panic!("confidence is an f64 histogram");
    };
    let point = histogram.data_points().next().expect("one point");
    let bounds: Vec<f64> = point.bounds().collect();
    assert_eq!(bounds, CONFIDENCE_BUCKETS);
    let counts: Vec<u64> = point.bucket_counts().collect();
    let boundary = bounds
        .iter()
        .position(|bound| (*bound - 1.0).abs() < f64::EPSILON)
        .expect("a 1.0 boundary");
    assert!((bounds[boundary - 1] - 0.999).abs() < f64::EPSILON);
    assert_eq!(counts[boundary], 1, "1.0 in (0.999, 1.0]: {counts:?}");
    assert_eq!(
        counts[boundary - 1],
        1,
        "0.995 in (0.99, 0.999]: {counts:?}"
    );
}
