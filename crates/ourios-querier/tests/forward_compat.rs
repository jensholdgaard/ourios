//! RFC0007.4 — forward-compatible reads (§3.5 / RFC 0005 §3.9).
//!
//! The querier reads through `DataFusion`'s `ListingTable`, which
//! infers one table schema from the *union* of the files it finds.
//! This test puts three files with **different** schemas — each
//! under a distinct `template_id` — in one tenant directory and
//! asserts every one of them stays individually queryable through
//! the real querier path:
//!
//! - a current full-schema file (the `ourios-parquet` writer);
//! - a **future** file with an extra column the reader doesn't
//!   know about (RFC 0005 §3.9 rule 1 — unknown columns ignored);
//! - an **old** file missing an OPTIONAL column (§3.9 rule 2 — the
//!   read tolerates the absence without error).
//!
//! Because each file carries a distinct `template_id`, a
//! template-exact query that matches the future / old file proves
//! that file was *read* — schema drift and all — not silently
//! dropped during schema union. (The querier is count-only, so the
//! row-level value of a missing optional column — its `None`
//! default — is the `ourios-parquet` `Reader`'s contract, asserted
//! there; here we assert the read path tolerates the drift.)
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

fn req(time_range: Option<(u64, u64)>, template_id: Option<u64>) -> QueryRequest {
    QueryRequest {
        tenant: TenantId::new("a"),
        time_range,
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
/// old-missing-optional-column), each under a distinct
/// `template_id`. Every file stays individually queryable through
/// the §4.6 read path (`DataFusion` `ListingTable`), honouring
/// RFC 0005 §3.9 rules 1 + 2.
#[tokio::test]
async fn rfc0007_4_heterogeneous_schemas_stay_queryable() {
    let bucket = tempfile::TempDir::new().expect("temp");

    // hour 10 / template 1 — current full schema, via the real writer.
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

    // hour 11 / template 2 — future writer: an extra, unknown column.
    let future = rec(2, TS0 + HOUR_NS);
    write_raw_at(bucket.path(), &future, &future_schema_batch(&future));

    // hour 12 / template 3 — old writer: a missing OPTIONAL column.
    let old = rec(3, TS0 + 2 * HOUR_NS);
    write_raw_at(bucket.path(), &old, &old_schema_batch(&old));

    let q = Querier::new(bucket.path());

    // Each file is independently addressable by its template_id.
    // Matching the *future* file's row proves the file was read
    // despite the unknown extra column (§3.9 rule 1) — not dropped
    // during schema union.
    let f = q
        .run(req(None, Some(2)))
        .await
        .expect("future-schema file must query without error");
    assert_eq!(f.rows, 1, "the file with an unknown extra column is read");

    // Matching the *old* file's row proves the read tolerates the
    // missing OPTIONAL column (§3.9 rule 2).
    let o = q
        .run(req(None, Some(3)))
        .await
        .expect("old-schema file must query without error");
    assert_eq!(o.rows, 1, "the file missing an optional column is read");

    // The current file is unaffected.
    let c = q.run(req(None, Some(1))).await.expect("current file");
    assert_eq!(c.rows, 1);

    // A time-range predicate also resolves against a drifted file:
    // the half-open window around hour 11 selects only the future
    // file, so pushdown and schema drift coexist.
    let windowed = q
        .run(req(Some((TS0 + HOUR_NS, TS0 + HOUR_NS + 1)), None))
        .await
        .expect("time-filtered query over drifted files");
    assert_eq!(windowed.rows, 1, "time pushdown works across schema drift");

    // And an unfiltered scan reads all three heterogeneous files.
    let all = q.run(req(None, None)).await.expect("unfiltered query");
    assert_eq!(all.rows, 3, "all three heterogeneous files are read");
}
