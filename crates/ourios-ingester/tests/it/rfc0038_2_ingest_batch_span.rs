//! RFC0038.2 — request-scope spans are O(1) in record count.
//!
//! The ingest hot path must never mint a span per record: that would both
//! strangle throughput and flood the trace backend, defeating the whole
//! sampling discipline of RFC 0038 §3. One OTLP Export batch yields exactly
//! one `ingest logs` span with a single `commit wal` child,
//! whether the batch carries one record or many. See `docs/rfcs/0038-self-tracing.md`.
//!
//! The subscriber is installed *scoped* to the driven future
//! (`WithSubscriber`) over an in-memory span exporter — no process-global
//! tracer provider, so this stays inside the consolidated harness (see
//! `tests/README.md` on the global-installer exclusions).

use crate::ingest_support::{open_pipeline, request, resource_logs};
use opentelemetry::trace::{SpanId, SpanKind, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::prelude::*;

/// Structurally distinct bodies so the miner mints a fresh `template_id`
/// per record — the per-record loop that RFC0038.2 forbids from spawning
/// spans is fully exercised.
fn distinct_bodies(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| format!("event kind {i} completed status=ok"))
        .collect()
}

/// Drive one `ingest_bound` of `record_count` records under a scoped
/// tracer and return every finished span.
async fn ingest_and_collect_spans(record_count: usize) -> Vec<SpanData> {
    let tmp = tempfile::TempDir::new().expect("temp");
    let pipeline = open_pipeline(tmp.path());

    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("ourios-test")));

    let bodies = distinct_bodies(record_count);
    let body_refs: Vec<&str> = bodies.iter().map(String::as_str).collect();
    let export = request(vec![resource_logs("checkout", &body_refs)]);

    let acked = pipeline
        .ingest_bound(export, None, false)
        .with_subscriber(subscriber)
        .await
        .expect("ingest acks");
    assert_eq!(acked, record_count, "every record acked");

    provider.force_flush().expect("spans flush");
    exporter.get_finished_spans().expect("spans exported")
}

fn spans_named<'a>(spans: &'a [SpanData], name: &str) -> Vec<&'a SpanData> {
    spans.iter().filter(|s| s.name.as_ref() == name).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rfc0038_2_ingest_batch_emits_one_span_with_a_wal_commit_child() {
    let spans = ingest_and_collect_spans(1).await;

    let batch = spans_named(&spans, "ingest logs");
    let commit = spans_named(&spans, "commit wal");
    assert_eq!(
        batch.len(),
        1,
        "exactly one ingest-logs span, got {spans:?}"
    );
    assert_eq!(
        commit.len(),
        1,
        "exactly one commit-wal span, got {spans:?}"
    );

    assert_ne!(
        batch[0].span_context.span_id(),
        SpanId::INVALID,
        "the batch span has a real span id",
    );
    assert_eq!(
        commit[0].parent_span_id,
        batch[0].span_context.span_id(),
        "commit-wal nests under ingest-logs",
    );

    // The §3.5 kind contract: the batch is a SERVER span (it answers an OTLP
    // Export RPC), its WAL commit an INTERNAL child. `otel.kind` on the
    // `#[instrument]`/`info_span!` fields must survive export as the span kind.
    assert_eq!(
        batch[0].span_kind,
        SpanKind::Server,
        "the ingest-logs batch span is SERVER",
    );
    assert_eq!(
        commit[0].span_kind,
        SpanKind::Internal,
        "commit-wal is INTERNAL",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rfc0038_2_span_count_is_constant_in_record_count() {
    let one = ingest_and_collect_spans(1).await;
    let many = ingest_and_collect_spans(64).await;

    assert_eq!(
        one.len(),
        many.len(),
        "span count must not grow with record count (no per-record spans): \
         1 record -> {} spans, 64 records -> {} spans",
        one.len(),
        many.len(),
    );
    // The two request-scope spans, and nothing per-record.
    assert_eq!(
        many.len(),
        2,
        "ingest-logs + commit-wal only, got {:?}",
        many.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
    );
}
