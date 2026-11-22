//! RFC0069.3, the reader's leg: a stored cell that does not decode
//! degrades on its own when the query path asks for it. Each bad attribute
//! cell becomes the reserved attribute holding its stored JSON, a body
//! that is not valid UTF-8 is flagged, and every other cell decodes as
//! usual. The strict decode is unchanged.

use std::sync::Arc;

use arrow_array::{Array, ArrayRef, BinaryArray, RecordBatch, StringArray};
use ourios_core::otlp::any_value::Value;
use ourios_core::otlp::{AnyValue, KeyValue};
use ourios_core::record::{BodyKind, MinedRecord};
use ourios_core::tenant::TenantId;
use ourios_parquet::{
    ReaderError, ShapeValidation, UndecodableCell, batch_to_degraded_records,
    batch_to_mined_records, columns, mined_records_to_batch,
};

const TS0: u64 = 1_775_127_480_000_000_000;

fn kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(Value::StringValue(value.to_owned())),
        }),
        key_strindex: 0,
    }
}

/// A lossy string row: its body is retained, and every attribute list
/// holds one key so each cell is stored as canonical JSON.
fn record(i: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("tenant-x"),
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

/// `batch` with `column`'s cell at `row` replaced by `stored`, cast back
/// to the column's own type.
fn corrupt(batch: &RecordBatch, column: &str, row: usize, stored: &[u8]) -> RecordBatch {
    let index = batch.schema().index_of(column).expect("column");
    let field_type = batch.schema().field(index).data_type().clone();
    let old =
        arrow_cast::cast(batch.column(index), &arrow_schema::DataType::Binary).expect("as binary");
    let old = old.as_any().downcast_ref::<BinaryArray>().expect("binary");
    let cells: Vec<Option<&[u8]>> = (0..old.len())
        .map(|i| {
            if i == row {
                Some(stored)
            } else {
                old.is_valid(i).then(|| old.value(i))
            }
        })
        .collect();
    let replaced: ArrayRef = if field_type == arrow_schema::DataType::Binary {
        Arc::new(BinaryArray::from(cells))
    } else {
        let text: Vec<Option<String>> = cells
            .into_iter()
            .map(|cell| cell.map(|bytes| String::from_utf8_lossy(bytes).into_owned()))
            .collect();
        arrow_cast::cast(&StringArray::from(text), &field_type).expect("cast back")
    };
    let mut arrays = batch.columns().to_vec();
    arrays[index] = replaced;
    RecordBatch::try_new(batch.schema(), arrays).expect("batch")
}

fn reserved(stored: &str) -> Vec<KeyValue> {
    vec![kv(ourios_semconv::OURIOS_ATTRIBUTE_UNDECODABLE, stored)]
}

const BAD_ATTRIBUTES: &str = r#"[{"key":"k","value":{"stringValue":"x"}"#;
const BAD_RESOURCE: &str = "{";
const BAD_SCOPE: &str = "not json";
const NOT_UTF8: &[u8] = b"line \xff\xfe";

/// Row 1 has all three attribute cells undecodable; row 2 has its scope
/// attributes undecodable and a body that is not valid UTF-8.
fn degraded_batch() -> RecordBatch {
    let batch = mined_records_to_batch(&[record(0), record(1), record(2)]).expect("batch");
    let batch = corrupt(&batch, columns::ATTRIBUTES, 1, BAD_ATTRIBUTES.as_bytes());
    let batch = corrupt(
        &batch,
        columns::RESOURCE_ATTRIBUTES,
        1,
        BAD_RESOURCE.as_bytes(),
    );
    let batch = corrupt(&batch, columns::SCOPE_ATTRIBUTES, 1, BAD_SCOPE.as_bytes());
    let batch = corrupt(&batch, columns::SCOPE_ATTRIBUTES, 2, BAD_SCOPE.as_bytes());
    corrupt(&batch, columns::BODY, 2, NOT_UTF8)
}

/// Scenario RFC0069.3 (reader): every row comes back, each bad cell
/// alone degraded and named, every valid cell of those rows decoded.
/// See `docs/rfcs/0069-nested-value-read-depth.md` §5.
#[test]
fn rfc0069_3_an_undecodable_cell_degrades_alone() {
    let decoded =
        batch_to_degraded_records(&degraded_batch(), 0, ShapeValidation::Skip).expect("decode");
    let [healthy, bad, mixed] = decoded.records.as_slice() else {
        panic!("three rows, got {}", decoded.records.len());
    };

    assert_eq!(*healthy, record(0), "a healthy row decodes unchanged");

    assert_eq!(bad.attributes, reserved(BAD_ATTRIBUTES));
    assert_eq!(bad.resource_attributes, reserved(BAD_RESOURCE));
    assert_eq!(bad.scope_attributes, reserved(BAD_SCOPE));
    assert_eq!(bad.body, record(1).body, "the bad row's valid body is kept");

    assert_eq!(mixed.scope_attributes, reserved(BAD_SCOPE));
    assert_eq!(mixed.attributes, record(2).attributes, "valid cells decode");
    assert_eq!(mixed.resource_attributes, record(2).resource_attributes);
    assert_eq!(
        mixed.body.as_deref(),
        Some(String::from_utf8_lossy(NOT_UTF8).as_ref())
    );

    assert_eq!(
        decoded.undecodable,
        vec![
            UndecodableCell {
                row: 1,
                column: columns::ATTRIBUTES
            },
            UndecodableCell {
                row: 1,
                column: columns::RESOURCE_ATTRIBUTES
            },
            UndecodableCell {
                row: 1,
                column: columns::SCOPE_ATTRIBUTES
            },
            UndecodableCell {
                row: 2,
                column: columns::SCOPE_ATTRIBUTES
            },
            UndecodableCell {
                row: 2,
                column: columns::BODY
            },
        ]
    );
}

/// Outside the query path nothing degrades: the strict decode still
/// refuses the batch, so compaction never writes the reserved attribute.
#[test]
fn the_strict_decode_still_refuses_an_undecodable_cell() {
    match batch_to_mined_records(&degraded_batch(), 0, ShapeValidation::Skip) {
        Err(ReaderError::AttributeDecode {
            column, row_index, ..
        }) => {
            assert_eq!((column, row_index), (columns::ATTRIBUTES, 1));
        }
        other => panic!("expected AttributeDecode, got {other:?}"),
    }
}
