//! RFC 0022 §5 — writer-side promoted attribute columns.
//!
//! Scenarios RFC0022.1/.2 (projection semantics + encodings). The
//! querier-side scenarios (`.3`–`.7`) live in
//! `crates/ourios-querier/tests/rfc0022_attr_columns.rs`.

use arrow_array::cast::AsArray;
use arrow_array::{Array, RecordBatch};
use arrow_schema::DataType;
use ourios_core::audit::ParamType;
use ourios_core::otlp::{AnyValue, KeyValue, any_value, canonical};
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{
    DEFAULT_ZSTD_LEVEL, PromotedAttributes, columns, encode_records_to_parquet,
    encode_records_to_parquet_with_promoted,
};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Encoding, LogicalType};
use parquet::file::metadata::ParquetMetaData;
use parquet::file::reader::{FileReader, SerializedFileReader};

const TS0: u64 = 1_775_127_480_000_000_000;

fn kv_str(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_string())),
        }),
        ..Default::default()
    }
}

fn kv_int(key: &str, value: i64) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::IntValue(value)),
        }),
        ..Default::default()
    }
}

fn rec(resource_attributes: Vec<KeyValue>, attributes: Vec<KeyValue>) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("a"),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("lib.cart".to_string()),
        scope_version: Some("1.0.0".to_string()),
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: TS0,
        observed_time_unix_nano: Some(TS0 + 1_000),
        attributes,
        dropped_attributes_count: 0,
        resource_attributes,
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

fn promoted_values(batch: &RecordBatch, column: &str) -> Vec<Option<String>> {
    let idx = batch.schema().index_of(column).expect("promoted column");
    let field = batch.schema().field(idx).clone();
    assert_eq!(*field.data_type(), DataType::Utf8, "{column} is Utf8");
    assert!(field.is_nullable(), "{column} is OPTIONAL");
    let arr = batch.column(idx).as_string::<i32>();
    (0..arr.len())
        .map(|i| (!arr.is_null(i)).then(|| arr.value(i).to_string()))
        .collect()
}

/// Scenario RFC0022.1 — `service.name` is always projected.
/// See `docs/rfcs/0022-queryable-attribute-columns.md` §5.
#[test]
fn rfc0022_1_service_name_always_projected() {
    let records = [
        rec(
            vec![kv_str("service.name", "api"), kv_str("other", "x")],
            Vec::new(),
        ),
        rec(vec![kv_int("service.name", 7)], Vec::new()), // non-string → NULL
        rec(Vec::new(), Vec::new()),                      // absent → NULL
    ];
    // The plain default writer path — no explicit promoted set anywhere.
    let bytes = encode_records_to_parquet(&records, DEFAULT_ZSTD_LEVEL).expect("encode");
    let (batch, metadata) = read_all(&bytes);

    assert_eq!(
        promoted_values(&batch, "resource.service.name"),
        [Some("api".to_string()), None, None],
    );

    // The promoted column is one Parquet leaf with the STRING logical
    // type (its name contains literal dots — not a nested group).
    let schema = metadata.file_metadata().schema_descr();
    let leaf = schema
        .columns()
        .iter()
        .find(|c| c.name() == "resource.service.name")
        .expect("promoted leaf in the Parquet schema");
    assert_eq!(leaf.path().parts().len(), 1, "single leaf, not nested");
    assert_eq!(
        leaf.logical_type_ref().cloned(),
        Some(LogicalType::String),
        "STRING logical type over BYTE_ARRAY"
    );

    // §3.1 "projection, not truth": the JSON column is byte-identical
    // to what the canonical encoder produces for the input — the same
    // bytes a pre-amendment writer emitted.
    let json_idx = batch
        .schema()
        .index_of(columns::RESOURCE_ATTRIBUTES)
        .expect("json column");
    let json = batch.column(json_idx).as_string::<i32>();
    for (i, r) in records.iter().enumerate() {
        let expected = if r.resource_attributes.is_empty() {
            b"[]".to_vec()
        } else {
            canonical::encode_attributes(&r.resource_attributes).expect("canonical")
        };
        assert_eq!(json.value(i).as_bytes(), expected, "row {i} JSON unchanged");
    }
}

