//! The registry-metadata guard: an exported `ourios.*` metric carries the
//! description and unit the semconv registry declares for it.
//!
//! The unit is identifying — a backend splits a metric's series when it
//! changes — so a call site that drops or hand-writes it must fail a test,
//! not surface as a broken dashboard. Each crate's metric test hands its
//! export and the generated `ourios_semconv` `(NAME, NAME_DESCRIPTION,
//! NAME_UNIT)` triples it drives to [`assert_registry_metadata`].

use opentelemetry_sdk::metrics::data::{Metric, ResourceMetrics, ScopeMetrics};

/// A registry metric's generated name, description and unit constants.
pub type Registered = (&'static str, &'static str, &'static str);

/// Assert that every exported `ourios.*` metric in `rms` carries a
/// description and a unit, and that each of `expected` is exported with
/// exactly the registry's description and unit.
///
/// # Panics
///
/// When an exported `ourios.*` metric has an empty description or unit, an
/// `expected` name is not exported, or its description or unit differs
/// from the registry's.
pub fn assert_registry_metadata(rms: &[ResourceMetrics], expected: &[Registered]) {
    let metrics: Vec<&Metric> = rms
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .collect();

    let undescribed: Vec<&str> = metrics
        .iter()
        .filter(|m| {
            m.name().starts_with("ourios.") && (m.description().is_empty() || m.unit().is_empty())
        })
        .map(|m| m.name())
        .collect();
    assert!(
        undescribed.is_empty(),
        "ourios.* metrics missing a registry description or unit: {undescribed:?}",
    );

    for &(name, description, unit) in expected {
        let exported: Vec<&&Metric> = metrics.iter().filter(|m| m.name() == name).collect();
        assert!(!exported.is_empty(), "metric `{name}` was not exported");
        for metric in exported {
            assert_eq!(metric.description(), description, "`{name}` description");
            assert_eq!(metric.unit(), unit, "`{name}` unit");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};

    const COUNTER: Registered = ("ourios.test.events", "Test events.", "{event}");

    fn export(description: &'static str, unit: &'static str) -> Vec<ResourceMetrics> {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();
        let meter = provider.meter_with_scope(crate::scope("guard-test"));
        meter
            .u64_counter(COUNTER.0)
            .with_description(description)
            .with_unit(unit)
            .build()
            .add(1, &[]);
        provider.force_flush().expect("force_flush succeeds");
        exporter.get_finished_metrics().expect("metrics exported")
    }

    #[test]
    fn registry_metadata_passes() {
        assert_registry_metadata(&export(COUNTER.1, COUNTER.2), &[COUNTER]);
    }

    #[test]
    #[should_panic(expected = "missing a registry description or unit")]
    fn a_metric_without_a_description_fails() {
        assert_registry_metadata(&export("", COUNTER.2), &[]);
    }

    #[test]
    #[should_panic(expected = "`ourios.test.events` unit")]
    fn a_hand_written_unit_fails() {
        assert_registry_metadata(&export(COUNTER.1, "1"), &[COUNTER]);
    }

    #[test]
    #[should_panic(expected = "was not exported")]
    fn a_missing_expected_metric_fails() {
        assert_registry_metadata(
            &export(COUNTER.1, COUNTER.2),
            &[("ourios.test.absent", "", "")],
        );
    }
}
