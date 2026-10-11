//! #945: the gauges are registered once per provider and report only
//! the clusters still alive. Every test builds its own provider rather
//! than the process-global one, so they cannot race each other.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use proptest::prelude::*;

use ourios_core::tenant::TenantId;
use ourios_semconv as semconv;

use super::super::{METER_NAME, MinerMetrics, MinerMetricsState};
use super::{Binding, GaugeRegistry, ObservableGauges, register, registry_for};

/// A local provider with the gauges registered once over a fresh
/// registry, as `global_registry` does for the global provider.
struct Harness {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
    registry: Arc<GaugeRegistry>,
    _gauges: ObservableGauges,
}

impl Harness {
    fn new() -> Self {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();
        let registry = Arc::new(GaugeRegistry::default());
        let gauges = register(&provider.meter(METER_NAME), &registry);
        Self {
            provider,
            exporter,
            registry,
            _gauges: gauges,
        }
    }

    fn cluster_metrics(&self) -> MinerMetrics {
        MinerMetrics::with_registry(&self.provider.meter(METER_NAME), &self.registry)
    }

    /// One collection: every gauge point, keyed by metric name and then
    /// by the point's sorted `key=value` attributes.
    fn collect(&self) -> BTreeMap<String, BTreeMap<String, f64>> {
        self.exporter.reset();
        self.provider.force_flush().unwrap();
        gauge_points(&self.exporter.get_finished_metrics().unwrap())
    }
}

#[allow(clippy::cast_precision_loss)] // test counts are tiny
fn gauge_points(rms: &[ResourceMetrics]) -> BTreeMap<String, BTreeMap<String, f64>> {
    let mut out: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    for metric in rms
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
    {
        let points = out.entry(metric.name().to_owned()).or_default();
        match metric.data() {
            AggregatedMetrics::U64(MetricData::Gauge(g)) => {
                for dp in g.data_points() {
                    points.insert(attrs_key(dp.attributes()), dp.value() as f64);
                }
            }
            AggregatedMetrics::F64(MetricData::Gauge(g)) => {
                for dp in g.data_points() {
                    points.insert(attrs_key(dp.attributes()), dp.value());
                }
            }
            _ => {}
        }
    }
    out
}

fn attrs_key<'a>(attrs: impl Iterator<Item = &'a opentelemetry::KeyValue>) -> String {
    let mut kv: Vec<String> = attrs
        .map(|kv| format!("{}={}", kv.key, kv.value.as_str()))
        .collect();
    kv.sort();
    kv.join(",")
}

fn tenant_key(tenant: &str) -> String {
    format!("{}={tenant}", semconv::OURIOS_TENANT)
}

fn weak_state(metrics: &MinerMetrics) -> Weak<Mutex<MinerMetricsState>> {
    Arc::downgrade(&metrics.state)
}

/// Drive one cluster's state the way `MinerCluster::ingest` does.
fn feed(metrics: &MinerMetrics, tenant: &TenantId, templates: u64) {
    metrics.record_line_denominator(tenant, Some("svc"));
    metrics.record_line(tenant, Some("svc"), 0.9);
    metrics.set_template_count(tenant, templates);
}

