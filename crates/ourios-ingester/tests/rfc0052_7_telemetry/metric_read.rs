//! Reading datapoints back out of the exported stream.

use std::collections::BTreeMap;

use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};

/// A datapoint, typed as the SDK aggregated it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Value {
    I(i64),
    U(u64),
    F(f64),
}

/// Every datapoint of `name`, keyed by its attributes.
pub(crate) fn points(
    rms: &[ResourceMetrics],
    name: &str,
) -> Vec<(BTreeMap<String, String>, Value)> {
    fn attrs<'a>(
        kvs: impl Iterator<Item = &'a opentelemetry::KeyValue>,
    ) -> BTreeMap<String, String> {
        kvs.map(|kv| (kv.key.as_str().to_owned(), kv.value.as_str().into_owned()))
            .collect()
    }
    let mut out = Vec::new();
    for metric in rms
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .filter(|m| m.name() == name)
    {
        match metric.data() {
            AggregatedMetrics::I64(MetricData::Sum(sum)) => {
                out.extend(
                    sum.data_points()
                        .map(|p| (attrs(p.attributes()), Value::I(p.value()))),
                );
            }
            AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                out.extend(
                    sum.data_points()
                        .map(|p| (attrs(p.attributes()), Value::U(p.value()))),
                );
            }
            AggregatedMetrics::U64(MetricData::Gauge(gauge)) => {
                out.extend(
                    gauge
                        .data_points()
                        .map(|p| (attrs(p.attributes()), Value::U(p.value()))),
                );
            }
            AggregatedMetrics::F64(MetricData::Gauge(gauge)) => {
                out.extend(
                    gauge
                        .data_points()
                        .map(|p| (attrs(p.attributes()), Value::F(p.value()))),
                );
            }
            other => panic!("{name}: unexpected aggregation {other:?}"),
        }
    }
    out
}

/// The single attribute-free value of `name`.
pub(crate) fn value(rms: &[ResourceMetrics], name: &str) -> Value {
    match points(rms, name).as_slice() {
        [(attributes, value)] if attributes.is_empty() => *value,
        other => panic!("{name}: expected one attribute-free point, got {other:?}"),
    }
}

/// A state metric's series, by its state attribute.
pub(crate) fn states(rms: &[ResourceMetrics], name: &str, key: &str) -> BTreeMap<String, i64> {
    points(rms, name)
        .into_iter()
        .map(|(attributes, value)| {
            let Value::I(v) = value else {
                panic!("{name} is an i64 UpDownCounter");
            };
            (
                attributes.get(key).cloned().expect("the state attribute"),
                v,
            )
        })
        .collect()
}

/// A counter's total over the points whose `key` is `want`.
pub(crate) fn counted(rms: &[ResourceMetrics], name: &str, key: &str, want: Option<&str>) -> u64 {
    points(rms, name)
        .into_iter()
        .filter(|(attributes, _)| attributes.get(key).map(String::as_str) == want)
        .map(|(_, value)| match value {
            Value::U(v) => v,
            other => panic!("{name} is a u64 counter, got {other:?}"),
        })
        .sum()
}
