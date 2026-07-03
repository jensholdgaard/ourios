//! RFC 0022 §3.2 threading — a record sink configured with a promoted
//! attribute set projects it into every flushed file, on **both** write
//! paths: the locked flush (`flush_all`, the size/age/rotation triggers'
//! shared path) and the off-lock publish (`publish_owned`, the issue #302
//! coordinator path). The projection semantics themselves are the
//! `ourios-parquet` RFC0022.1/.2 suites' contract; here we pin that the
//! sink threads its configured set into them.

use std::path::Path;

use ourios_core::audit::ParamType;
use ourios_core::otlp::any_value::Value as AvValue;
use ourios_core::otlp::{AnyValue, KeyValue};
use ourios_core::record::{BodyKind, MinedRecord, Param, RecordSink};
use ourios_core::tenant::TenantId;
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_parquet::{PromotedAttributes, Store};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

fn kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(AvValue::StringValue(value.to_string())),
        }),
        ..Default::default()
    }
}

fn rec() -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("a"),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: None,
        scope_name: Some("lib.cart".to_string()),
        scope_version: Some("1.0.0".to_string()),
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: 1_775_127_480_000_000_000,
        observed_time_unix_nano: None,
        attributes: vec![kv("http.route", "/cart")],
        dropped_attributes_count: 0,
        resource_attributes: vec![kv("service.name", "api")],
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

/// A "never trigger" flush config, so the test drives each path explicitly.
fn never_flush() -> FlushConfig {
    FlushConfig {
        target_bytes: usize::MAX,
        max_buffer_age: std::time::Duration::from_secs(86_400),
        ceiling_bytes: usize::MAX,
    }
}

fn promoted_set() -> PromotedAttributes {
    PromotedAttributes::new([], ["http.route".to_string()])
}

/// The column names of every flushed `*.parquet` file under `root`.
fn flushed_schemas(root: &Path) -> Vec<Vec<String>> {
    let mut schemas = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "parquet") {
                let file = std::fs::File::open(&path).expect("open flushed file");
                let reader =
                    ParquetRecordBatchReaderBuilder::try_new(file).expect("read flushed footer");
                schemas.push(
                    reader
                        .schema()
                        .fields()
                        .iter()
                        .map(|f| f.name().clone())
                        .collect(),
                );
            }
        }
    }
    schemas
}

fn assert_promoted(schemas: &[Vec<String>]) {
    assert_eq!(schemas.len(), 1, "exactly one file flushed");
    let names = &schemas[0];
    assert!(
        names.iter().any(|n| n == "attr.http.route"),
        "the configured key's column is projected: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "resource.service.name"),
        "the implicit promotion rides along: {names:?}"
    );
}

/// The locked flush path (`flush_all` — size/age/rotation triggers).
#[test]
fn flush_path_projects_the_configured_set() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let mut sink = ParquetRecordSink::new(Store::local(dir.path()).expect("store"), never_flush())
        .with_promoted_attributes(promoted_set());
    sink.emit(rec());
    sink.flush_all();
    assert_promoted(&flushed_schemas(dir.path()));
}

/// The off-lock publish path (`publish_owned` — the issue #302 coordinator).
#[test]
fn publish_path_projects_the_configured_set() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let shared = SharedParquetSink::new(
        ParquetRecordSink::new(Store::local(dir.path()).expect("store"), never_flush())
            .with_promoted_attributes(promoted_set()),
    );
    let mut producer = shared.clone();
    producer.emit(rec());
    let batches = shared.drain_all();
    assert!(
        shared.publish_owned(batches, "test"),
        "the drained partition publishes"
    );
    assert_promoted(&flushed_schemas(dir.path()));
}
