//! Post-hoc `DataFusion` `ExecutionPlan` → `OTel` operator span tree (RFC
//! 0040). `record_plan_spans` walks a finished physical plan and emits one
//! child span per `BaselineMetrics`-backed node, using the node's real
//! `StartTimestamp`/`EndTimestamp` wall-clock bounds — not derived timing.
//! Deliberately dependency-light (`datafusion` + `opentelemetry` only, no
//! `ourios-*` types) so it lifts cleanly to a standalone
//! `datafusion-contrib` crate (RFC 0040 §3.5).
//!
//! See `docs/rfcs/0040-datafusion-operator-instrumentation.md` §3.

#![deny(unsafe_code)]

use std::time::SystemTime;

use datafusion::physical_plan::ExecutionPlan;
use datafusion::physical_plan::metrics::{MetricValue, MetricsSet};
use opentelemetry::trace::{SpanKind, TraceContextExt as _, Tracer};
use opentelemetry::{Context, KeyValue};

/// The `PruningMetrics` name `accumulate_scan_stats`/`fold_metrics`
/// (`ourios-querier`) already key off — the B1 numerator/denominator.
const ROW_GROUPS_PRUNED_STATISTICS: &str = "row_groups_pruned_statistics";

pub const ATTR_OUTPUT_ROWS: &str = "datafusion.operator.output_rows";
pub const ATTR_ELAPSED_COMPUTE: &str = "datafusion.operator.elapsed_compute";
pub const ATTR_OUTPUT_BYTES: &str = "datafusion.operator.output_bytes";
pub const ATTR_ROW_GROUPS_PRUNED: &str = "datafusion.operator.row_groups_pruned";
pub const ATTR_ROW_GROUPS_MATCHED: &str = "datafusion.operator.row_groups_matched";

/// Walk `plan` and emit one child `OTel` span per timed `ExecutionPlan` node
/// (RFC 0040 §3.2), nested under `parent`. Gated on `parent`'s span being
/// sampled (§3.6): an unsampled or absent parent means the walk is skipped
/// entirely, so this is a no-op on the default (traces off / unsampled)
/// path.
///
/// `T::Span` needs `Send + Sync + 'static` to be threaded through
/// `opentelemetry::Context` as a child span's parent — every real tracer
/// (`BoxedTracer`, the SDK tracer) satisfies it.
pub fn record_plan_spans<T: Tracer>(plan: &dyn ExecutionPlan, parent: &Context, tracer: &T)
where
    T::Span: Send + Sync + 'static,
{
    if !parent.span().span_context().is_sampled() {
        return;
    }
    walk(plan, parent, tracer);
}

fn walk<T: Tracer>(plan: &dyn ExecutionPlan, parent: &Context, tracer: &T)
where
    T::Span: Send + Sync + 'static,
{
    let Some(aggregated) = plan.metrics().map(|m| m.aggregate_by_name()) else {
        for child in plan.children() {
            walk(child.as_ref(), parent, tracer);
        }
        return;
    };
    // §3.1 constraint 2 / RFC0040.4 — a node whose `MetricsSet` carries no
    // `BaselineMetrics` timestamps (e.g. `CooperativeExec`) is skipped, not
    // faked: its children re-parent to `parent` instead of getting a span
    // with an invented timeline.
    let Some((start, end)) = timed_bounds(&aggregated) else {
        for child in plan.children() {
            walk(child.as_ref(), parent, tracer);
        }
        return;
    };
    let span = tracer
        .span_builder(plan.name().to_string())
        .with_kind(SpanKind::Internal)
        .with_start_time(start)
        .with_attributes(node_attributes(&aggregated))
        .start_with_context(tracer, parent);
    let child_cx = parent.with_span(span);
    for child in plan.children() {
        walk(child.as_ref(), &child_cx, tracer);
    }
    child_cx.span().end_with_timestamp(end);
}

/// The node's aggregated `StartTimestamp`/`EndTimestamp`, converted to
/// `SystemTime` for the span builder. `None` if either is missing — a node
/// with only one of the two (shouldn't happen in practice, `BaselineMetrics`
/// always records both) is treated the same as having neither: skipped.
/// Reduces explicitly via earliest-start / latest-end (RFC0040.2) rather than
/// trusting a single `MetricsSet` entry per name, and treats an inverted
/// `end < start` the same as missing bounds — an untimed node, not a span
/// with an invented, backwards timeline.
fn timed_bounds(aggregated: &MetricsSet) -> Option<(SystemTime, SystemTime)> {
    let mut start = None;
    let mut end = None;
    for metric in aggregated.iter() {
        match metric.value() {
            MetricValue::StartTimestamp(ts) => {
                if let Some(v) = ts.value() {
                    start = Some(match start {
                        Some(cur) if cur < v => cur,
                        _ => v,
                    });
                }
            }
            MetricValue::EndTimestamp(ts) => {
                if let Some(v) = ts.value() {
                    end = Some(match end {
                        Some(cur) if cur > v => cur,
                        _ => v,
                    });
                }
            }
            _ => {}
        }
    }
    match (start, end) {
        (Some(start), Some(end)) if start <= end => {
            Some((SystemTime::from(start), SystemTime::from(end)))
        }
        _ => None,
    }
}

