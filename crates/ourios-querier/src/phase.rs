//! Query-phase context for blocking work (RFC 0038 §3.7.4).
//!
//! A phase span is re-entered on the blocking thread, but a `tracing` span
//! does not set the `OTel` context the store bridge carries to its runtime.
//! These wrappers attach the phase span's `OTel` context, with the
//! [`QueryPhase`] marker, for the duration of the work, so the object-store
//! requests it makes open `CLIENT` spans under that span.
//!
//! The span is passed explicitly and the marker attached only when it is
//! enabled: a phase span the subscriber filtered out must not hand the
//! marker to an ancestor (the query root), which would parent `CLIENT`
//! spans with no phase span between.

use opentelemetry::Context;
use ourios_parquet::QueryPhase;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

/// `work`, run inside `span` as a query phase.
pub(crate) fn in_phase<T>(span: &Span, work: impl FnOnce() -> T) -> T {
    // Attached after entering: entering activates the span's own context,
    // which would otherwise replace the marked one.
    span.in_scope(|| {
        let _phase = (!span.is_disabled()).then(|| span.context().with_value(QueryPhase).attach());
        work()
    })
}

/// `work`, run inside `span`, and still a query phase when the work around
/// it is one.
pub(crate) fn nested_in<T>(span: &Span, work: impl FnOnce() -> T) -> T {
    let in_phase = Context::current().get::<QueryPhase>().is_some();
    span.in_scope(|| {
        let _attached = (!span.is_disabled()).then(|| {
            let cx = span.context();
            if in_phase {
                cx.with_value(QueryPhase)
            } else {
                cx
            }
            .attach()
        });
        work()
    })
}

#[cfg(test)]
mod tests {
    use opentelemetry::Context;
    use opentelemetry::trace::TraceContextExt as _;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use ourios_parquet::QueryPhase;
    use tracing_subscriber::filter::filter_fn;
    use tracing_subscriber::prelude::*;

    use super::{in_phase, nested_in};
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    /// Whether the work runs in a query phase under a valid parent span.
    fn traced_phase() -> bool {
        let cx = Context::current();
        cx.get::<QueryPhase>().is_some() && cx.span().span_context().is_valid()
    }

    /// A phase span the subscriber filters out attaches no marker, so the
    /// enclosing query span does not become a `CLIENT` parent; an enabled
    /// phase span does.
    #[test]
    fn only_an_enabled_phase_span_attaches_the_marker() {
        let provider = SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_opentelemetry::layer()
                    .with_tracer(ourios_telemetry::test_tracer(&provider)),
            )
            .with(filter_fn(|meta| meta.name() != "filtered phase"));
        tracing::subscriber::with_default(subscriber, || {
            let query = tracing::info_span!("POST /v1/query");
            let _query = query.enter();

            let filtered = tracing::info_span!("filtered phase");
            assert!(filtered.is_disabled());
            assert!(!in_phase(&filtered, traced_phase), "no phase on the query");

            let enabled = tracing::info_span!("load template_map");
            assert!(in_phase(&enabled, traced_phase), "the enabled phase");

            // Entering a span activates its own context; the marker must
            // survive that, and carry into a nested phase span.
            in_phase(&enabled, || {
                let derive = tracing::info_span!("derive template_map");
                let id = derive.context().span().span_context().span_id();
                nested_in(&derive, || {
                    assert!(traced_phase(), "the nested phase");
                    assert_eq!(Context::current().span().span_context().span_id(), id);
                });
            });
        });
    }
}
