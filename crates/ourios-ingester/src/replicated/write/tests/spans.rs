//! RFC 0038 §3.5 on the replicated path: one `ingest logs` server span per
//! export with one `commit wal` child covering the group commit.

use std::time::Duration;

use opentelemetry::trace::{SpanKind, Status};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::prelude::*;

use super::{export, followers, ready_leader, writer};
use crate::receiver::ReceiveError;
use crate::replicated::ingest::{Ingested, ReplicatedIngest};
use crate::replicated::raft::cluster::Cluster;

/// Ingest one export carrying `line` under a scoped tracer; its outcome
/// and every span.
async fn traced(
    ingest: &ReplicatedIngest,
    line: &str,
) -> (Result<Ingested, ReceiveError>, Vec<SpanData>) {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(ourios_telemetry::test_tracer(&provider)));
    let outcome = ingest
        .ingest(export(line))
        .with_subscriber(subscriber)
        .await;
    provider.force_flush().expect("flush");
    (outcome, exporter.get_finished_spans().expect("spans"))
}

fn one<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    let [span] = spans.iter().filter(|s| s.name == name).collect::<Vec<_>>()[..] else {
        panic!("one {name} span, got {spans:?}");
    };
    span
}

fn error_type(span: &SpanData) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == "error.type")
        .map(|kv| kv.value.as_str().into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_committed_export_has_one_batch_span_with_one_commit_child() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let ingest = ReplicatedIngest::new(writer(&cluster, leader));
    let (outcome, spans) = traced(&ingest, "traced").await;
    outcome.expect("acknowledged");

    let batch = one(&spans, "ingest logs");
    let commit = one(&spans, "commit wal");
    assert_eq!(commit.parent_span_id, batch.span_context.span_id());
    assert_eq!(batch.span_kind, SpanKind::Server);
    assert_eq!(commit.span_kind, SpanKind::Internal);
    for span in [batch, commit] {
        assert_eq!(span.status, Status::Unset, "{}", span.name);
    }
    cluster.shutdown().await;
}

/// Without a quorum the commit span fails with the rejection's own
/// `error.type`, and the server span with the transport class.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commit_without_a_quorum_fails_both_spans() {
    let mut cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    for f in followers(leader) {
        cluster.kill(f).await;
    }
    let writer = writer(&cluster, leader).with_timeout(Duration::from_millis(300));
    let (outcome, spans) = traced(&ReplicatedIngest::new(writer), "traced").await;
    outcome.expect_err("no quorum");

    for (name, expected) in [
        ("commit wal", "not_enough_replicas"),
        ("ingest logs", "unavailable"),
    ] {
        let span = one(&spans, name);
        assert!(matches!(span.status, Status::Error { .. }), "{name}");
        assert_eq!(error_type(span).as_deref(), Some(expected), "{name}");
    }
    cluster.shutdown().await;
}

/// An export too large for one WAL frame fails its commit, but it is the
/// client's fault, so the server span stays unset (HTTP 413 / gRPC
/// `RESOURCE_EXHAUSTED`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oversize_export_fails_the_commit_span_but_not_the_batch_span() {
    let cluster = Cluster::start(3, &[0]).await;
    let leader = ready_leader(&cluster).await;
    let ingest = ReplicatedIngest::new(writer(&cluster, leader));
    let line = "x".repeat(ourios_wal::ReplicatedRecord::MAX_ENTRY_PAYLOAD_BYTES + 1);
    let (outcome, spans) = traced(&ingest, &line).await;
    outcome.expect_err("too large");

    let commit = one(&spans, "commit wal");
    assert!(matches!(commit.status, Status::Error { .. }));
    assert_eq!(error_type(commit).as_deref(), Some("request_too_large"));
    let batch = one(&spans, "ingest logs");
    assert_eq!(batch.status, Status::Unset);
    assert_eq!(error_type(batch), None);
    cluster.shutdown().await;
}