/// Scenario RFC0022.2 — configured keys project the same way.
/// See `docs/rfcs/0022-queryable-attribute-columns.md` §5.
#[test]
fn rfc0022_2_configured_keys_project() {
    let promoted = PromotedAttributes::new(
        ["k8s.namespace.name".to_string()],
        ["http.route".to_string()],
    );
    let records = [
        rec(
            vec![
                kv_str("service.name", "api"),
                kv_str("k8s.namespace.name", "prod"),
            ],
            vec![
                kv_str("http.route", "/cart/{id}"),
                kv_str("http.request.method", "GET"), // present but NOT configured
            ],
        ),
        rec(
            vec![kv_int("k8s.namespace.name", 3)], // non-string → NULL
            Vec::new(),                            // http.route absent → NULL
        ),
    ];
    let bytes = encode_records_to_parquet_with_promoted(&records, DEFAULT_ZSTD_LEVEL, &promoted)
        .expect("encode");
    let (batch, metadata) = read_all(&bytes);

    assert_eq!(
        promoted_values(&batch, "resource.k8s.namespace.name"),
        [Some("prod".to_string()), None],
    );
    assert_eq!(
        promoted_values(&batch, "attr.http.route"),
        [Some("/cart/{id}".to_string()), None],
    );
    // The implicit service.name rides along with any configured set.
    assert_eq!(
        promoted_values(&batch, "resource.service.name"),
        [Some("api".to_string()), None],
    );
    // A key not in the configured set produces no column.
    assert!(
        batch.schema().index_of("attr.http.request.method").is_err(),
        "unconfigured keys must not grow columns"
    );

    // Encodings row of the RFC 0022 §3.1 table: dictionary + bloom on
    // every promoted column (page-index/statistics ride the writer's
    // global defaults, asserted via the chunk statistics).
    let rg = metadata.row_group(0);
    for name in [
        "resource.service.name",
        "resource.k8s.namespace.name",
        "attr.http.route",
    ] {
        let col = (0..rg.num_columns())
            .map(|i| rg.column(i))
            .find(|c| c.column_path().string() == name)
            .expect("promoted column chunk");
        assert!(
            col.bloom_filter_offset().is_some(),
            "{name}: bloom filter written"
        );
        let encodings: Vec<Encoding> = col.encodings().collect();
        assert!(
            encodings.contains(&Encoding::RLE_DICTIONARY)
                || encodings.contains(&Encoding::PLAIN_DICTIONARY),
            "{name}: dictionary-encoded, got {encodings:?}"
        );
        assert!(
            col.statistics().is_some(),
            "{name}: chunk statistics present"
        );
    }
}

/// §3.1 first-occurrence semantics hold through the batch path: the
/// first attribute carrying a promoted key decides the cell, even when
/// a later duplicate would project differently.
#[test]
fn promoted_projection_is_first_occurrence_per_record() {
    let promoted = PromotedAttributes::new(Vec::new(), ["http.route".to_string()]);
    let records = [
        rec(
            Vec::new(),
            vec![
                kv_str("http.route", "/first"),
                kv_str("http.route", "/shadowed"),
            ],
        ),
        rec(
            Vec::new(),
            // First occurrence is non-string → NULL, despite the later string.
            vec![kv_int("http.route", 1), kv_str("http.route", "/late")],
        ),
    ];
    let bytes = encode_records_to_parquet_with_promoted(&records, DEFAULT_ZSTD_LEVEL, &promoted)
        .expect("encode");
    let (batch, _) = read_all(&bytes);
    assert_eq!(
        promoted_values(&batch, "attr.http.route"),
        [Some("/first".to_string()), None],
    );
}
