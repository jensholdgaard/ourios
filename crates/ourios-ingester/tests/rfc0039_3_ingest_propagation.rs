//! RFC0039.3 — the caller's trace context survives the ingest `tokio::spawn`.
//!
//! Harness-exempt (RFC0028.2, see `tests/README.md`): installs the
//! **process-global** `OTel` tracer, which cannot share a process with the
//! consolidated `it` harness or another global installer.
//!
//! The `ingest logs` span is created inside `ingest_bound`, *after* the
//! receivers' `tokio::spawn` — a boundary ambient context does not cross. Both
//! handlers therefore extract the caller's context while the request is still in
//! hand and re-attach it inside the spawned task (RFC 0039 §3.3). What follows
//! asserts the observable consequence for both OTLP transports: the batch span,
//! and the `commit wal` child minted under it, land in the *caller's* trace
//! rather than a fresh one.
//!
//! The two propagated arms carry **different** traces so a single exporter can
//! hold both and each span still be attributed unambiguously. A third arm sends
//! no `traceparent` at all and asserts that batch is a fresh, valid root —
//! RFC0039.2 for the ingest path, which propagation must leave exactly as it
//! was. `rfc0038_3_spawn_boundary.rs` also exercises the untraced path, but
//! asserts only that the spans survive the spawn, not their parentage; it is
//! deliberately left untouched.

#[path = "it/ingest_support/mod.rs"]
mod ingest_support;

use axum::body::Body;
use axum::http::{Request, header};
use ingest_support::{grpc_request, request, resource_logs, shared_wal_pipeline};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsService as _;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use ourios_ingester::receiver::grpc::LogsReceiver;
use ourios_ingester::receiver::http::{HttpConfig, router};
use prost::Message as _;
use tower::ServiceExt as _;
use tracing_subscriber::prelude::*;

/// The trace the gRPC caller claims to be inside.
const GRPC_TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const GRPC_SPAN: &str = "00f067aa0ba902b7";
/// …and the HTTP caller's, distinct from it.
const HTTP_TRACE: &str = "0af7651916cd43dd8448eb211c80319c";
const HTTP_SPAN: &str = "b7ad6b7169203331";

fn traceparent(trace: &str, span: &str) -> String {
    format!("00-{trace}-{span}-01")
}

/// The spans named `name` that resolved into trace `trace`.
fn spans_in_trace<'a>(spans: &'a [SpanData], trace: &str, name: &str) -> Vec<&'a SpanData> {
    spans
        .iter()
        .filter(|s| s.name.as_ref() == name && s.span_context.trace_id().to_string() == trace)
        .collect()
}

/// Assert that one `ingest logs` span joined `trace` as a child of `parent_span`,
/// and that its `commit wal` child nests under it inside that same trace.
fn assert_batch_joined_caller_trace(spans: &[SpanData], trace: &str, parent_span: &str, arm: &str) {
    let batch = spans_in_trace(spans, trace, "ingest logs");
    assert_eq!(
        batch.len(),
        1,
        "{arm}: exactly one `ingest logs` span in the caller's trace, got {:?}",
        spans
            .iter()
            .map(|s| (s.name.clone(), s.span_context.trace_id().to_string()))
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        batch[0].parent_span_id.to_string(),
        parent_span,
        "{arm}: the batch span is parented to the caller's span across the spawn",
    );

    let commit = spans_in_trace(spans, trace, "commit wal");
    assert_eq!(
        commit.len(),
        1,
        "{arm}: exactly one `commit wal` span in the caller's trace",
    );
    assert_eq!(
        commit[0].parent_span_id,
        batch[0].span_context.span_id(),
        "{arm}: `commit wal` still nests under `ingest logs` once the batch span has a remote parent",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0039_3_extracted_context_survives_the_ingest_spawn() {
    // The global tracer, as in `rfc0038_3_spawn_boundary.rs`: only the global
    // default reaches a span opened inside a spawned task. The global
    // *propagator* is what the handlers' extraction resolves through — in
    // production `ourios_telemetry::init` installs it (RFC 0039 §3.1).
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("ourios-test")))
        .try_init()
        .expect("install global subscriber");
    opentelemetry::global::set_tracer_provider(provider.clone());
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

    // --- Arm A: OTLP/gRPC. The spawn is inside `export`, which awaits it, so
    // the batch span is finished by the time this returns. ---
    let grpc_tmp = tempfile::tempdir().expect("temp");
    let receiver = LogsReceiver::new(shared_wal_pipeline(grpc_tmp.path()));
    let mut traced = grpc_request(request(vec![resource_logs("checkout", &["alpha", "beta"])]));
    traced.metadata_mut().insert(
        "traceparent",
        traceparent(GRPC_TRACE, GRPC_SPAN)
            .parse()
            .expect("valid metadata value"),
    );
    receiver.export(traced).await.expect("export acks");

    // --- Arm B: OTLP/HTTP, driven in-process through the real router. ---
    let http_tmp = tempfile::tempdir().expect("temp");
    let config = HttpConfig::default();
    let app = router(shared_wal_pipeline(http_tmp.path()), &config);
    let body = request(vec![resource_logs("payments", &["gamma"])]).encode_to_vec();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&config.path)
                .header(header::CONTENT_TYPE, "application/x-protobuf")
                .header("x-ourios-tenant", "checkout")
                .header("traceparent", traceparent(HTTP_TRACE, HTTP_SPAN))
                .body(Body::from(body))
                .expect("build request"),
        )
        .await
        .expect("oneshot");
    assert!(
        response.status().is_success(),
        "the HTTP export should be acked, got {}",
        response.status(),
    );

    // --- Arm C (RFC0039.2, ingest): no traceparent at all. ---
    let root_tmp = tempfile::tempdir().expect("temp");
    LogsReceiver::new(shared_wal_pipeline(root_tmp.path()))
        .export(grpc_request(request(vec![resource_logs(
            "billing",
            &["delta"],
        )])))
        .await
        .expect("export acks");

    provider.force_flush().expect("spans flush");
    let spans = exporter.get_finished_spans().expect("spans exported");

    assert_batch_joined_caller_trace(&spans, GRPC_TRACE, GRPC_SPAN, "gRPC");
    assert_batch_joined_caller_trace(&spans, HTTP_TRACE, HTTP_SPAN, "HTTP");

    // The untraced batch is a fresh, valid root — the pre-RFC behaviour, which
    // propagation must leave alone (RFC0039.2). Asserted explicitly rather than
    // inferred from "no traceparent was sent": a bug that invented a parent, or
    // one that dropped the trace id, would look identical from the outside.
    let rooted: Vec<_> = spans
        .iter()
        .filter(|s| {
            s.name.as_ref() == "ingest logs" && {
                let trace = s.span_context.trace_id().to_string();
                trace != GRPC_TRACE && trace != HTTP_TRACE
            }
        })
        .collect();
    assert_eq!(
        rooted.len(),
        1,
        "one `ingest logs` span outside both callers' traces, got {:?}",
        spans
            .iter()
            .map(|s| (s.name.clone(), s.span_context.trace_id().to_string()))
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        rooted[0].parent_span_id,
        opentelemetry::trace::SpanId::INVALID,
        "a batch with no inbound context has no parent",
    );
    assert_ne!(
        rooted[0].span_context.trace_id(),
        opentelemetry::trace::TraceId::INVALID,
        "and still gets a real trace id of its own",
    );
}
