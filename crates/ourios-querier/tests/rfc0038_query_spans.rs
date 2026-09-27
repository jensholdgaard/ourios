//! RFC 0038 — the querier's own internal spans for the phases no operator span
//! covers (#853): file-set resolution (`resolve files`) and template-map
//! acquisition (`load template_map`), each a child of the caller's query span
//! even though both run on the blocking pool.
//!
//! Both are opened inside the blocking callee (RFC 0038 §3.3), where the
//! thread's dispatcher is the process-global one — as in production, where
//! `ourios_telemetry` installs the subscriber globally. So this test installs
//! a global subscriber too, and lives in its own integration binary (its own
//! process), the RFC0028.2 process-isolation exemption. Both scenarios run
//! sequentially inside one `#[tokio::test]` so nothing in this binary races
//! the global subscriber.

#[path = "it/common/mod.rs"]
mod common;

use opentelemetry::trace::{SpanKind, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use ourios_core::tenant::TenantId;
use ourios_querier::{Querier, QueryRequest};
use tracing::Instrument as _;
use tracing_subscriber::prelude::*;

use common::{HOUR_NS, TS0, simple, write_all};

fn request(template_id: u64) -> QueryRequest {
    QueryRequest {
        tenant: TenantId::new("acme"),
        time_range: Some((TS0 - HOUR_NS, TS0 + HOUR_NS)),
        template_id: Some(template_id),
        severity_text: None,
        limit: Some(10),
    }
}

async fn query_spans(
    provider: &SdkTracerProvider,
    exporter: &InMemorySpanExporter,
    bucket: &std::path::Path,
    request: QueryRequest,
) -> Vec<SpanData> {
    exporter.reset();
    Querier::new(bucket)
        .run(request)
        .instrument(tracing::info_span!("POST /v1/query"))
        .await
        .expect("query succeeds");
    provider.force_flush().expect("flush");
    exporter.get_finished_spans().expect("exported spans")
}

fn named<'a>(spans: &'a [SpanData], name: &str) -> Vec<&'a SpanData> {
    spans.iter().filter(|s| s.name == name).collect()
}

/// A row-returning query emits one `resolve files` and one `load template_map`
/// span, each an internal direct child of the query span ending within it.
fn assert_nested_under_the_query_span(spans: &[SpanData]) {
    let root = named(spans, "POST /v1/query");
    assert_eq!(root.len(), 1, "{spans:?}");
    let root = root[0];
    for name in ["resolve files", "load template_map"] {
        let found = named(spans, name);
        assert_eq!(found.len(), 1, "one {name:?} span: {spans:?}");
        let span = found[0];
        assert_eq!(span.parent_span_id, root.span_context.span_id(), "{name}");
        assert_eq!(
            span.span_context.trace_id(),
            root.span_context.trace_id(),
            "{name}"
        );
        assert_eq!(span.span_kind, SpanKind::Internal, "{name}");
        assert!(span.start_time <= span.end_time, "{name}");
        assert!(span.end_time <= root.end_time, "{name}");
    }
}

#[tokio::test]
async fn resolution_and_template_map_spans() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("ourios-test"))),
    )
    .expect("the only global subscriber in this binary");
    let dir = tempfile::tempdir().expect("tempdir");
    write_all(
        dir.path(),
        &[simple("acme", 7, TS0), simple("acme", 7, TS0 + 1)],
    );

    let with_rows = query_spans(&provider, &exporter, dir.path(), request(7)).await;
    assert_nested_under_the_query_span(&with_rows);

    // A query that renders no row resolves its files but never loads the
    // template map.
    let without_rows = query_spans(&provider, &exporter, dir.path(), request(999)).await;
    assert_eq!(
        named(&without_rows, "resolve files").len(),
        1,
        "{without_rows:?}"
    );
    assert!(
        named(&without_rows, "load template_map").is_empty(),
        "{without_rows:?}"
    );
}
