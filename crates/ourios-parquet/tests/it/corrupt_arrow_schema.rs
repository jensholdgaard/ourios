//! A file read back from the store whose `ARROW:schema` footer entry is
//! corrupt still decodes: every reader derives its Arrow schema from the
//! Parquet schema and ignores that entry, which `arrow-ipc` would panic on
//! (`unimplemented!("Type NONE not supported")`) instead of returning an
//! error. Found by the `audit_reader` fuzz target (RFC 0015).

use std::time::{Duration, UNIX_EPOCH};

use arrow_array::RecordBatch;
use ourios_core::audit::{AuditEvent, AuditPayload, ParamType, TemplateChange};
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{
    AuditReader, IdColumns, Reader, audit_events_to_batch, decoded_max, mined_records_to_batch,
};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_writer::ArrowWriterOptions;
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

/// The IPC schema `parquet::arrow::encode_arrow_schema` writes for one
/// non-nullable `Int32` column `x`, base64 as the footer stores it, with
/// the field's type tag (decoded byte 91) zeroed to `Type::NONE`. It passes
/// the flatbuffer verifier, and `arrow-ipc` 59 panics converting it.
const CORRUPT_ARROW_SCHEMA: &str = "/////3gAAAAQAAAAAAAKAAwACgAJAAQACgAAABAAAAAAAQQACAAIAAAABAAIAAAABAAAAAEAAAAUAAAAEAAUABAAAAAPAAQAAAAIABAAAAAYAAAAIAAAAAAAAAAcAAAACAAMAAQACwAIAAAAIAAAAAAAAAEAAAAAAQAAAHgAAAA=";

/// `batch` as a Parquet file whose only `ARROW:schema` entry is
/// [`CORRUPT_ARROW_SCHEMA`].
fn file_with_corrupt_arrow_schema(batch: &RecordBatch) -> bytes::Bytes {
    let properties = WriterProperties::builder()
        .set_key_value_metadata(Some(vec![KeyValue::new(
            "ARROW:schema".to_string(),
            CORRUPT_ARROW_SCHEMA.to_string(),
        )]))
        .build();
    let options = ArrowWriterOptions::new()
        .with_properties(properties)
        .with_skip_arrow_metadata(true);
    let mut file = Vec::new();
    let mut writer = ArrowWriter::try_new_with_options(&mut file, batch.schema(), options)
        .expect("open the writer");
    writer.write(batch).expect("write the batch");
    writer.close().expect("close the file");
    bytes::Bytes::from(file)
}

fn audit_events() -> Vec<AuditEvent> {
    let tenant = TenantId::new("acme");
    let at = UNIX_EPOCH + Duration::from_secs(1_791_676_800);
    vec![
        AuditEvent {
            tenant_id: tenant.clone(),
            timestamp: at,
            payload: AuditPayload::Template {
                template_id: 7,
                triggering_line_hash: [3; 16],
                triggering_line_sample: Some("user 42 logged in".to_string()),
                change: TemplateChange::Created {
                    new_template: "user <*> logged in".to_string(),
                },
            },
        },
        AuditEvent {
            tenant_id: tenant,
            timestamp: at + Duration::from_secs(1),
            payload: AuditPayload::Template {
                template_id: 9,
                triggering_line_hash: [4; 16],
                triggering_line_sample: None,
                change: TemplateChange::Widened {
                    old_version: 1,
                    new_version: 2,
                    old_template: "job alpha done".to_string(),
                    new_template: "job <*> done".to_string(),
                    positions_widened: vec![1],
                },
            },
        },
    ]
}

fn mined_record() -> MinedRecord {
    let ts_ns: u64 = 1_791_676_800_000_000_000;
    MinedRecord {
        tenant_id: TenantId::new("acme"),
        template_id: 7,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("lib.auth".to_string()),
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: ts_ns,
        observed_time_unix_nano: Some(ts_ns),
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ParamType::Num,
            value: "42".to_string(),
        }],
        separators: vec![String::new(), " ".to_string(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

#[test]
fn audit_reader_ignores_a_corrupt_arrow_schema() {
    // Arrange
    let events = audit_events();
    let batch = audit_events_to_batch(&events).expect("encode events");
    let file = file_with_corrupt_arrow_schema(&batch);

    // Act
    let read = AuditReader::open_bytes(file).and_then(AuditReader::read_all);

    // Assert
    assert_eq!(read.expect("read the events"), events);
}

#[test]
fn data_reader_ignores_a_corrupt_arrow_schema() {
    // Arrange
    let records = vec![mined_record()];
    let batch = mined_records_to_batch(&records).expect("encode records");
    let file = file_with_corrupt_arrow_schema(&batch);

    // Act
    let read = Reader::open_bytes(file).and_then(Reader::read_all);

    // Assert
    assert_eq!(read.expect("read the records"), records);
}

#[test]
fn decoded_max_ignores_a_corrupt_arrow_schema() {
    // Arrange
    let batch = audit_events_to_batch(&audit_events()).expect("encode events");
    let file = file_with_corrupt_arrow_schema(&batch);

    // Act
    let max = decoded_max(file, IdColumns::Audit);

    // Assert
    assert_eq!(max.expect("decode the ids"), Some(9));
}
