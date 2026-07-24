//! RFC0038.3 — span context survives the `tokio::spawn` / `spawn_blocking`
//! boundaries.
//!
//! Harness-exempt (RFC0028.2, see `tests/README.md`): this installs the
//! **process-global** `OTel` tracer, which can't share a process with the
//! consolidated `it` harness or another global installer — hence its own
//! top-level binary with a single test.
//!
//! A **global** (not scoped) tracer is required here, and that is the whole
//! point: the receiver's ingest runs on a `tokio::spawn`ed task (grpc.rs) and
//! the compactor's sweep on a `spawn_blocking`ed one (compactor.rs). Neither
//! carries a *scoped* subscriber across the spawn — only the global default
//! reaches the span opened *inside* the spawned callee. Asserting those spans
//! are still present (and that the `commit wal` child nests correctly under
//! the `ingest logs` batch span, both minted inside the spawned task) proves
//! the "`tokio::spawn` context trap is closed" contract: whatever runs under
//! the span — child spans or log lines — resolves to the batch's trace.

#[path = "it/ingest_support/mod.rs"]
mod ingest_support;

use std::sync::mpsc;
use std::time::Duration;

use ingest_support::{request, resource_logs, shared_wal_pipeline};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsService as _;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use ourios_ingester::Compactor;
use ourios_ingester::receiver::grpc::LogsReceiver;
use ourios_parquet::{CompactionPolicy, Store};
use tracing_subscriber::prelude::*;

fn spans_named<'a>(spans: &'a [SpanData], name: &str) -> Vec<&'a SpanData> {
    spans.iter().filter(|s| s.name.as_ref() == name).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0038_3_span_context_survives_spawn_boundaries() {
    // Install the process-global tracer over an in-memory exporter. `try_init`
    // is the global-default subscriber install; the tracer provider global is
    // set alongside so the tracing-opentelemetry layer resolves it inside
    // spawned tasks. Keep `provider` alive until after the flush below.
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("ourios-test")))
        .try_init()
        .expect("install global subscriber");
    opentelemetry::global::set_tracer_provider(provider.clone());

    // --- Boundary 1: the receiver's `tokio::spawn`ed ingest (grpc.rs). ---
    // Call the gRPC handler directly — the spawn is inside `export`, so no
    // socket is needed to cross it, and `export` awaits the spawned task, so
    // the `ingest logs` span is finished when this returns.
    let ingest_tmp = tempfile::tempdir().expect("temp");
    let receiver = LogsReceiver::new(shared_wal_pipeline(ingest_tmp.path()));
    receiver
        .export(tonic::Request::new(request(vec![resource_logs(
            "checkout",
            &["alpha", "beta"],
        )])))
        .await
        .expect("export acks");

    // --- Boundary 2: the compactor's `spawn_blocking`ed sweep (compactor.rs). ---
    // An empty store still emits the sweep span (instrument-on-entry). The
    // daemon's `tokio::time::interval` fires its first tick immediately, so a
    // long interval yields exactly one sweep before the abort — no second tick,
    // no span pile-up.
    let sweep_tmp = tempfile::tempdir().expect("temp");
    let store = Store::local(sweep_tmp.path()).expect("local store");
    let compactor = Compactor::new(
        store,
        CompactionPolicy::default(),
        Duration::from_secs(3600),
    );
    let (tx, rx) = mpsc::channel();
    let daemon = tokio::spawn(compactor.run(move |result| {
        let _ = tx.send(result.is_ok());
    }));
    let sweep_ok = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(5)))
        .await
        .expect("join")
        .expect("a sweep ran within 5s");
    daemon.abort();
    assert!(
        sweep_ok,
        "the sweep succeeded (so its span reflects real work)"
    );

    provider.force_flush().expect("spans flush");
    let spans = exporter.get_finished_spans().expect("spans exported");

    // Boundary 1 assertions — the batch span crossed `tokio::spawn`, and its
    // WAL-commit child nests correctly (context propagated within the task).
    let batch = spans_named(&spans, "ingest logs");
    let commit = spans_named(&spans, "commit wal");
    assert_eq!(
        batch.len(),
        1,
        "one `ingest logs` span survived tokio::spawn, got {:?}",
        spans.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
    );
    assert_eq!(commit.len(), 1, "one `commit wal` span, got {spans:?}");
    assert_eq!(
        commit[0].parent_span_id,
        batch[0].span_context.span_id(),
        "`commit wal` nests under `ingest logs` across the spawn boundary",
    );
    assert_eq!(
        commit[0].span_context.trace_id(),
        batch[0].span_context.trace_id(),
        "child and batch share one trace",
    );

    // Boundary 2 assertion — exactly one `sweep partitions` span crossed
    // `spawn_blocking` (the long interval means a single tick before abort).
    let sweep = spans_named(&spans, "sweep partitions");
    assert_eq!(
        sweep.len(),
        1,
        "one `sweep partitions` span survived spawn_blocking, got {:?}",
        spans.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
    );
}
