//! RFC 0038 §3.7.1–§3.7.2 — file-set resolution is a `resolve files` span
//! under the query's span, carrying the tenant and the live-file count, and
//! emitted even when the tenant has nothing live.
//!
//! The listing runs on the blocking pool, where `spawn_blocking_phase`
//! re-enters the phase span — which is what lets a scoped subscriber see it.

use std::sync::Arc;

use opentelemetry::Value;
use opentelemetry::trace::{SpanId, SpanKind, TraceContextExt as _, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use ourios_core::tenant::TenantId;
use ourios_querier::Querier;
use ourios_semconv as semconv;
use tracing::Instrument as _;
use tracing::instrument::WithSubscriber as _;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use tracing_subscriber::prelude::*;

use crate::common::{DEFAULT_WINDOW_NS, NOW, TS0, simple, write_all};

const TENANT: &str = "acme";

/// A scoped tracing pipeline over its own in-memory span exporter. Keep the
/// provider alive until the spans are read.
fn traced() -> (InMemorySpanExporter, SdkTracerProvider, tracing::Dispatch) {
    crate::common::global_subscriber();
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let dispatch =
        tracing::Dispatch::new(tracing_subscriber::registry().with(
            tracing_opentelemetry::layer().with_tracer(
                provider.tracer_with_scope(ourios_core::telemetry::scope("ourios-test")),
            ),
        ));
    (exporter, provider, dispatch)
}

fn named<'s>(spans: &'s [SpanData], name: &str) -> Vec<&'s SpanData> {
    spans.iter().filter(|s| s.name.as_ref() == name).collect()
}

fn attribute(span: &SpanData, key: &str) -> Option<Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.clone())
}

fn text(span: &SpanData, key: &str) -> Option<String> {
    attribute(span, key).map(|v| v.as_str().into_owned())
}

/// One query under a stand-in `POST /v1/query` span (the real one is
/// `ourios-server`'s); the stand-in's span id.
async fn query(querier: Arc<Querier>) -> SpanId {
    let span = tracing::info_span!("POST /v1/query");
    let id = span.context().span().span_context().span_id();
    let parsed = ourios_querier::dsl::parse("template_id == 1 | limit 10").expect("parse");
    querier
        .run_query(
            &parsed,
            &TenantId::new(TENANT),
            NOW,
            DEFAULT_WINDOW_NS,
            None,
        )
        .instrument(span)
        .await
        .expect("query");
    id
}

/// A query over a live file set opens exactly one `resolve files` span,
/// `INTERNAL`, under the query, with the tenant and the resolved count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolution_spans_the_file_set_under_the_query() {
    let (exporter, _provider, dispatch) = traced();
    let dir = tempfile::tempdir().expect("tempdir");
    write_all(dir.path(), &[simple(TENANT, 1, TS0)]);
    let querier = Arc::new(Querier::new(dir.path()));
    let query_id = query(querier).with_subscriber(dispatch).await;
    let spans = exporter.get_finished_spans().expect("spans");

    let [resolve] = named(&spans, "resolve files")[..] else {
        panic!(
            "one resolve span: {:?}",
            spans.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    };
    assert_eq!(resolve.span_kind, SpanKind::Internal);
    assert_eq!(
        resolve.parent_span_id, query_id,
        "the phase nests under the query"
    );
    assert_eq!(
        text(resolve, semconv::OURIOS_TENANT).as_deref(),
        Some(TENANT)
    );
    assert_eq!(
        attribute(resolve, semconv::OURIOS_FILE_SET_LIVE_FILES),
        Some(Value::I64(1))
    );
}

/// A tenant with nothing live still gets the span — the listing ran — with
/// the count at zero (RFC 0038 §3.7.1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_file_set_still_spans_the_resolution() {
    let (exporter, _provider, dispatch) = traced();
    let dir = tempfile::tempdir().expect("tempdir");
    let querier = Arc::new(Querier::new(dir.path()));
    let query_id = query(querier).with_subscriber(dispatch).await;
    let spans = exporter.get_finished_spans().expect("spans");

    let [resolve] = named(&spans, "resolve files")[..] else {
        panic!("one resolve span over an empty tenant");
    };
    assert_eq!(resolve.parent_span_id, query_id);
    assert_eq!(
        attribute(resolve, semconv::OURIOS_FILE_SET_LIVE_FILES),
        Some(Value::I64(0))
    );
}

/// Plant a manifest that does not parse in every partition holding data
/// under `root`, so file-set resolution fails on its first manifest read.
fn corrupt_manifests(root: &std::path::Path) -> usize {
    let mut corrupted = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut holds_data = false;
        for entry in std::fs::read_dir(&dir).expect("read dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                holds_data |= path.extension().is_some_and(|e| e == "parquet");
            }
        }
        if holds_data {
            std::fs::write(
                dir.join(ourios_parquet::MANIFEST_FILENAME),
                b"not a manifest",
            )
            .expect("corrupt manifest");
            corrupted += 1;
        }
    }
    corrupted
}

/// A resolution that fails sets the phase's status to `Error` with
/// `error.type` and records no live-file count (RFC 0038 §3.7.2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_resolution_marks_the_phase_as_an_error() {
    let (exporter, _provider, dispatch) = traced();
    let dir = tempfile::tempdir().expect("tempdir");
    write_all(dir.path(), &[simple(TENANT, 1, TS0)]);
    assert!(
        corrupt_manifests(dir.path()) > 0,
        "the fixture wrote a partition"
    );
    let querier = Querier::new(dir.path());
    let result = async {
        let parsed = ourios_querier::dsl::parse("template_id == 1 | limit 10").expect("parse");
        querier
            .run_query(
                &parsed,
                &TenantId::new(TENANT),
                NOW,
                DEFAULT_WINDOW_NS,
                None,
            )
            .instrument(tracing::info_span!("POST /v1/query"))
            .await
    }
    .with_subscriber(dispatch)
    .await;
    assert!(result.is_err(), "a corrupt manifest fails the query");
    let spans = exporter.get_finished_spans().expect("spans");

    let [resolve] = named(&spans, "resolve files")[..] else {
        panic!(
            "one resolve span for the failed resolution: {:?}",
            spans.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    };
    assert!(
        matches!(resolve.status, opentelemetry::trace::Status::Error { .. }),
        "{:?}",
        resolve.status
    );
    assert_eq!(text(resolve, "error.type").as_deref(), Some("storage"));
    assert_eq!(
        attribute(resolve, semconv::OURIOS_FILE_SET_LIVE_FILES),
        None
    );
}
