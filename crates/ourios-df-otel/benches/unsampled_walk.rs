//! RFC0040.6 — `record_plan_spans` costs nothing on the unsampled path.
//!
//! `PanicsIfTouched` is an `ExecutionPlan` whose `metrics()`/`children()`
//! panic if called: this bench doesn't just measure that the unsampled path
//! is *fast*, it proves it never touches the plan at all — a regression that
//! started walking would panic the bench run, not merely show up as a
//! slower number to eyeball.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use datafusion::physical_plan::ExecutionPlan;
use datafusion::physical_plan::metrics::MetricsSet;
use opentelemetry::Context;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::SdkTracerProvider;

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
    ) -> datafusion::error::Result<datafusion::physical_plan::SendableRecordBatchStream> {
        unimplemented!("never executed — this double is metrics()/name()/children() only")
    }

    fn metrics(&self) -> Option<MetricsSet> {
        panic!("record_plan_spans must not walk the plan on the unsampled path")
    }
}

fn unsampled_walk(c: &mut Criterion) {
    let provider = SdkTracerProvider::builder().build();
    let tracer = provider.tracer("bench");
    let unsampled_cx = Context::new();
    let plan = PanicsIfTouched::new();

    c.bench_function("record_plan_spans/unsampled", |b| {
        b.iter(|| {
            // Every argument goes through `black_box`, the context especially:
            // without it the optimizer can constant-fold `is_sampled()` to
            // `false`, prove the body unreachable, and elide the call — leaving
            // the bench measuring an empty loop rather than the guard.
            ourios_df_otel::record_plan_spans(
                black_box(&plan),
                black_box(&unsampled_cx),
                black_box(&tracer),
            );
        });
    });
}

criterion_group!(benches, unsampled_walk);
criterion_main!(benches);
