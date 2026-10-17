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

use crate::ingest_support::{
    failing_append_pipeline_transient, open_pipeline, oversize_append_pipeline, request,
    resource_logs,
};
use opentelemetry::trace::{SpanId, SpanKind, Status};
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
        .with(tracing_opentelemetry::layer().with_tracer(ourios_telemetry::test_tracer(&provider)));

    let bodies = distinct_bodies(record_count);
    let body_refs: Vec<&str> = bodies.iter().map(String::as_str).collect();
    let export = request(vec![resource_logs("checkout", &body_refs)]);

    let acked = pipeline
        .ingest_bound(
            export,
            ourios_core::tenant::TenantId::new("checkout"),
            None,
            false,
        )
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

/// The batch span names the tenant it ingested for, so ingest is
/// attributable per tenant.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn the_ingest_span_carries_the_batch_tenant() {
    let spans = ingest_and_collect_spans(1).await;
    let [batch] = spans_named(&spans, "ingest logs")[..] else {
        panic!("one ingest-logs span, got {spans:?}");
    };
    let tenant = batch
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == ourios_semconv::OURIOS_TENANT)
        .map(|kv| kv.value.as_str().into_owned());
    assert_eq!(tenant.as_deref(), Some("checkout"));
}

fn int_attribute(span: &SpanData, key: &str) -> Option<i64> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| match kv.value {
            opentelemetry::Value::I64(v) => v,
            ref other => panic!("{key} should be an int, got {other:?}"),
        })
}

/// RFC 0038 §3.5: the batch span carries the batch's record count, and
/// the byte length of the WAL frame payload it committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn the_ingest_span_carries_the_batch_record_count_and_size() {
    let spans = ingest_and_collect_spans(3).await;
    let [batch] = spans_named(&spans, "ingest logs")[..] else {
        panic!("one ingest-logs span, got {spans:?}");
    };
    assert_eq!(
        int_attribute(batch, ourios_semconv::OURIOS_INGEST_BATCH_RECORD_COUNT),
        Some(3),
    );

    let bodies = distinct_bodies(3);
    let body_refs: Vec<&str> = bodies.iter().map(String::as_str).collect();
    let export = request(vec![resource_logs("checkout", &body_refs)]);
    let payload =
        ourios_wal::TenantBatch::encode("checkout", &prost::Message::encode_to_vec(&export))
            .expect("tenant frame");
    assert_eq!(
        int_attribute(batch, ourios_semconv::OURIOS_INGEST_BATCH_SIZE),
        Some(i64::try_from(payload.len()).expect("fits")),
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

/// Drive one batch through `pipeline`, whose WAL append fails, and return
/// the finished spans.
async fn failed_ingest_spans(pipeline: ourios_ingester::receiver::IngestPipeline) -> Vec<SpanData> {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(ourios_telemetry::test_tracer(&provider)));
    let export = request(vec![resource_logs("checkout", &["a line"])]);
    pipeline
        .ingest_bound(
            export,
            ourios_core::tenant::TenantId::new("checkout"),
            None,
            false,
        )
        .with_subscriber(subscriber)
        .await
        .expect_err("the append fails");
    provider.force_flush().expect("spans flush");
    exporter.get_finished_spans().expect("spans exported")
}

fn error_type(span: &SpanData) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == "error.type")
        .map(|kv| kv.value.as_str().into_owned())
}

/// A WAL that cannot take the batch fails the `commit wal` span itself, and
/// the server span above it, since the server is at fault.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_failed_wal_commit_marks_the_commit_span_and_the_batch_span() {
    let spans = failed_ingest_spans(failing_append_pipeline_transient()).await;
    for name in ["commit wal", "ingest logs"] {
        let [span] = spans_named(&spans, name)[..] else {
            panic!("one {name} span, got {spans:?}");
        };
        assert!(
            matches!(span.status, Status::Error { .. }),
            "{name}: {:?}",
            span.status
        );
        assert_eq!(error_type(span).as_deref(), Some("unavailable"), "{name}");
    }
}

/// An oversize batch fails its commit, but it is the client's fault, so the
/// server span stays unset (HTTP 413 / gRPC `INVALID_ARGUMENT`).
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn an_oversize_batch_fails_the_commit_span_but_not_the_batch_span() {
    let spans = failed_ingest_spans(oversize_append_pipeline()).await;
    let [commit] = spans_named(&spans, "commit wal")[..] else {
        panic!("one commit span, got {spans:?}");
    };
    assert!(matches!(commit.status, Status::Error { .. }));
    assert_eq!(error_type(commit).as_deref(), Some("too_large"));
    let [batch] = spans_named(&spans, "ingest logs")[..] else {
        panic!("one batch span");
    };
    assert_eq!(batch.status, Status::Unset);
    assert_eq!(error_type(batch), None);
}
