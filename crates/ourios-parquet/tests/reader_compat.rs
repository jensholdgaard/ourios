//! Schema-evolution scenarios per RFC 0005 §3.9.
//!
//! - **RFC0005.2** — Missing OPTIONAL column surfaces as `None`
//!   (old-file reader path).
//! - **RFC0005.3** — Unknown column silently ignored (forward
//!   compatibility).
//! - **RFC0005.4** — Missing baseline REQUIRED column is a hard
//!   error.
//! - **RFC0005.9** — Unknown `ParamType` ordinal surfaces as
//!   `ParamType::Unknown(N)`.
//!
//! Each test hand-rolls a Parquet file with a deliberately
//! shaped schema, then reads via [`Reader::open_file`] (no
//! partition validation — that's RFC0005.11's concern).

use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, FixedSizeBinaryBuilder, Float32Builder, GenericListBuilder,
    Int32Builder, StringBuilder, StructBuilder, TimestampNanosecondBuilder, UInt8Builder,
    UInt32Builder, UInt64Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, Reader, ReaderError, Writer};
use parquet::arrow::ArrowWriter;
use tempfile::TempDir;

/// Build a single-row `RecordBatch` against an `ArrowSchema`
/// the test passes in. The schema may omit OPTIONAL columns or
/// add extras; this builder fills sensible values for whatever
/// columns the schema declares (mirroring the production schema
/// shape for the matching names).
//
// `clippy::too_many_lines` — long flat match is the readable
// shape here; splitting per-column constructors out hides
// rather than helps.
//
// `clippy::match_same_arms` — `dropped_attributes_count` and
// `flags` happen to share the "UInt32 zero" shape, but they're
// semantically distinct columns and merging would hurt
// readability when one of them later changes default.
#[allow(clippy::too_many_lines, clippy::match_same_arms)]
fn build_one_row_against(schema: Arc<ArrowSchema>) -> RecordBatch {
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len());
    for field in schema.fields() {
        let arr: ArrayRef = match field.name().as_str() {
            "tenant_id" => {
                let mut b = StringBuilder::new();
                b.append_value("tenant-a");
                Arc::new(b.finish())
            }
            "template_id" => {
                let mut b = UInt64Builder::new();
                b.append_value(42);
                Arc::new(b.finish())
            }
            "template_version" => {
                let mut b = UInt32Builder::new();
                b.append_value(1);
                Arc::new(b.finish())
            }
            "time_unix_nano" => {
                let mut b = TimestampNanosecondBuilder::new().with_timezone("UTC");
                b.append_value(1_775_127_480_000_000_000);
                Arc::new(b.finish())
            }
            "observed_time_unix_nano" => {
                let mut b = TimestampNanosecondBuilder::new().with_timezone("UTC");
                b.append_null();
                Arc::new(b.finish())
            }
            "severity_number" => {
                let mut b = UInt8Builder::new();
                b.append_value(9);
                Arc::new(b.finish())
            }
            "severity_text" | "scope_name" | "scope_version" | "event_name" => {
                let mut b = StringBuilder::new();
                b.append_null();
                Arc::new(b.finish())
            }
            "attributes" | "resource_attributes" => {
                let mut b = StringBuilder::new();
                b.append_value("[]");
                Arc::new(b.finish())
            }
            "dropped_attributes_count" => {
                let mut b = UInt32Builder::new();
                b.append_value(0);
                Arc::new(b.finish())
            }
            "trace_id" => {
                let mut b = FixedSizeBinaryBuilder::new(16);
                b.append_null();
                Arc::new(b.finish())
            }
            "span_id" => {
                let mut b = FixedSizeBinaryBuilder::new(8);
                b.append_null();
                Arc::new(b.finish())
            }
            "flags" => {
                let mut b = UInt32Builder::new();
                b.append_value(0);
                Arc::new(b.finish())
            }
            "body_kind" => {
                let mut b = UInt8Builder::new();
                b.append_value(0); // String
                Arc::new(b.finish())
            }
            "body" => {
                let mut b = BinaryBuilder::new();
                b.append_null();
                Arc::new(b.finish())
            }
            "params" => {
                let element_struct = StructBuilder::new(
                    vec![
                        Field::new("type_tag", DataType::Int32, false),
                        Field::new("value", DataType::Binary, true),
                    ],
                    vec![
                        Box::new(Int32Builder::new()),
                        Box::new(BinaryBuilder::new()),
                    ],
                );
                let mut b = GenericListBuilder::<i32, StructBuilder>::new(element_struct)
                    .with_field(Field::new(
                        "element",
                        DataType::Struct(
                            vec![
                                Field::new("type_tag", DataType::Int32, false),
                                Field::new("value", DataType::Binary, true),
                            ]
                            .into(),
                        ),
                        false,
                    ));
                b.append(true); // empty list
                Arc::new(b.finish())
            }
            "separators" => {
                let mut b = GenericListBuilder::<i32, BinaryBuilder>::new(BinaryBuilder::new())
                    .with_field(Field::new("element", DataType::Binary, false));
                // One element to satisfy the lower-bound check
                // when read back, even though we're writing a
                // single-row record.
                b.values().append_value("");
                b.append(true);
                Arc::new(b.finish())
            }
            "confidence" => {
                let mut b = Float32Builder::new();
                b.append_value(0.0);
                Arc::new(b.finish())
            }
            "lossy_flag" => {
                let mut b = BooleanBuilder::new();
                // Clean-attach shape: body is None (no
                // retention needed), so the writer-side
                // invariant (separators.len() >= params.len() + 1)
                // is the relevant check. Our params=0 +
                // separators=[""] satisfies it (1 >= 1).
                b.append_value(false);
                Arc::new(b.finish())
            }
            "future_column" => {
                // Extra column the current reader doesn't know
                // about — RFC0005.3 says it must be ignored.
                let mut b = StringBuilder::new();
                b.append_value("some-future-thing");
                Arc::new(b.finish())
            }
            other => panic!("unhandled column in builder: {other}"),
        };
        arrays.push(arr);
    }
    RecordBatch::try_new(schema, arrays).expect("build batch")
}