/// Acceptance 1: dropped clusters free their state, leave the
/// registry, and stop reporting; only the live ones are collected.
#[test]
fn dropped_clusters_free_state_and_stop_reporting() {
    const BUILT: u64 = 200;
    let h = Harness::new();
    let mut live = Vec::new();
    let mut dropped = Vec::new();
    for i in 0..BUILT {
        let metrics = h.cluster_metrics();
        feed(&metrics, &TenantId::new(format!("t{i}")), i + 1);
        if i < 2 {
            live.push(metrics);
        } else {
            dropped.push(weak_state(&metrics));
        }
    }

    assert!(dropped.iter().all(|w| w.strong_count() == 0));
    let points = h.collect();
    assert_eq!(lock_len(&h.registry), live.len(), "dead entries pruned");

    let counts = &points[semconv::OURIOS_MINER_TEMPLATE_COUNT];
    let expected: BTreeMap<String, f64> = [(tenant_key("t0"), 1.0), (tenant_key("t1"), 2.0)]
        .into_iter()
        .collect();
    assert_eq!(counts, &expected, "only the live clusters report");
    for name in [
        semconv::OURIOS_MINER_BODY_RETENTION_UTILIZATION,
        semconv::OURIOS_MINER_PARAMS_OVERFLOW_UTILIZATION,
        semconv::OURIOS_MINER_CONFIDENCE_P50,
        semconv::OURIOS_MINER_CONFIDENCE_P01,
    ] {
        assert_eq!(
            points[name].len(),
            live.len(),
            "{name} reports live clusters only"
        );
    }
}

/// With no collection at all (the no-op provider never runs the
/// callbacks), registration alone keeps the registry bounded.
#[test]
fn registration_prunes_without_collection() {
    let registry = GaugeRegistry::default();
    let keep = Arc::new(Mutex::new(MinerMetricsState::default()));
    registry.register(&keep);
    for _ in 0..10_000 {
        let state = Arc::new(Mutex::new(MinerMetricsState::default()));
        registry.register(&state);
    }
    assert!(
        lock_len(&registry) <= 2 * super::MIN_PRUNE_AT,
        "registry grew to {}",
        lock_len(&registry),
    );
}

/// Acceptance 2: a live cluster's five gauges read exactly what the
/// per-cluster registration reported.
#[test]
fn live_cluster_gauge_values_are_unchanged() {
    let h = Harness::new();
    let metrics = h.cluster_metrics();
    let tenant = TenantId::new("acme");
    for (service, confidence, overflow, retained) in [
        (Some("checkout"), 0.2, 1, true),
        (Some("checkout"), 0.6, 0, false),
        (Some("checkout"), 1.0, 0, false),
        (Some("checkout"), 0.8, 0, false),
        (None, 0.5, 0, false),
    ] {
        metrics.record_line_denominator(&tenant, service);
        metrics.record_line(&tenant, service, confidence);
        metrics.record_overflow(&tenant, service, overflow);
        if retained {
            metrics.record_body_retention(&tenant);
        }
    }
    metrics.set_template_count(&tenant, 7);

    let points = h.collect();
    let checkout = format!(
        "{}=checkout,{}",
        semconv::OURIOS_SERVICE,
        tenant_key("acme")
    );
    let acme = tenant_key("acme");
    let expect = |name: &str, key: &str, value: f64| {
        let got = points[name][key];
        assert!(
            (got - value).abs() < 1e-12,
            "{name}{{{key}}} = {got}, want {value}"
        );
    };
    expect(semconv::OURIOS_MINER_TEMPLATE_COUNT, &acme, 7.0);
    expect(semconv::OURIOS_MINER_BODY_RETENTION_UTILIZATION, &acme, 0.2);
    expect(
        semconv::OURIOS_MINER_PARAMS_OVERFLOW_UTILIZATION,
        &checkout,
        0.25,
    );
    expect(
        semconv::OURIOS_MINER_PARAMS_OVERFLOW_UTILIZATION,
        &acme,
        0.0,
    );
    // Nearest-rank over {0.2, 0.6, 0.8, 1.0}: p50 = 2nd, p01 = 1st.
    expect(semconv::OURIOS_MINER_CONFIDENCE_P50, &checkout, 0.6);
    expect(semconv::OURIOS_MINER_CONFIDENCE_P01, &checkout, 0.2);
    expect(semconv::OURIOS_MINER_CONFIDENCE_P50, &acme, 0.5);
    expect(semconv::OURIOS_MINER_CONFIDENCE_P01, &acme, 0.5);
    assert_eq!(points[semconv::OURIOS_MINER_TEMPLATE_COUNT].len(), 1);
    assert_eq!(
        points[semconv::OURIOS_MINER_PARAMS_OVERFLOW_UTILIZATION].len(),
        2
    );
}

