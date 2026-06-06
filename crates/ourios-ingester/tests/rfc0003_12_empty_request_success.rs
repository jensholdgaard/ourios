//! RFC0003.12 — Empty `ExportLogsServiceRequest` returns success without WAL write.
//!
//! All three "zero `LogRecord`" shapes — empty `resource_logs`, a
//! Resource with empty `scope_logs`, and a `ScopeLogs` with empty
//! `log_records` — are accepted as success with no `OtlpBatch` frame
//! appended and no record reaching the miner.

mod ingest_support;

use ingest_support::{
    open_pipeline, replay_frames, request, resource_logs, resource_logs_without_scopes,
};
use ourios_core::tenant::TenantId;

/// Scenario RFC0003.12 — Empty `ExportLogsServiceRequest` returns success without WAL write.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_12_empty_request_succeeds_without_persisting() {
    let empty_shapes = [
        // (i) no ResourceLogs at all.
        request(vec![]),
        // (ii) a Resource that resolves, but no ScopeLogs.
        request(vec![resource_logs_without_scopes("svc")]),
        // (iii) a ScopeLogs with zero LogRecords.
        request(vec![resource_logs("svc", &[])]),
    ];

    for export in empty_shapes {
        // Arrange
        let tmp = tempfile::TempDir::new().expect("temp");
        let mut pipeline = open_pipeline(tmp.path());

        // Act
        let ingested = pipeline
            .ingest(export)
            .expect("an empty request is accepted");

        // Assert: success with zero records, miner untouched, no frame.
        assert_eq!(ingested, 0, "no records ingested");
        assert_eq!(
            pipeline.miner().template_count(&TenantId::new("svc")),
            0,
            "no record reached the miner",
        );
        drop(pipeline);
        assert!(
            replay_frames(tmp.path()).is_empty(),
            "no OtlpBatch frame was appended for an empty request",
        );
    }
}