fn write_batch(schema: Arc<ArrowSchema>, batch: &RecordBatch) -> tempfile::NamedTempFile {
    let file = tempfile::Builder::new()
        .suffix(".parquet")
        .tempfile()
        .expect("tempfile");
    let mut writer = ArrowWriter::try_new(file.reopen().unwrap(), schema, None).expect("writer");
    writer.write(batch).expect("write");
    writer.close().expect("close");
    file
}

/// Scenario RFC0005.2 — Missing OPTIONAL column → `None`.
#[test]
fn rfc0005_2_missing_optional_column_surfaces_as_none() {
    // Schema omits `severity_text` (OPTIONAL in §3.2).
    let fields: Vec<Field> = ourios_parquet::data_schema()
        .fields()
        .iter()
        .filter(|f| f.name() != "severity_text")
        .map(|f| (**f).clone())
        .collect();
    let schema = Arc::new(ArrowSchema::new(fields));
    let batch = build_one_row_against(schema.clone());
    let f = write_batch(schema, &batch);

    let reader = Reader::open_file(f.path()).expect("open_file");
    let records = reader.read_all().expect("read_all");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].severity_text, None);
}

/// Scenario RFC0005.3 — Unknown column added → silently
/// ignored on read.
#[test]
fn rfc0005_3_unknown_column_is_silently_ignored() {
    // Schema adds a `future_column` not in `data_schema()`.
    let mut fields: Vec<Field> = ourios_parquet::data_schema()
        .fields()
        .iter()
        .map(|f| (**f).clone())
        .collect();
    fields.push(Field::new("future_column", DataType::Utf8, true));
    let schema = Arc::new(ArrowSchema::new(fields));
    let batch = build_one_row_against(schema.clone());
    let f = write_batch(schema, &batch);

    let reader = Reader::open_file(f.path()).expect("open_file");
    let records = reader.read_all().expect("read_all");
    assert_eq!(records.len(), 1);
    // Reader doesn't surface the unknown column; the record
    // shape is unaffected.
    assert_eq!(records[0].tenant_id.as_str(), "tenant-a");
}

/// Scenario RFC0005.4 — Missing baseline REQUIRED column → hard
/// error naming the column.
#[test]
fn rfc0005_4_missing_required_column_returns_hard_error() {
    // Schema omits `template_id` (REQUIRED in §3.2).
    let fields: Vec<Field> = ourios_parquet::data_schema()
        .fields()
        .iter()
        .filter(|f| f.name() != "template_id")
        .map(|f| (**f).clone())
        .collect();
    let schema = Arc::new(ArrowSchema::new(fields));
    let batch = build_one_row_against(schema.clone());
    let f = write_batch(schema, &batch);

    match Reader::open_file(f.path()) {
        Err(ReaderError::MissingRequiredColumn { name }) => assert_eq!(name, "template_id"),
        Err(other) => panic!("expected MissingRequiredColumn, got {other:?}"),
        Ok(_) => panic!("expected MissingRequiredColumn, got Ok"),
    }
}

/// Reader-side shape-invariant: a foreign-built file with
/// `lossy_flag = true` and `body = NULL` is corruption. The
/// reader mirrors the writer's `MissingBodyForLossyString`
/// rejection — RFC 0001 §6.6 reconstruction has nothing to
/// fall back to.
#[test]
fn reader_rejects_lossy_string_without_body() {
    let schema = ourios_parquet::data_schema();
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len());
    for field in schema.fields() {
        // Build the row using the helper, then override
        // `lossy_flag = true` + `body = NULL` after the fact
        // would be awkward; build inline instead.
        let arr: ArrayRef = match field.name().as_str() {
            "lossy_flag" => {
                let mut b = BooleanBuilder::new();
                b.append_value(true);
                Arc::new(b.finish())
            }
            "body" => {
                let mut b = BinaryBuilder::new();
                b.append_null();
                Arc::new(b.finish())
            }
            other => single_field_array(other),
        };
        arrays.push(arr);
    }
    let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
    let f = write_batch(schema, &batch);

    match Reader::open_file(f.path()).and_then(Reader::read_all) {
        Err(ReaderError::Conversion { column, .. }) => assert_eq!(column, "body"),
        Err(other) => panic!("expected Conversion on body, got {other:?}"),
        Ok(_) => panic!("expected Conversion error"),
    }
}

