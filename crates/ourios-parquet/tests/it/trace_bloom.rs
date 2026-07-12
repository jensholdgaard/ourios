//! RFC 0005 §3.6 — bloom filters on the trace-context id columns.
//!
//! Random 16/8-byte ids defeat min/max statistics entirely, so an
//! exact-id lookup (the RFC 0031 L3 class) degenerates to a
//! whole-column scan without blooms — measured at 72.4 MB for a 9-row
//! trace on the 4.9M-record otel-demo-v8 corpus (comparative run #12).
//! This pins the writer emitting blooms for `trace_id` and `span_id`,
//! alongside the pre-existing `template_id` one.

use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{DEFAULT_ZSTD_LEVEL, encode_records_to_parquet};
use parquet::file::reader::{FileReader, SerializedFileReader};

const TS0: u64 = 1_775_127_480_000_000_000;

fn rec(trace_id: Option<[u8; 16]>, span_id: Option<[u8; 8]>) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("a"),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: None,
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: TS0,
        observed_time_unix_nano: Some(TS0 + 1_000),
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id,
        span_id,
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

#[test]
fn trace_context_columns_carry_bloom_filters() {
    let records = [
        rec(Some([0xAB; 16]), Some([0xCD; 8])),
        rec(Some([0x11; 16]), Some([0x22; 8])),
        rec(None, None),
    ];
    let bytes = encode_records_to_parquet(&records, DEFAULT_ZSTD_LEVEL).expect("encode");
    let reader = SerializedFileReader::new(bytes::Bytes::from(bytes)).expect("footer");
    let rg = reader.metadata().row_group(0);

    for name in ["trace_id", "span_id", "template_id"] {
        let col = (0..rg.num_columns())
            .map(|i| rg.column(i))
            .find(|c| c.column_path().string() == name)
            .expect("column chunk present");
        assert!(
            col.bloom_filter_offset().is_some(),
            "{name}: bloom filter written",
        );
    }
}
