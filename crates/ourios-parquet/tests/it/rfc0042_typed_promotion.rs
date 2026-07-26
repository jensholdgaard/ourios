//! RFC 0042 §5 — typed numeric promotion, writer side.
//!
//! Scenarios RFC0042.1 (typed projection through the encode path) and
//! RFC0042.2 (projection totality over `AnyValue` variants × classes,
//! by property). The querier-side scenarios (`.3`–`.7`) land with the
//! query slice.

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int64Type};
use arrow_array::{Array, RecordBatch};
use arrow_schema::DataType;
use ourios_core::audit::ParamType;
use ourios_core::otlp::{AnyValue, KeyValue, any_value};
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{
    DEFAULT_ZSTD_LEVEL, PromotedAttributes, PromotedClass, PromotedKey, encode_records_to_parquet,
    encode_records_to_parquet_with_promoted,
};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::metadata::ParquetMetaData;
use parquet::file::reader::{FileReader, SerializedFileReader};
use proptest::prelude::*;

const TS0: u64 = 1_775_127_480_000_000_000;

fn kv(key: &str, value: Option<any_value::Value>) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: value.map(|v| AnyValue { value: Some(v) }),
        ..Default::default()
    }
}

fn kv_str(key: &str, value: &str) -> KeyValue {
    kv(key, Some(any_value::Value::StringValue(value.to_string())))
}

fn kv_int(key: &str, value: i64) -> KeyValue {
    kv(key, Some(any_value::Value::IntValue(value)))
}

fn kv_double(key: &str, value: f64) -> KeyValue {
    kv(key, Some(any_value::Value::DoubleValue(value)))
}

fn rec(attributes: Vec<KeyValue>) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("a"),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("com.anthropic.claude_code.events".to_string()),
        scope_version: Some("1.0.0".to_string()),
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: TS0,
        observed_time_unix_nano: Some(TS0 + 1_000),
        attributes,
        dropped_attributes_count: 0,
        resource_attributes: vec![kv_str("service.name", "claude-code")],
        trace_id: None,
        span_id: None,
        flags: 0x01,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ParamType::Num,
            value: "42".to_string(),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

fn typed_set() -> PromotedAttributes {
    PromotedAttributes::new_typed(
        [],
        [
            PromotedKey::string("model"),
            PromotedKey {
                key: "cost_usd".into(),
                class: PromotedClass::F64,
            },
            PromotedKey {
                key: "input_tokens".into(),
                class: PromotedClass::I64,
            },
        ],
    )
}

fn read_all(bytes: &[u8]) -> (RecordBatch, ParquetMetaData) {
    let reader = SerializedFileReader::new(bytes::Bytes::copy_from_slice(bytes)).expect("footer");
    let metadata = reader.metadata().clone();
    let batches: Vec<RecordBatch> =
        ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::copy_from_slice(bytes))
            .expect("builder")
            .build()
            .expect("reader")
            .collect::<Result<_, _>>()
            .expect("batches");
    assert_eq!(batches.len(), 1, "fixture fits one batch");
    (batches.into_iter().next().expect("one batch"), metadata)
}

fn f64_cells(batch: &RecordBatch, column: &str) -> Vec<Option<f64>> {
    let idx = batch.schema().index_of(column).expect("promoted column");
    let schema = batch.schema();
    let field = schema.field(idx);
    assert_eq!(*field.data_type(), DataType::Float64, "{column} is Float64");
    assert!(field.is_nullable(), "{column} is OPTIONAL");
    let arr = batch.column(idx).as_primitive::<Float64Type>();
    (0..arr.len())
        .map(|i| (!arr.is_null(i)).then(|| arr.value(i)))
        .collect()
}

fn i64_cells(batch: &RecordBatch, column: &str) -> Vec<Option<i64>> {
    let idx = batch.schema().index_of(column).expect("promoted column");
    let schema = batch.schema();
    let field = schema.field(idx);
    assert_eq!(*field.data_type(), DataType::Int64, "{column} is Int64");
    assert!(field.is_nullable(), "{column} is OPTIONAL");
    let arr = batch.column(idx).as_primitive::<Int64Type>();
    (0..arr.len())
        .map(|i| (!arr.is_null(i)).then(|| arr.value(i)))
        .collect()
}

