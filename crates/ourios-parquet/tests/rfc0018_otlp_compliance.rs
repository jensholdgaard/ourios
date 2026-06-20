//! RFC 0018 §5 — the scope-attribute / schema-URL storage arms.
//!
//! `.1` proves the three additive RFC 0018 §3.1 columns
//! (`scope_attributes`, `resource_schema_url`, `scope_schema_url`) persist
//! and round-trip through the production `Writer` → `Reader`. `.2` proves a
//! pre-amendment file (a batch written without those columns) still reads —
//! the §3.5 / RFC 0005 §3.9 missing-OPTIONAL-column carve-out — with the
//! fields surfacing absent.
//!
//! See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5/§6.

use std::fs::File;
use std::path::{Path, PathBuf};

use parquet::arrow::ArrowWriter;

use ourios_core::audit::ParamType;
use ourios_core::otlp::{AnyValue, KeyValue, any_value};
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, Reader, Writer, columns, mined_records_to_batch};

const TS0: u64 = 1_775_127_480_000_000_000;

fn kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_string())),
        }),
        ..Default::default()
    }
}

fn base_rec() -> MinedRecord {
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
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
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

/// Write `record` through the production `Writer`; return the emitted file +
/// its partition (for `Reader::open_partition`).
fn write_one(bucket: &Path, record: &MinedRecord) -> (PathBuf, PartitionKey) {
    let part = PartitionKey::derive(record).expect("derive partition");
    let mut w = Writer::open(bucket, part.clone()).expect("open writer");
    w.append_records(std::slice::from_ref(record))
        .expect("append");
    w.close().expect("close");
    let dir = part.data_path(bucket);
    let file = std::fs::read_dir(&dir)
        .expect("read partition dir")
        .map(|e| e.expect("dir entry").path())
        .find(|p| p.extension().is_some_and(|x| x == "parquet"))
        .expect("one parquet file");
    (file, part)
}

/// Scenario RFC0018.1 — scope attributes + schema URLs survive ingest→storage:
/// a record carrying `scope_attributes`, `scope_schema_url`, and
/// `resource_schema_url` round-trips through `Writer` → `Reader` unchanged.
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[test]
fn rfc0018_1_scope_fields_round_trip() {
    let bucket = tempfile::TempDir::new().expect("temp");
    let mut record = base_rec();
    record.scope_attributes = vec![kv("db.system", "postgres"), kv("library.mascot", "gopher")];
    record.resource_schema_url = Some("https://opentelemetry.io/schemas/1.31.0".to_string());
    record.scope_schema_url = Some("https://opentelemetry.io/schemas/1.0.0".to_string());

    let (file, part) = write_one(bucket.path(), &record);
    let read = Reader::open_partition(&file, part)
        .expect("open")
        .read_all()
        .expect("read_all");

    assert_eq!(read.len(), 1, "one row written, one read");
    let r = &read[0];
    assert_eq!(
        r.scope_attributes, record.scope_attributes,
        "scope_attributes round-trip (canonical JSON)"
    );
    assert_eq!(
        r.resource_schema_url, record.resource_schema_url,
        "resource_schema_url round-trip"
    );
    assert_eq!(
        r.scope_schema_url, record.scope_schema_url,
        "scope_schema_url round-trip"
    );
}

/// Scenario RFC0018.2 — the new columns are OPTIONAL / back-compatible: a
/// pre-amendment file (the three columns projected out) reads successfully,
/// with `scope_attributes` empty and the two `schema_url`s `None` — the
/// §3.5 / RFC 0005 §3.9 missing-OPTIONAL-column carve-out, no error.
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[test]
fn rfc0018_2_new_columns_back_compatible() {
    let bucket = tempfile::TempDir::new().expect("temp");
    let record = base_rec();

    // Build the batch, then drop the three RFC 0018 columns — exactly the
    // shape an older writer produced before they existed.
    let batch = mined_records_to_batch(std::slice::from_ref(&record)).expect("batch");
    let dropped = [
        columns::SCOPE_ATTRIBUTES,
        columns::RESOURCE_SCHEMA_URL,
        columns::SCOPE_SCHEMA_URL,
    ];
    let keep: Vec<usize> = batch
        .schema()
        .fields()
        .iter()
        .enumerate()
        .filter(|(_, f)| !dropped.contains(&f.name().as_str()))
        .map(|(i, _)| i)
        .collect();
    let old_batch = batch.project(&keep).expect("old-schema batch");

    let part = PartitionKey::derive(&record).expect("derive partition");
    let dir = part.data_path(bucket.path());
    std::fs::create_dir_all(&dir).expect("mkdir partition");
    let file = dir.join("fixture.parquet");
    {
        let out = File::create(&file).expect("create parquet");
        let mut w = ArrowWriter::try_new(out, old_batch.schema(), None).expect("arrow writer");
        w.write(&old_batch).expect("write batch");
        w.close().expect("close writer");
    }

    let read = Reader::open_partition(&file, part)
        .expect("open pre-amendment file")
        .read_all()
        .expect("read tolerates the absent columns");

    assert_eq!(read.len(), 1, "the pre-amendment row reads");
    let r = &read[0];
    assert!(
        r.scope_attributes.is_empty(),
        "absent scope_attributes column → empty vec"
    );
    assert_eq!(r.resource_schema_url, None, "absent column → None");
    assert_eq!(r.scope_schema_url, None, "absent column → None");
}
