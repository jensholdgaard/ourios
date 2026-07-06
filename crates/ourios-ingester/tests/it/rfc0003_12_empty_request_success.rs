//! RFC0003.12 — Empty `ExportLogsServiceRequest` returns success without WAL write.
//!
//! All three "zero `LogRecord`" shapes — empty `resource_logs`, a
//! Resource with empty `scope_logs`, and a `ScopeLogs` with empty
//! `log_records` — are accepted as success with no `OtlpBatch` frame
//! appended and no record reaching the miner.

use crate::ingest_support::{
    CallLog, request, resource_logs, resource_logs_without_scopes, spy_pipeline,
};
use ourios_core::tenant::TenantId;

/// Scenario RFC0003.12 — Empty `ExportLogsServiceRequest` returns success without WAL write.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0003_12_empty_request_succeeds_without_persisting() {
    let empty_shapes = [
        // (i) no ResourceLogs at all.
        request(vec![]),
        // (ii) a Resource that resolves, but no ScopeLogs.
        request(vec![resource_logs_without_scopes("svc")]),
        // (iii) a ScopeLogs with zero LogRecords.
        request(vec![resource_logs("svc", &[])]),
    ];

    for export in empty_shapes {
        // Arrange: a spy Journal so we can assert the WAL is touched not
        // at all (no append *and* no sync — stronger than "no frame").
        let log: CallLog = CallLog::default();
        let pipeline = spy_pipeline(log.clone());

        // Act
        let ingested = pipeline
            .ingest(export)
            .await
            .expect("an empty request is accepted");

        // Assert: success with zero records, the WAL untouched, and the
        // miner untouched.
        assert_eq!(ingested, 0, "no records ingested");
        assert!(
            log.lock().expect("call log").is_empty(),
            "an empty request neither appends nor syncs the WAL",
        );
        assert_eq!(
            pipeline.with_miner(|m| m.template_count(&TenantId::new("svc"))),
            0,
            "no record reached the miner",
        );
    }
}