/// Scenario RFC0042.1 — typed keys project to `OPTIONAL`
/// `Float64`/`Int64` columns holding the class projection, and the
/// JSON attribute columns are byte-identical to an unpromoted run.
/// See `docs/rfcs/0042-typed-numeric-promotion.md` §5.
#[test]
fn rfc0042_1_typed_projection() {
    let records = [
        // The captured Claude Code shape: double cost, int tokens.
        rec(vec![
            kv_str("model", "claude-opus-4-8"),
            kv_double("cost_usd", 0.188_796),
            kv_int("input_tokens", 2),
        ]),
        // Int cost widens into the f64 column; a double under the i64
        // class does NOT narrow.
        rec(vec![kv_int("cost_usd", 0), kv_double("input_tokens", 2.0)]),
        // Strings never parse into numeric columns.
        rec(vec![
            kv_str("cost_usd", "0.5"),
            kv_str("input_tokens", "42"),
        ]),
        // Keys absent entirely.
        rec(Vec::new()),
    ];
    let promoted = typed_set();
    let bytes = encode_records_to_parquet_with_promoted(&records, DEFAULT_ZSTD_LEVEL, &promoted)
        .expect("encode");
    let (batch, metadata) = read_all(&bytes);

    assert_eq!(
        f64_cells(&batch, "attr.cost_usd"),
        [Some(0.188_796), Some(0.0), None, None],
    );
    assert_eq!(
        i64_cells(&batch, "attr.input_tokens"),
        [Some(2), None, None, None],
    );

    // The JSON source-of-truth column is untouched by promotion
    // (RFC 0022 §3.1 "projection, not truth" — re-asserted for the
    // typed classes).
    let plain = encode_records_to_parquet(&records, DEFAULT_ZSTD_LEVEL).expect("encode plain");
    let (plain_batch, _) = read_all(&plain);
    let col = |b: &RecordBatch, name: &str| {
        let idx = b.schema().index_of(name).expect(name);
        b.column(idx).clone()
    };
    assert_eq!(
        col(&batch, "attributes").as_ref(),
        col(&plain_batch, "attributes").as_ref(),
        "attributes JSON identical with and without typed promotion"
    );

    // RFC 0042 §3.5 encodings: bloom on the i64 column, none on f64.
    let rg = metadata.row_group(0);
    let chunk = |name: &str| {
        (0..rg.num_columns())
            .map(|i| rg.column(i))
            .find(|c| c.column_path().string() == name)
            .expect("column chunk")
    };
    assert!(
        chunk("attr.input_tokens").bloom_filter_offset().is_some(),
        "i64 column carries a bloom filter"
    );
    assert!(
        chunk("attr.cost_usd").bloom_filter_offset().is_none(),
        "f64 column carries no bloom filter (equality is typed-arm-only)"
    );
    assert!(
        chunk("attr.model").bloom_filter_offset().is_some(),
        "string columns keep the RFC 0022 bloom"
    );
}

/// Any-variant generator covering every `AnyValue` shape a promoted
/// key can meet, including the absent-value forms.
fn arb_any_value() -> impl Strategy<Value = Option<any_value::Value>> {
    prop_oneof![
        Just(None),
        any::<String>().prop_map(|s| Some(any_value::Value::StringValue(s))),
        any::<i64>().prop_map(|v| Some(any_value::Value::IntValue(v))),
        any::<f64>().prop_map(|v| Some(any_value::Value::DoubleValue(v))),
        any::<bool>().prop_map(|v| Some(any_value::Value::BoolValue(v))),
        proptest::collection::vec(any::<u8>(), 0..8)
            .prop_map(|b| Some(any_value::Value::BytesValue(b))),
    ]
}

proptest! {
    /// Scenario RFC0042.2 — projection totality: for every `AnyValue`
    /// variant under every class, the projected cell is exactly the
    /// RFC 0042 §3.1 table's value or `NULL` — never a panic, never a
    /// parse. In particular: ints widen into `f64`, doubles do not
    /// narrow into `i64`, and strings project `NULL` under both
    /// numeric classes.
    #[test]
    fn rfc0042_2_projection_totality(value in arb_any_value()) {
        let attrs = [kv("k", value.clone())];
        let expect_i64 = match &value {
            Some(any_value::Value::IntValue(v)) => Some(*v),
            _ => None,
        };
        #[allow(clippy::cast_precision_loss)] // §3.1: widening is the contract under test
        let expect_f64 = match &value {
            Some(any_value::Value::DoubleValue(v)) => Some(*v),
            Some(any_value::Value::IntValue(v)) => Some(*v as f64),
            _ => None,
        };
        let expect_str = match &value {
            Some(any_value::Value::StringValue(s)) => Some(s.clone()),
            _ => None,
        };

        prop_assert_eq!(ourios_parquet::promoted::i64_value(&attrs[0]), expect_i64);
        prop_assert_eq!(ourios_parquet::promoted::f64_value(&attrs[0]), expect_f64);
        prop_assert_eq!(
            ourios_parquet::promoted::string_value(&attrs[0]).map(str::to_string),
            expect_str
        );
    }
}
