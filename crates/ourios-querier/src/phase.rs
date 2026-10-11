//! Query-phase context for blocking work (RFC 0038 §3.7.4).
//!
//! A phase span is re-entered on the blocking thread, but a `tracing` span
//! does not set the `OTel` context the store bridge carries to its runtime.
//! These wrappers attach the current span's `OTel` context, with the
//! [`QueryPhase`] marker, for the duration of the work, so the object-store
//! requests it makes open `CLIENT` spans under that span.

use opentelemetry::Context;
use ourios_parquet::QueryPhase;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

/// `work`, run as a query phase under the current span.
pub(crate) fn attached<T>(work: impl FnOnce() -> T) -> impl FnOnce() -> T {
    move || {
        let _phase = tracing::Span::current()
            .context()
            .with_value(QueryPhase)
            .attach();
        work()
    }
}

/// `work`, run under the current span, and still a query phase when the
/// work around it is one.
pub(crate) fn nested<T>(work: impl FnOnce() -> T) -> impl FnOnce() -> T {
    move || {
        let cx = tracing::Span::current().context();
        let cx = match Context::current().get::<QueryPhase>() {
            Some(_) => cx.with_value(QueryPhase),
            None => cx,
        };
        let _attached = cx.attach();
        work()
    }
}
