//! Scan pruning / IO stats accumulation and RFC 0040 operator-span
//! recording over an executed physical plan (epic #745 wave 1; moved
//! verbatim from the crate root).

// Split from the crate root (epic #745 wave 1); the parent scope is
// the import surface so every pre-split `crate::X` path resolves
// unchanged.
#[allow(clippy::wildcard_imports)]
use super::*;

/// Walk the executed physical plan and accumulate the scan
/// pruning / IO metrics into a [`QueryStats`]. Recursive — the
/// Parquet scan is a leaf under the aggregate.
pub(super) fn scan_stats(plan: &dyn ExecutionPlan) -> QueryStats {
    let mut stats = QueryStats::default();
    accumulate_scan_stats(plan, &mut stats);
    stats
}

/// Reconstruct the operator span tree for the just-executed `plan` under the
/// current tracing span (RFC 0040), once `collect()` has returned so every
/// node's `BaselineMetrics` timestamps are final. A no-op when no `OTel` layer
/// is installed or the current span isn't sampled — see
/// [`ourios_df_otel::record_plan_spans`]'s own gate.
///
/// Resolves the process-global tracer (mirrors the RFC 0033 §3.7 meter
/// pattern): this crate has no `TracerProvider` of its own to hand out, and
/// the installed global is what every OTel-instrumented path in the binary
/// already resolves through.
pub(super) fn record_operator_spans(plan: &dyn ExecutionPlan) {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let cx = tracing::Span::current().context();
    let tracer = opentelemetry::global::tracer("ourios-df-otel");
    ourios_df_otel::record_plan_spans(plan, &cx, &tracer);
}

pub(super) fn accumulate_scan_stats(plan: &dyn ExecutionPlan, stats: &mut QueryStats) {
    if let Some(metrics) = plan.metrics() {
        // `aggregate_by_name` sums each metric across the scan's
        // per-file / per-partition instances.
        fold_metrics(&metrics.aggregate_by_name(), stats);
    }
    for child in plan.children() {
        accumulate_scan_stats(child.as_ref(), stats);
    }
}

/// Fold the `DataFusion`-version-sensitive scan metrics — the
/// `row_groups_pruned_statistics` `PruningMetrics` and the
/// `bytes_scanned` `Count` — into `stats`. Pulled out of
/// [`accumulate_scan_stats`] so the metric-name / value-shape
/// matching is unit-testable without a live plan (the names are an
/// engine contract that can drift across `DataFusion` releases).
pub(super) fn fold_metrics(metrics: &MetricsSet, stats: &mut QueryStats) {
    for metric in metrics.iter() {
        match metric.value() {
            // `row_groups_pruned_statistics` is a PruningMetrics
            // carrying both pruned (skipped via min/max stats) and
            // matched (read) row-group counts — exactly the B1
            // numerator + denominator.
            MetricValue::PruningMetrics {
                name,
                pruning_metrics,
            } if name == "row_groups_pruned_statistics" => {
                stats.row_groups_pruned += pruning_metrics.pruned() as u64;
                stats.row_groups_scanned += pruning_metrics.matched() as u64;
            }
            MetricValue::Count { name, count } if name == "bytes_scanned" => {
                stats.bytes_read += count.value() as u64;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::super::*;

    /// Pin the metric-name / value-shape contract `fold_metrics`
    /// depends on: a `row_groups_pruned_statistics` `PruningMetrics`
    /// maps to pruned/matched row-group counts, `bytes_scanned`
    /// `Count` maps to `bytes_read`, and any other metric is ignored.
    /// If a `DataFusion` bump renames or reshapes these, this fails
    /// locally rather than letting the live test silently report
    /// always-zero stats.
    #[test]
    fn fold_metrics_extracts_pruning_and_bytes() {
        use std::borrow::Cow;

        use datafusion::physical_plan::metrics::{Count, Metric, PruningMetrics};

        let pruning = PruningMetrics::new();
        pruning.add_pruned(3);
        pruning.add_matched(2);
        let bytes = Count::new();
        bytes.add(4096);
        // A metric we don't track — must be left untouched.
        let other = Count::new();
        other.add(99);

        let mut set = MetricsSet::new();
        set.push(Arc::new(Metric::new(
            MetricValue::PruningMetrics {
                name: Cow::Borrowed("row_groups_pruned_statistics"),
                pruning_metrics: pruning,
            },
            None,
        )));
        set.push(Arc::new(Metric::new(
            MetricValue::Count {
                name: Cow::Borrowed("bytes_scanned"),
                count: bytes,
            },
            None,
        )));
        set.push(Arc::new(Metric::new(
            MetricValue::Count {
                name: Cow::Borrowed("output_rows"),
                count: other,
            },
            None,
        )));

        let mut stats = QueryStats::default();
        fold_metrics(&set, &mut stats);
        assert_eq!(stats.row_groups_pruned, 3);
        assert_eq!(stats.row_groups_scanned, 2);
        assert_eq!(stats.bytes_read, 4096);
    }

    // --- resolve_live_files (RFC 0009 §3.4 manifest / glob fallback) ---
}