/// One registration per provider: the same provider reuses its
/// registry, a newly installed one gets its own, and a cluster keeps
/// reporting to the provider it was built under.
#[test]
fn binding_follows_the_provider() {
    let slot: Mutex<Option<Binding>> = Mutex::new(None);
    let (first, first_exporter) = sdk_provider();
    let first_sdk = first.clone();
    let first: Arc<dyn MeterProvider + Send + Sync> = Arc::new(first);
    let a = registry_for(&first, &slot);
    assert!(Arc::ptr_eq(&a, &registry_for(&first, &slot)));

    let early = MinerMetrics::with_registry(&first.meter(METER_NAME), &a);
    early.set_template_count(&TenantId::new("early"), 1);

    let (second, second_exporter) = sdk_provider();
    let second_sdk = second.clone();
    let second: Arc<dyn MeterProvider + Send + Sync> = Arc::new(second);
    let b = registry_for(&second, &slot);
    assert!(!Arc::ptr_eq(&a, &b));
    let late = MinerMetrics::with_registry(&second.meter(METER_NAME), &b);
    late.set_template_count(&TenantId::new("late"), 2);

    for (sdk, exporter, tenant) in [
        (&first_sdk, &first_exporter, "early"),
        (&second_sdk, &second_exporter, "late"),
    ] {
        sdk.force_flush().unwrap();
        let points = gauge_points(&exporter.get_finished_metrics().unwrap());
        let counts = &points[semconv::OURIOS_MINER_TEMPLATE_COUNT];
        assert_eq!(counts.keys().collect::<Vec<_>>(), [&tenant_key(tenant)]);
    }
}

fn sdk_provider() -> (SdkMeterProvider, InMemoryMetricExporter) {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .build();
    (provider, exporter)
}

fn lock_len(registry: &GaugeRegistry) -> usize {
    registry.entries.lock().unwrap().states.len()
}

#[derive(Debug, Clone)]
enum Op {
    Build(u64),
    Drop(usize),
    Collect,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (1u64..100).prop_map(Op::Build),
        any::<usize>().prop_map(Op::Drop),
        Just(Op::Collect),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Acceptance 3: under any interleaving of build, drop and collect,
    /// a collection reports exactly the live clusters, and the registry
    /// never holds more than the live set after one.
    #[test]
    fn collect_reports_exactly_the_live_clusters(ops in prop::collection::vec(op(), 1..40)) {
        let h = Harness::new();
        let mut live: Vec<(String, u64, MinerMetrics)> = Vec::new();
        for (step, op) in ops.into_iter().enumerate() {
            match op {
                Op::Build(templates) => {
                    let tenant = format!("t{step}");
                    let metrics = h.cluster_metrics();
                    feed(&metrics, &TenantId::new(tenant.clone()), templates);
                    live.push((tenant, templates, metrics));
                }
                Op::Drop(_) if live.is_empty() => {}
                Op::Drop(i) => {
                    let (_, _, metrics) = live.swap_remove(i % live.len());
                    let weak = weak_state(&metrics);
                    drop(metrics);
                    prop_assert_eq!(weak.strong_count(), 0);
                }
                Op::Collect => {
                    let points = h.collect();
                    let expected: BTreeMap<String, f64> = live
                        .iter()
                        .map(|(tenant, templates, _)| (tenant_key(tenant), f64::from(u32::try_from(*templates).unwrap())))
                        .collect();
                    let got = points
                        .get(semconv::OURIOS_MINER_TEMPLATE_COUNT)
                        .cloned()
                        .unwrap_or_default();
                    prop_assert_eq!(got, expected);
                    prop_assert_eq!(lock_len(&h.registry), live.len());
                }
            }
        }
    }
}
