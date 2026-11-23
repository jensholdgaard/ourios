//! RFC0069.3's fixture: one Parquet file holding undecodable cells,
//! shared by the query test and the reader-event live-check binary.

use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Mutex};

use arrow_array::{Array, ArrayRef, BinaryArray, RecordBatch, StringArray};
use ourios_core::otlp::any_value::Value;
use ourios_core::otlp::{AnyValue, KeyValue};
use ourios_core::record::{BodyKind, MinedRecord};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, columns, mined_records_to_batch};
use parquet::arrow::ArrowWriter;

pub(crate) const TS0: u64 = 1_775_127_480_000_000_000;

pub(crate) fn kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(Value::StringValue(value.to_owned())),
        }),
        key_strindex: 0,
    }
}

/// A lossy string row whose body is retained and whose three attribute
/// lists each hold one key.
pub(crate) fn record(i: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("a"),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: None,
        scope_name: Some("lib".to_owned()),
        scope_version: None,
        scope_attributes: vec![kv("scope.k", "s")],
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: TS0 + i,
        observed_time_unix_nano: None,
        attributes: vec![kv("k", "x")],
        dropped_attributes_count: 0,
        resource_attributes: vec![kv("service.name", "cart")],
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: Vec::new(),
        separators: Vec::new(),
        body: Some(format!("line {i}")),
        confidence: 0.1,
        lossy_flag: true,
        ingest_position: None,
    }
}

/// A structured row; its body is replaced by bytes that do not decode.
pub(crate) fn structured(i: u64) -> MinedRecord {
    MinedRecord {
        body_kind: BodyKind::Structured,
        body: Some(r#"{"stringValue":"ok"}"#.to_owned()),
        lossy_flag: false,
        confidence: 1.0,
        ..record(i)
    }
}

/// `batch` with `column`'s cell at `row` replaced by `stored`.
pub(crate) fn corrupt(batch: &RecordBatch, column: &str, row: usize, stored: &[u8]) -> RecordBatch {
    let index = batch.schema().index_of(column).expect("column");
    let old = batch.column(index);
    let replaced: ArrayRef = if let Some(old) = old.as_any().downcast_ref::<BinaryArray>() {
        Arc::new(BinaryArray::from(
            (0..old.len())
                .map(|i| {
                    if i == row {
                        Some(stored)
                    } else {
                        old.is_valid(i).then(|| old.value(i))
                    }
                })
                .collect::<Vec<_>>(),
        ))
    } else {
        let old = old.as_any().downcast_ref::<StringArray>().expect("utf8");
        let stored = std::str::from_utf8(stored).expect("attribute cells are text");
        Arc::new(StringArray::from(
            (0..old.len())
                .map(|i| {
                    if i == row {
                        Some(stored)
                    } else {
                        old.is_valid(i).then(|| old.value(i))
                    }
                })
                .collect::<Vec<_>>(),
        ))
    };
    let mut arrays = batch.columns().to_vec();
    arrays[index] = replaced;
    RecordBatch::try_new(batch.schema(), arrays).expect("batch")
}

pub(crate) const BAD_ATTRIBUTES: &str = r#"[{"key":"k","value":{"stringValue":"x"}"#;
pub(crate) const BAD_RESOURCE: &str = "{";
pub(crate) const BAD_SCOPE: &str = "not json";
pub(crate) const NOT_UTF8: &[u8] = b"line \xff\xfe";
pub(crate) const BAD_STRUCTURED: &str = r#"{"stringValue":"#;

/// Row 1: all three attribute cells bad. Row 2: scope attributes bad and a
/// body that is not valid UTF-8. Row 3: a structured body that does not
/// decode.
pub(crate) fn write_fixture(bucket: &Path) {
    let records = [record(0), record(1), record(2), structured(3)];
    let batch = mined_records_to_batch(&records).expect("batch");
    let batch = corrupt(&batch, columns::ATTRIBUTES, 1, BAD_ATTRIBUTES.as_bytes());
    let batch = corrupt(
        &batch,
        columns::RESOURCE_ATTRIBUTES,
        1,
        BAD_RESOURCE.as_bytes(),
    );
    let batch = corrupt(&batch, columns::SCOPE_ATTRIBUTES, 1, BAD_SCOPE.as_bytes());
    let batch = corrupt(&batch, columns::SCOPE_ATTRIBUTES, 2, BAD_SCOPE.as_bytes());
    let batch = corrupt(&batch, columns::BODY, 2, NOT_UTF8);
    let batch = corrupt(&batch, columns::BODY, 3, BAD_STRUCTURED.as_bytes());
    write_batch(bucket, &records[0], &batch);
}

/// Write `batch` as `degraded.parquet` in the partition `record` derives to.
pub(crate) fn write_batch(bucket: &Path, record: &MinedRecord, batch: &RecordBatch) {
    let dir = PartitionKey::derive(record)
        .expect("partition")
        .data_path(bucket);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let file = File::create(dir.join("degraded.parquet")).expect("create");
    let mut writer = ArrowWriter::try_new(file, batch.schema(), None).expect("writer");
    writer.write(batch).expect("write");
    writer.close().expect("close");
}

/// One `ourios.reader.row.undecodable` event's (file, row, column).
pub(crate) type Seen = (String, String, String);

#[derive(Clone, Default)]
pub(crate) struct Events(pub(crate) Arc<Mutex<Vec<Seen>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Events {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Fields(Vec<(String, String)>);
        impl tracing::field::Visit for Fields {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.push((field.name().to_owned(), value.to_owned()));
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push((field.name().to_owned(), format!("{value:?}")));
            }
        }
        if event.metadata().name() != ourios_semconv::EVENT_OURIOS_READER_ROW_UNDECODABLE {
            return;
        }
        let mut fields = Fields(Vec::new());
        event.record(&mut fields);
        let get = |name: &str| {
            fields
                .0
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };
        self.0.lock().expect("events").push((
            get("file.path"),
            get(ourios_semconv::OURIOS_READER_ROW_INDEX),
            get(ourios_semconv::OURIOS_READER_COLUMN_NAME),
        ));
    }
}
