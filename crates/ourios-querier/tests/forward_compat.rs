//! RFC0007.4 — forward-compatible reads (§3.5 / RFC 0005 §3.9).
//!
//! The querier reads through `DataFusion`'s `ListingTable`, which
//! infers one table schema from the *union* of the files it finds.
//! This test puts three files with **different** schemas in one
//! tenant directory and asserts a query still succeeds with the
//! right count:
//!
//! - a current full-schema file (the `ourios-parquet` writer);
//! - a **future** file with an extra column the reader doesn't
//!   know about (RFC 0005 §3.9 rule 1 — unknown columns ignored);
//! - an **old** file missing an OPTIONAL column (§3.9 rule 2 —
//!   missing optional columns default to absent/null, no error).
//!
//! The heterogeneous files are written via the raw `ArrowWriter`
//! over a batch built from the public `ourios_parquet::data_schema()`
//! so the fixtures stay in step with the real schema (no
//! duplicated field list). Missing *baseline REQUIRED* columns are
//! deliberately out of scope: §3.9 says those are a hard error, and
//! the criterion is about optional/unknown columns.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow_array::{ArrayRef, Int32Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;

use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, Writer, columns, mined_records_to_batch};
use ourios_querier::{Querier, QueryRequest};

/// 2026-04-02T10:58:00 UTC (hour=10) — same anchor as the other
/// querier tests.
const TS0: u64 = 1_775_127_480_000_000_000;
const HOUR_NS: u64 = 3_600_000_000_000;

fn rec(template_id: u64, ts_ns: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("a"),
        template_id,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("lib.cart".to_string()),
        scope_version: Some("1.0.0".to_string()),
        time_unix_nano: ts_ns,
        observed_time_unix_nano: Some(ts_ns + 1_000),
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

fn req(template_id: Option<u64>) -> QueryRequest {
    QueryRequest {
        tenant: TenantId::new("a"),
        time_range: None,
        template_id,
    }
}

/// Write `batch` as a single committed `*.parquet` at the RFC 0005
/// partition directory derived from `record` (so it lands under the
/// tenant/time path the querier scans).
fn write_raw_at(bucket: &Path, record: &MinedRecord, batch: &RecordBatch) {
    let dir = PartitionKey::derive(record)
        .expect("derive partition")
        .data_path(bucket);
    std::fs::create_dir_all(&dir).expect("mkdir partition");
    let file = File::create(dir.join("fixture.parquet")).expect("create parquet");
    let mut w = ArrowWriter::try_new(file, batch.schema(), None).expect("arrow writer");
    w.write(batch).expect("write batch");
    w.close().expect("close writer");
}

/// A batch built from the current schema, plus one extra column the
/// current reader has never heard of (a future writer's addition).
fn future_schema_batch(record: &MinedRecord) -> RecordBatch {
    let base = mined_records_to_batch(std::slice::from_ref(record)).expect("base batch");
    let mut fields: Vec<Field> = base
        .schema()
        .fields()
        .iter()
        .map(|f| f.as_ref().clone())
        .collect();
    fields.push(Field::new("future_field", DataType::Int32, true));

    let mut cols: Vec<ArrayRef> = base.columns().to_vec();
    cols.push(Arc::new(Int32Array::from(vec![7_i32])) as ArrayRef);

    RecordBatch::try_new(Arc::new(Schema::new(fields)), cols).expect("future batch")
}

/// A batch built from the current schema with one OPTIONAL column
/// (`severity_text`) dropped — what an older writer produced before
/// that column existed.
fn old_schema_batch(record: &MinedRecord) -> RecordBatch {
    let base = mined_records_to_batch(std::slice::from_ref(record)).expect("base batch");
    let keep: Vec<usize> = base
        .schema()
        .fields()
        .iter()
        .enumerate()
        .filter(|(_, f)| f.name() != columns::SEVERITY_TEXT)
        .map(|(i, _)| i)
        .collect();
    base.project(&keep).expect("old batch")
}

/// RFC0007.4 — a tenant directory holding three files with three
/// different schemas (current, future-with-extra-column,
/// old-missing-optional-column) queries without error and returns
/// the correct count. This is the §4.6 read path (`DataFusion`
/// `ListingTable`) honouring RFC 0005 §3.9 rules 1 + 2.
#[tokio::test]
async fn rfc0007_4_heterogeneous_schemas_read_without_error() {
    let bucket = tempfile::TempDir::new().expect("temp");

    // hour 10 — current full schema, via the real writer.
    let current = rec(1, TS0);
    {
        let mut w = Writer::open(
            bucket.path(),
            PartitionKey::derive(&current).expect("derive"),
        )
        .expect("open writer");
        w.append_records(std::slice::from_ref(&current))
            .expect("append");
        w.close().expect("close");
    }

    // hour 11 — future writer: an extra, unknown column.
    let future = rec(1, TS0 + HOUR_NS);
    write_raw_at(bucket.path(), &future, &future_schema_batch(&future));

    // hour 12 — old writer: a missing OPTIONAL column.
    let old = rec(1, TS0 + 2 * HOUR_NS);
    write_raw_at(bucket.path(), &old, &old_schema_batch(&old));

    let q = Querier::new(bucket.path());

    // Template-exact across the heterogeneous corpus: all three
    // rows are template 1 and must be counted, schema drift and all.
    let r = q
        .run(req(Some(1)))
        .await
        .expect("heterogeneous-schema query must not error");
    assert_eq!(
        r.rows, 3,
        "all three rows count despite the extra/missing columns",
    );

    // And an unfiltered scan agrees — the unknown column is ignored,
    // the missing optional column defaults to absent, no read error.
    let all = q.run(req(None)).await.expect("unfiltered query");
    assert_eq!(all.rows, 3);
}
