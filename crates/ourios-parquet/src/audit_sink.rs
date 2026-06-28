//! A durable [`AuditSink`] backed by the RFC 0005 §3.7 audit Parquet
//! stream.

use ourios_core::audit::{AuditEvent, AuditSink};

use crate::audit_writer::{AuditWriter, AuditWriterError, derive_audit_partition};
use crate::store::Store;

/// An [`AuditSink`] that persists each event to the RFC 0005 §3.7
/// audit Parquet stream through the [`Store`] (local or S3, RFC 0019).
///
/// Each [`emit`](AuditSink::emit) derives the event's audit partition
/// (tenant + UTC day, RFC 0005 §3.4) and writes a single-event file at
/// `audit/tenant_id=…/year=…/month=…/day=…/<uuid>.parquet`. Compaction
/// audit events are low-volume — at most one per compacted partition
/// per sweep — so a file-per-event is acceptable here; batching and the
/// RFC 0001 §6.4 crash-ordering barrier arrive with the WAL-backed sink
/// (RFC 0005 §3.7 / RFC 0008), which supersedes this best-effort
/// pre-WAL persistence (and for which the §6.4 barrier is moot anyway:
/// a compaction commits *before* its audit event).
///
/// `emit` is infallible per the [`AuditSink`] contract, so a write
/// failure increments [`Self::write_failures`] rather than propagating
/// — surfaced through that counter until the logging / metrics
/// bootstrap can report it (`CLAUDE.md` §6.3).
#[derive(Debug)]
pub struct ParquetAuditSink {
    store: Store,
    write_failures: u64,
}

impl ParquetAuditSink {
    /// A sink writing the audit stream through `store` (local or S3).
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self {
            store,
            write_failures: 0,
        }
    }

    /// Count of events that failed to persist. Non-zero means audit
    /// data was lost — best-effort is the pre-WAL contract.
    #[must_use]
    pub fn write_failures(&self) -> u64 {
        self.write_failures
    }

    fn try_write(&self, event: &AuditEvent) -> Result<(), AuditWriterError> {
        let partition = derive_audit_partition(event)?;
        let mut writer = AuditWriter::open_in(&self.store, partition)?;
        writer.append_events(std::slice::from_ref(event))?;
        writer.close()?;
        Ok(())
    }
}

impl AuditSink for ParquetAuditSink {
    fn emit(&mut self, event: AuditEvent) {
        if self.try_write(&event).is_err() {
            self.write_failures = self.write_failures.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AuditReader;
    use ourios_core::audit::AuditPayload;
    use ourios_core::tenant::TenantId;
    use std::time::{Duration, UNIX_EPOCH};

    fn compaction_event() -> AuditEvent {
        AuditEvent {
            tenant_id: TenantId::new("acme"),
            // 2026-04-02T10:58:00Z.
            timestamp: UNIX_EPOCH + Duration::from_secs(1_775_127_480),
            payload: AuditPayload::Compaction {
                partition: "year=2026/month=04/day=02/hour=10".to_string(),
                input_files: vec!["a.parquet".to_string(), "b.parquet".to_string()],
                output_file: "c.parquet".to_string(),
                generation: 7,
                rows: 100,
            },
        }
    }

    #[test]
    fn persists_an_event_to_the_audit_stream() {
        // Arrange
        let bucket = tempfile::tempdir().expect("temp");
        let mut sink = ParquetAuditSink::new(Store::local(bucket.path()).expect("store"));
        let event = compaction_event();

        // Act
        sink.emit(event.clone());

        // Assert — no failures, and the event reads back byte-for-byte
        // from its derived audit partition.
        assert_eq!(sink.write_failures(), 0);
        let partition = derive_audit_partition(&event).expect("derive");
        let dir = partition.audit_path(bucket.path());
        let file = std::fs::read_dir(&dir)
            .expect("partition dir exists")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "parquet"))
            .expect("one .parquet file written");
        let read = AuditReader::open_partition(&file, partition)
            .expect("open")
            .read_all()
            .expect("read_all");
        assert_eq!(read, vec![event]);
    }
}