/// Reader-side shape-invariant: clean-attach String row with
/// `separators` shorter than `params.len() + 1` is rejected,
/// mirroring the writer's `InvalidSeparatorsForString`.
#[test]
fn reader_rejects_clean_string_with_too_few_separators() {
    let schema = ourios_parquet::data_schema();
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len());
    for field in schema.fields() {
        let arr: ArrayRef = match field.name().as_str() {
            "lossy_flag" => {
                let mut b = BooleanBuilder::new();
                b.append_value(false);
                Arc::new(b.finish())
            }
            "params" => {
                // Two params on disk; with the empty separators
                // below, separators.len() = 0 < params.len() + 1 = 3.
                let element_struct = StructBuilder::new(
                    vec![
                        Field::new("type_tag", DataType::Int32, false),
                        Field::new("value", DataType::Binary, true),
                    ],
                    vec![
                        Box::new(Int32Builder::new()),
                        Box::new(BinaryBuilder::new()),
                    ],
                );
                let mut b = GenericListBuilder::<i32, StructBuilder>::new(element_struct)
                    .with_field(Field::new(
                        "element",
                        DataType::Struct(
                            vec![
                                Field::new("type_tag", DataType::Int32, false),
                                Field::new("value", DataType::Binary, true),
                            ]
                            .into(),
                        ),
                        false,
                    ));
                for _ in 0..2 {
                    b.values()
                        .field_builder::<Int32Builder>(0)
                        .unwrap()
                        .append_value(2);
                    b.values()
                        .field_builder::<BinaryBuilder>(1)
                        .unwrap()
                        .append_value(b"x");
                    b.values().append(true);
                }
                b.append(true);
                Arc::new(b.finish())
            }
            "separators" => {
                // Empty separators on a clean-attach String row.
                let mut b = GenericListBuilder::<i32, BinaryBuilder>::new(BinaryBuilder::new())
                    .with_field(Field::new("element", DataType::Binary, false));
                b.append(true);
                Arc::new(b.finish())
            }
            other => single_field_array(other),
        };
        arrays.push(arr);
    }
    let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
    let f = write_batch(schema, &batch);

    match Reader::open_file(f.path()).and_then(Reader::read_all) {
        Err(ReaderError::Conversion { column, .. }) => assert_eq!(column, "separators"),
        Err(other) => panic!("expected Conversion on separators, got {other:?}"),
        Ok(_) => panic!("expected Conversion error"),
    }
}

/// Helper used by the two shape-invariant tests above to
/// populate the "everything else" columns. Defers to
/// `build_one_row_against` for the columns it overrides.
fn single_field_array(field_name: &str) -> ArrayRef {
    // Build a one-row schema containing only the requested
    // field, run the standard builder over it, return the
    // resulting array.
    let schema = ourios_parquet::data_schema();
    let target = schema
        .fields()
        .iter()
        .find(|f| f.name() == field_name)
        .unwrap_or_else(|| panic!("field {field_name} not in data_schema"));
    let single = Arc::new(ArrowSchema::new(vec![(**target).clone()]));
    let one_row = build_one_row_against(single);
    one_row.column(0).clone()
}

/// Scenario RFC0005.9 — Unknown `ParamType` ordinal → reader
/// returns `ParamType::Unknown(N)` rather than erroring.
/// Round-trip path: the writer accepts `ParamType::Unknown(99)`
/// (preserves the ordinal on disk) and the reader recovers it.
#[test]
fn rfc0005_9_unknown_param_type_round_trips_as_unknown_variant() {
    let bucket = TempDir::new().unwrap();

    let rec = MinedRecord {
        tenant_id: TenantId::new("tenant-x"),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        time_unix_nano: 1_775_127_480_000_000_000,
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ParamType::Unknown(99),
            value: "unknown-variant-payload".to_string(),
        }],
        // params.len() + 1 = 2 ≤ separators.len() = 2.
        separators: vec![String::new(), String::new()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    };
    let partition = PartitionKey::derive(&rec).unwrap();

    let mut writer = Writer::open(bucket.path(), partition.clone()).unwrap();
    writer.append_records(&[rec]).unwrap();
    let written = writer.close().unwrap();

    let reader = Reader::open_partition(&written.path, partition).unwrap();
    let records = reader
        .read_all()
        .expect("unknown variant must read cleanly");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].params.len(), 1);
    match records[0].params[0].type_tag {
        ParamType::Unknown(99) => {}
        other => panic!("expected Unknown(99), got {other:?}"),
    }
    assert_eq!(records[0].params[0].value, "unknown-variant-payload");
}
