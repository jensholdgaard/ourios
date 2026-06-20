//! Scenario RFC0005.11 — Row-vs-path validation on partition mismatch
//! (audit-stream axis).
//! See `docs/rfcs/0005-parquet-storage.md` §5.
//!
//! Mirrors `tests/row_vs_path_validation.rs` for the audit
//! stream. The audit-side validation runs on a *coarser* set of
//! axes than the data side: only `tenant_id` plus `year` /
//! `month` / `day` (the audit partition path has no `hour`
//! segment per §3.4). The §3.9 reader contract still fires on a
//! mismatch.

use std::time::{Duration, UNIX_EPOCH};

use ourios_core::audit::{AuditEvent, AuditPayload, TemplateChange, hash_triggering_line};
use ourios_core::tenant::TenantId;
use ourios_parquet::{AuditReader, AuditReaderError, AuditWriter, PartitionKey};
use tempfile::TempDir;

fn widening_event(tenant: &str, ts_secs: u64) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: UNIX_EPOCH + Duration::from_secs(ts_secs),
        payload: AuditPayload::Template {
            template_id: 7,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change: TemplateChange::Widened {
                old_version: 1,
                new_version: 2,
                old_template: "[\"user\",\"<*>\"]".to_string(),
                new_template: "[\"user\",\"<*>\",\"<*>\"]".to_string(),
                positions_widened: vec![1],
            },
        },
    }
}

fn partition_for(event: &AuditEvent) -> PartitionKey {
    use ourios_core::record::{BodyKind, MinedRecord};
    let nanos = event
        .timestamp
        .duration_since(UNIX_EPOCH)
        .expect("post-epoch")
        .as_nanos();
    let ns = u64::try_from(nanos).expect("fits u64");
    let proxy = MinedRecord {
        tenant_id: event.tenant_id.clone(),
        template_id: 0,
        template_version: 0,
        severity_number: 0,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: ns,
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: Vec::new(),
        separators: vec![String::new()],
        body: None,
        confidence: 0.0,
        lossy_flag: false,
    };
    PartitionKey::derive(&proxy).expect("derive")
}

/// Open with a deliberately mismatching `tenant_id`: reader
/// errors with `PartitionMismatch`.
#[test]
fn rfc0005_11_audit_tenant_mismatch_returns_partition_mismatch_error() {
    let bucket = TempDir::new().unwrap();
    let event = widening_event("tenant-a", 1_775_127_480);
    let written_partition = partition_for(&event);

    let mut writer = AuditWriter::open(bucket.path(), written_partition.clone()).unwrap();
    writer.append_events(&[event]).unwrap();
    let written = writer.close().unwrap();

    let wrong_partition = PartitionKey {
        tenant_id: "tenant-b".to_string(),
        ..written_partition
    };

    let reader = AuditReader::open_partition(&written.path, wrong_partition).unwrap();
    let err = reader
        .read_all()
        .expect_err("mismatching tenant must error");
    match err {
        AuditReaderError::PartitionMismatch {
            row_index,
            expected,
            actual,
            ..
        } => {
            assert_eq!(row_index, 0);
            assert_eq!(expected.tenant_id, "tenant-b");
            assert_eq!(actual.tenant_id, "tenant-a");
        }
        other => panic!("expected PartitionMismatch, got {other:?}"),
    }
}

/// Open with the wrong day: reader errors.
#[test]
fn rfc0005_11_audit_day_mismatch_returns_partition_mismatch_error() {
    let bucket = TempDir::new().unwrap();
    let event = widening_event("tenant-x", 1_775_127_480);
    let written_partition = partition_for(&event);

    let mut writer = AuditWriter::open(bucket.path(), written_partition.clone()).unwrap();
    writer.append_events(&[event]).unwrap();
    let written = writer.close().unwrap();

    // Shift the day by +1 (within month, so no month/year carry
    // arithmetic needed). The audit row-vs-path check fires on
    // day axis specifically.
    let wrong_partition = PartitionKey {
        day: written_partition.day + 1,
        ..written_partition
    };

    let reader = AuditReader::open_partition(&written.path, wrong_partition).unwrap();
    let err = reader.read_all().expect_err("mismatching day must error");
    assert!(matches!(err, AuditReaderError::PartitionMismatch { .. }));
}

/// The hour field is part of the `PartitionKey` shape but ignored
/// on the audit axis — different hours on the same day must
/// validate cleanly (the §3.4 "audit partitioning stops at day"
/// rule).
#[test]
fn rfc0005_11_audit_different_hour_same_day_validates_cleanly() {
    let bucket = TempDir::new().unwrap();
    let event = widening_event("tenant-x", 1_775_127_480);
    let written_partition = partition_for(&event);

    let mut writer = AuditWriter::open(bucket.path(), written_partition.clone()).unwrap();
    writer.append_events(&[event]).unwrap();
    let written = writer.close().unwrap();

    // Bump the hour field — the audit reader must NOT treat
    // this as a mismatch since the audit path has no hour
    // segment.
    let same_day_diff_hour = PartitionKey {
        hour: (written_partition.hour + 1) % 24,
        ..written_partition
    };

    let reader = AuditReader::open_partition(&written.path, same_day_diff_hour).unwrap();
    let events = reader
        .read_all()
        .expect("same-day, different-hour validates cleanly");
    assert_eq!(events.len(), 1);
}

/// Diagnostic `open_file` mode skips row-vs-path validation —
/// events that would error under `open_partition` surface as-
/// stored.
#[test]
fn audit_open_file_skips_partition_validation() {
    let bucket = TempDir::new().unwrap();
    let event = widening_event("tenant-a", 1_775_127_480);
    let partition = partition_for(&event);

    let mut writer = AuditWriter::open(bucket.path(), partition).unwrap();
    writer.append_events(std::slice::from_ref(&event)).unwrap();
    let written = writer.close().unwrap();

    let reader = AuditReader::open_file(&written.path).unwrap();
    let events = reader
        .read_all()
        .expect("open_file never partition-validates");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].tenant_id.as_str(), event.tenant_id.as_str());
}