/// The node's `usize` metric value converted to `OTel`'s `i64` attribute type.
/// `None` on overflow rather than a wrapped (and silently negative) value —
/// row/byte/duration counts never realistically approach `i64::MAX`, but an
/// attribute that's missing is honest; one that's wrapped is not.
fn attr(key: &'static str, value: usize) -> Option<KeyValue> {
    i64::try_from(value).ok().map(|v| KeyValue::new(key, v))
}

/// The RFC 0040 §3.3 normative attribute table. Omits a metric the node
/// doesn't report rather than zero-filling, so presence stays meaningful.
fn node_attributes(aggregated: &MetricsSet) -> Vec<KeyValue> {
    let mut attrs = Vec::new();
    for metric in aggregated.iter() {
        match metric.value() {
            MetricValue::OutputRows(count) => {
                attrs.extend(attr(ATTR_OUTPUT_ROWS, count.value()));
            }
            MetricValue::ElapsedCompute(time) => {
                attrs.extend(attr(ATTR_ELAPSED_COMPUTE, time.value()));
            }
            MetricValue::OutputBytes(count) => {
                attrs.extend(attr(ATTR_OUTPUT_BYTES, count.value()));
            }
            MetricValue::PruningMetrics {
                name,
                pruning_metrics,
            } if name == ROW_GROUPS_PRUNED_STATISTICS => {
                attrs.extend(attr(ATTR_ROW_GROUPS_PRUNED, pruning_metrics.pruned()));
                attrs.extend(attr(ATTR_ROW_GROUPS_MATCHED, pruning_metrics.matched()));
            }
            _ => {}
        }
    }
    attrs
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::physical_plan::metrics::{
        Count, ExecutionPlanMetricsSet, MetricBuilder, PruningMetrics, Time, Timestamp,
    };
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use std::sync::Arc;

    /// A hand-built `MetricsSet` shaped like a scan leaf's, exercising the
    /// `MetricValue -> attribute` mapping and the timestamp reduction
    /// without a live plan (RFC 0040 §6 "unit tests over hand-built
    /// `MetricsSet`s").
    fn scan_like_metrics() -> MetricsSet {
        let set = ExecutionPlanMetricsSet::new();

        let start = Timestamp::new();
        start.set(chrono::DateTime::from_timestamp(1_000, 0).expect("valid"));
        MetricBuilder::new(&set).build(MetricValue::StartTimestamp(start));

        let end = Timestamp::new();
        end.set(chrono::DateTime::from_timestamp(1_000, 500_000_000).expect("valid"));
        MetricBuilder::new(&set).build(MetricValue::EndTimestamp(end));

        let rows = Count::new();
        rows.add(42);
        MetricBuilder::new(&set).build(MetricValue::OutputRows(rows));

        let elapsed = Time::new();
        elapsed.add_duration(std::time::Duration::from_micros(250));
        MetricBuilder::new(&set).build(MetricValue::ElapsedCompute(elapsed));

        let bytes = Count::new();
        bytes.add(4096);
        MetricBuilder::new(&set).build(MetricValue::OutputBytes(bytes));

        let pruning = PruningMetrics::new();
        pruning.add_pruned(3);
        pruning.add_matched(1);
        MetricBuilder::new(&set).build(MetricValue::PruningMetrics {
            name: ROW_GROUPS_PRUNED_STATISTICS.into(),
            pruning_metrics: pruning,
        });

        set.clone_inner()
    }

    #[test]
    fn timed_bounds_reduces_start_end() {
        let aggregated = scan_like_metrics().aggregate_by_name();
        let (start, end) = timed_bounds(&aggregated).expect("both timestamps present");
        assert!(start < end);
    }

    #[test]
    fn timed_bounds_none_when_end_precedes_start() {
        let set = ExecutionPlanMetricsSet::new();

        let start = Timestamp::new();
        start.set(chrono::DateTime::from_timestamp(1_000, 0).expect("valid"));
        MetricBuilder::new(&set).build(MetricValue::StartTimestamp(start));

        let end = Timestamp::new();
        end.set(chrono::DateTime::from_timestamp(999, 0).expect("valid"));
        MetricBuilder::new(&set).build(MetricValue::EndTimestamp(end));

        let aggregated = set.clone_inner().aggregate_by_name();
        assert!(
            timed_bounds(&aggregated).is_none(),
            "an inverted end < start must be treated as untimed, not emitted"
        );
    }

    #[test]
    fn attr_omits_on_i64_overflow_rather_than_wrapping() {
        assert!(
            attr("test.key", usize::MAX).is_none(),
            "a value beyond i64::MAX must be omitted, not silently wrapped negative"
        );
        assert_eq!(
            attr("test.key", 42).map(|kv| kv.value),
            Some(42_i64.into()),
            "an in-range value still converts normally"
        );
    }

    #[test]
    fn node_attributes_maps_every_normative_metric() {
        let aggregated = scan_like_metrics().aggregate_by_name();
        let attrs = node_attributes(&aggregated);
        let get = |key: &str| {
            attrs
                .iter()
                .find(|kv| kv.key.as_str() == key)
                .map(|kv| kv.value.clone())
        };
        assert_eq!(get(ATTR_OUTPUT_ROWS), Some(42_i64.into()));
        assert_eq!(get(ATTR_OUTPUT_BYTES), Some(4096_i64.into()));
        assert_eq!(get(ATTR_ROW_GROUPS_PRUNED), Some(3_i64.into()));
        assert_eq!(get(ATTR_ROW_GROUPS_MATCHED), Some(1_i64.into()));
        assert!(get(ATTR_ELAPSED_COMPUTE).is_some());
    }

    /// A minimal `ExecutionPlan` leaf that reports a fixed, hand-built
    /// `MetricsSet` with real Start/EndTimestamp — a stand-in for a real
    /// timed operator, without needing to stand up a live scan.
    #[derive(Debug)]
    struct FakeTimedLeaf {
        properties: Arc<datafusion::physical_plan::PlanProperties>,
    }

    impl FakeTimedLeaf {
        fn new() -> Self {
            use datafusion::arrow::datatypes::Schema;
            use datafusion::physical_expr::EquivalenceProperties;
            use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
            use datafusion::physical_plan::{Partitioning, PlanProperties};

            let schema = Arc::new(Schema::empty());
            let properties = Arc::new(PlanProperties::new(
                EquivalenceProperties::new(schema),
                Partitioning::UnknownPartitioning(1),
                EmissionType::Incremental,
                Boundedness::Bounded,
            ));
            Self { properties }
        }
    }

    impl datafusion::physical_plan::DisplayAs for FakeTimedLeaf {
        fn fmt_as(
            &self,
            _t: datafusion::physical_plan::DisplayFormatType,
            f: &mut std::fmt::Formatter,
        ) -> std::fmt::Result {
            write!(f, "FakeTimedLeaf")
        }
    }

    impl ExecutionPlan for FakeTimedLeaf {
        fn name(&self) -> &'static str {
            "FakeTimedLeaf"
        }

        fn properties(&self) -> &Arc<datafusion::physical_plan::PlanProperties> {
            &self.properties
        }

        fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
            vec![]
        }

        fn with_new_children(
            self: Arc<Self>,
            _children: Vec<Arc<dyn ExecutionPlan>>,
        ) -> datafusion::error::Result<Arc<dyn ExecutionPlan>> {
            Ok(self)
        }

        fn execute(
            &self,
            _partition: usize,
            _task_ctx: Arc<datafusion::execution::TaskContext>,
        ) -> datafusion::error::Result<datafusion::physical_plan::SendableRecordBatchStream>
        {
            unimplemented!("never executed — this double is metrics()/name()/children() only")
        }

        fn metrics(&self) -> Option<MetricsSet> {
            Some(scan_like_metrics())
        }
    }

    /// RFC0040.4 over a real `ExecutionPlan` tree: `CooperativeExec`
    /// (`datafusion_physical_plan::coop`) is `DataFusion` 54's own
    /// cooperative-scheduling wrapper — a real, optimizer-inserted node with
    /// no `BaselineMetrics` (`metrics()` returns `None`, the crate default).
    /// Wrapping it around [`FakeTimedLeaf`] gives a genuine
    /// untimed-parent/timed-child pair: `record_plan_spans` must skip
    /// `CooperativeExec` and re-parent its span to the root context, not
    /// invent a timeline for it.
    #[test]
    fn untimed_wrapper_is_skipped_and_child_reparents_to_root() {
        use datafusion::physical_plan::coop::CooperativeExec;

        let exporter = InMemorySpanExporter::default();
        // Explicit sampler, not the SDK default: `record_plan_spans` gates on
        // the parent span being sampled (§3.6), so a default change here
        // would make this test flaky rather than exercising the walk logic.
        let provider = SdkTracerProvider::builder()
            .with_sampler(opentelemetry_sdk::trace::Sampler::AlwaysOn)
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("test");

        let leaf: Arc<dyn ExecutionPlan> = Arc::new(FakeTimedLeaf::new());
        assert!(
            leaf.metrics().is_some(),
            "the leaf itself must be timed for this to test anything"
        );
        let wrapped: Arc<dyn ExecutionPlan> = Arc::new(CooperativeExec::new(leaf));
        assert!(
            wrapped.metrics().is_none(),
            "CooperativeExec must be the untimed node under test"
        );

        let root_span = tracer.span_builder("root").start(&tracer);
        let root_cx = Context::current_with_span(root_span);

        record_plan_spans(wrapped.as_ref(), &root_cx, &tracer);
        root_cx.span().end();

        provider.force_flush().expect("flush");
        let spans = exporter.get_finished_spans().expect("spans");

        let names: Vec<_> = spans.iter().map(|s| s.name.to_string()).collect();
        assert!(
            !names.contains(&"CooperativeExec".to_string()),
            "the untimed wrapper must not get a span: {names:?}"
        );
        let leaf_span = spans
            .iter()
            .find(|s| s.name == "FakeTimedLeaf")
            .expect("the timed child still gets a span");
        let root = spans.iter().find(|s| s.name == "root").expect("root span");
        assert_eq!(
            leaf_span.parent_span_id,
            root.span_context.span_id(),
            "skipping CooperativeExec re-parents its child to the root, not a phantom parent",
        );
    }

    /// An `ExecutionPlan` whose `metrics()`/`children()` panic if called —
    /// RFC0040.6's guarantee ("the plan walk does not run" on the unsampled
    /// path) proven directly: if `record_plan_spans` ever regressed to
    /// touching the plan before checking `is_sampled()`, this test would
    /// panic, not merely run slow.
    #[derive(Debug)]
    struct PanicsIfTouched {
        properties: Arc<datafusion::physical_plan::PlanProperties>,
    }

    impl PanicsIfTouched {
        fn new() -> Self {
            use datafusion::arrow::datatypes::Schema;
            use datafusion::physical_expr::EquivalenceProperties;
            use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
            use datafusion::physical_plan::{Partitioning, PlanProperties};

            let schema = Arc::new(Schema::empty());
            let properties = Arc::new(PlanProperties::new(
                EquivalenceProperties::new(schema),
                Partitioning::UnknownPartitioning(1),
                EmissionType::Incremental,
                Boundedness::Bounded,
            ));
            Self { properties }
        }
    }

    impl datafusion::physical_plan::DisplayAs for PanicsIfTouched {
        fn fmt_as(
            &self,
            _t: datafusion::physical_plan::DisplayFormatType,
            f: &mut std::fmt::Formatter,
        ) -> std::fmt::Result {
            write!(f, "PanicsIfTouched")
        }
    }

    impl ExecutionPlan for PanicsIfTouched {
        fn name(&self) -> &'static str {
            "PanicsIfTouched"
        }

        fn properties(&self) -> &Arc<datafusion::physical_plan::PlanProperties> {
            &self.properties
        }

        fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
            panic!("record_plan_spans must not walk the plan on the unsampled path")
        }

        fn with_new_children(
            self: Arc<Self>,
            _children: Vec<Arc<dyn ExecutionPlan>>,
        ) -> datafusion::error::Result<Arc<dyn ExecutionPlan>> {
            Ok(self)
        }

        fn execute(
            &self,
            _partition: usize,
            _task_ctx: Arc<datafusion::execution::TaskContext>,
        ) -> datafusion::error::Result<datafusion::physical_plan::SendableRecordBatchStream>
        {
            unimplemented!("never executed — this double is metrics()/name()/children() only")
        }

        fn metrics(&self) -> Option<MetricsSet> {
            panic!("record_plan_spans must not walk the plan on the unsampled path")
        }
    }

    /// RFC0040.6 (unit half) — an unsampled parent context means no span is
    /// emitted and the plan is never touched at all, proven by a plan double
    /// that panics if `metrics()`/`children()` are called.
    #[test]
    fn record_plan_spans_never_touches_the_plan_when_unsampled() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("test");

        // An empty `Context` carries no span, so `.span()` resolves to a
        // placeholder whose `SpanContext` is invalid — `is_sampled()` is
        // `false`, exactly the "traces off" / unsampled default (§3.6).
        let unsampled_cx = Context::new();
        let plan = PanicsIfTouched::new();

        record_plan_spans(&plan, &unsampled_cx, &tracer);

        provider.force_flush().expect("flush");
        let spans = exporter.get_finished_spans().expect("spans");
        assert!(
            spans.is_empty(),
            "no span should be emitted on the unsampled path, got {spans:?}"
        );
    }
}
