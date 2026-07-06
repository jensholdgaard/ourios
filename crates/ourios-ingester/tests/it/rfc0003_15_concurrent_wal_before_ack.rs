//! RFC0003.15 — Concurrent `Export` calls each obey WAL-before-ack independently.
//!
//! N concurrent gRPC `Export` calls share one pipeline (the group-commit
//! coordinator serializes appends to the single-writer WAL and folds
//! their fsyncs into a window — RFC0008.8). Each call is durable before
//! it acks, so all N succeed and the WAL ends with exactly N durable
//! `OtlpBatch` frames — none lost or interleaved away.

use std::sync::Arc;

use crate::ingest_support::{replay_frames, request, resource_logs, shared_wal_pipeline};
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsService;
use ourios_ingester::receiver::grpc::LogsReceiver;
use ourios_wal::FrameKind;
use tonic::Request;

const N: usize = 8;

/// Scenario RFC0003.15 — Concurrent `Export` calls each obey WAL-before-ack independently.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0003_15_concurrent_exports_are_each_durable() {
    // Arrange: one shared pipeline over a real WAL, N concurrent callers.
    let tmp = tempfile::TempDir::new().expect("temp");
    let receiver = Arc::new(LogsReceiver::new(shared_wal_pipeline(tmp.path())));

    // Act: fire N concurrent Export calls.
    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let receiver = Arc::clone(&receiver);
        handles.push(tokio::spawn(async move {
            let body = format!("line {i}");
            let export = request(vec![resource_logs("checkout", &[body.as_str()])]);
            receiver.export(Request::new(export)).await
        }));
    }

    // Assert: every call acked.
    for handle in handles {
        let response = handle.await.expect("task joins");
        assert!(response.is_ok(), "each concurrent Export acks");
    }

    // ...and every batch is durable: the WAL holds exactly N OtlpBatch
    // frames (none lost or dropped under concurrency). Drop the last
    // pipeline ref so the WAL handle is released before replay.
    drop(receiver);
    let frames = replay_frames(tmp.path());
    assert_eq!(frames.len(), N, "one durable frame per concurrent Export");
    assert!(
        frames.iter().all(|(kind, _)| *kind == FrameKind::OtlpBatch),
        "every durable frame is an OtlpBatch",
    );
}
