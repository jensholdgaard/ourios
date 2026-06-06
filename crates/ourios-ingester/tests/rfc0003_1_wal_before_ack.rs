//! RFC0003.1 — WAL-before-ack `[§3.4]`.
//!
//! A non-empty batch is acked only after its `OtlpBatch` frame is
//! durably written: when `ingest` returns `Ok`, a fresh WAL replay
//! finds the frame on disk (it was fsync'd before the ack), and its
//! payload recovers the export.

mod ingest_support;

use ingest_support::{open_pipeline, replay_frames, request, resource_logs};
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use ourios_core::tenant::TenantId;
use ourios_ingester::receiver::decode_protobuf;
use ourios_wal::FrameKind;

/// Scenario RFC0003.1 — WAL-before-ack.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_1_no_ack_until_the_batch_is_durable() {
    // Arrange
    let tmp = tempfile::TempDir::new().expect("temp");
    let mut pipeline = open_pipeline(tmp.path());
    let export = request(vec![resource_logs("checkout", &["user 1 logged in"])]);

    // Act
    let ingested = pipeline.ingest(export).expect("the batch is acked");

    // Assert: the ack reflects one record, which reached the miner only
    // after durability (§6.5 step ordering). One distinct line → one
    // template.
    assert_eq!(ingested, 1);
    assert_eq!(
        pipeline.miner().template_count(&TenantId::new("checkout")),
        1,
        "the one record reached the miner as one template",
    );

    // The ack returned only after the frame was fsync'd: reopening the
    // WAL and replaying finds exactly one durable OtlpBatch frame whose
    // payload recovers the export content (not just its shape).
    drop(pipeline);
    let frames = replay_frames(tmp.path());
    assert_eq!(frames.len(), 1, "exactly one OtlpBatch frame is durable");
    assert_eq!(frames[0].0, FrameKind::OtlpBatch);
    let recovered = decode_protobuf(&frames[0].1)
        .expect("the frame payload is a valid ExportLogsServiceRequest");
    let body = recovered.resource_logs[0].scope_logs[0].log_records[0]
        .body
        .as_ref()
        .and_then(|b| b.value.as_ref());
    assert!(
        matches!(body, Some(Value::StringValue(s)) if s == "user 1 logged in"),
        "the durable frame recovers the record's body verbatim, got {body:?}",
    );
}
