//! Execution slice 1 — `Querier::run` against a real RFC 0005
//! Parquet store written by `ourios-parquet`. Covers the minimal
//! predicate set B1/B2 need (tenant + time range + template-exact)
//! and RFC0007.5 tenant isolation. Row-group pruning stats
//! (RFC0007.1) are slice 2.

use std::collections::HashMap;
use std::path::Path;

use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, Writer};
use ourios_querier::{Querier, QueryError, QueryRequest};

/// 2026-04-02T10:58:00 UTC — all test records sit in one hour
/// (one partition per tenant), matching the round-trip fixtures.
const TS0: u64 = 1_775_127_480_000_000_000;

fn rec(tenant: &str, template_id: u64, ts_ns: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new(tenant),
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

/// Write records into the RFC 0005 store under `bucket`, grouping
/// by partition the way the ingester would.
fn write_all(bucket: &Path, recs: &[MinedRecord]) {
    let mut by_part: HashMap<PartitionKey, Vec<MinedRecord>> = HashMap::new();
    for r in recs {
        by_part
            .entry(PartitionKey::derive(r).expect("derive partition"))
            .or_default()
            .push(r.clone());
    }
    for (part, rs) in by_part {
        let mut w = Writer::open(bucket, part).expect("open writer");
        w.append_records(&rs).expect("append");
        w.close().expect("close");
    }
}

fn req(tenant: &str, time_range: Option<(u64, u64)>, template_id: Option<u64>) -> QueryRequest {
    QueryRequest {
        tenant: TenantId::new(tenant),
        time_range,
        template_id,
    }
}

/// Tenant "a": 3 rows (template 1 ×2, template 2 ×1); tenant "b":
/// 1 row. Exercises the full filter surface.
#[tokio::test]
async fn executes_and_counts_matching_rows() {
    let bucket = tempfile::TempDir::new().expect("temp");
    let a = vec![
        rec("a", 1, TS0),
        rec("a", 1, TS0 + 1_000_000),
        rec("a", 2, TS0 + 2_000_000),
    ];
    let b = vec![rec("b", 1, TS0)];
    write_all(bucket.path(), &a);
    write_all(bucket.path(), &b);

    let q = Querier::new(bucket.path());

    // All of tenant a.
    let r = q.run(req("a", None, None)).await.expect("run");
    assert_eq!(r.rows, 3, "tenant a has 3 rows");

    // Template-exact (B2 shape): template 1 only.
    let r = q.run(req("a", None, Some(1))).await.expect("run");
    assert_eq!(r.rows, 2, "tenant a has 2 template-1 rows");

    // Time range [TS0, TS0 + 1.5ms): the first two rows.
    let r = q
        .run(req("a", Some((TS0, TS0 + 1_500_000)), None))
        .await
        .expect("run");
    assert_eq!(r.rows, 2, "two rows fall in the half-open window");

    // Template + time combined: template 2 is at TS0+2ms, outside
    // the window → zero.
    let r = q
        .run(req("a", Some((TS0, TS0 + 1_500_000)), Some(2)))
        .await
        .expect("run");
    assert_eq!(r.rows, 0);
}

/// RFC0007.5 — tenant isolation. A query for one tenant never sees
/// another's rows (structural: the listing table is rooted at the
/// tenant's partition dir), and a tenant with no data is empty,
/// not an error.
#[tokio::test]
async fn rfc0007_5_tenant_isolation() {
    let bucket = tempfile::TempDir::new().expect("temp");
    write_all(
        bucket.path(),
        &[rec("a", 1, TS0), rec("a", 1, TS0 + 1_000_000)],
    );
    write_all(bucket.path(), &[rec("b", 1, TS0)]);

    let q = Querier::new(bucket.path());

    let a = q.run(req("a", None, None)).await.expect("run a");
    assert_eq!(a.rows, 2, "tenant a sees only its 2 rows");

    let b = q.run(req("b", None, None)).await.expect("run b");
    assert_eq!(b.rows, 1, "tenant b sees only its 1 row");

    // A tenant that wrote nothing: empty result, not an error.
    let none = q.run(req("ghost", None, None)).await.expect("run ghost");
    assert_eq!(none.rows, 0, "unknown tenant yields an empty result");
}

/// A tenant directory that exists but holds no *committed*
/// `*.parquet` (only an uncommitted `*.parquet.tmp`, e.g. a
/// crashed writer) is an empty result, not an error — the query
/// must not fail when schema inference would find nothing.
#[tokio::test]
async fn tenant_dir_without_committed_parquet_is_empty() {
    let bucket = tempfile::TempDir::new().expect("temp");
    // Mimic a poisoned/crashed writer: the partition dir and a
    // `.tmp` file exist, but nothing was committed to `.parquet`.
    let part = bucket
        .path()
        .join("data/tenant_id=a/year=2026/month=04/day=02/hour=10");
    std::fs::create_dir_all(&part).expect("mkdir partition");
    std::fs::write(
        part.join("01890000-0000-7000-8000-000000000000.parquet.tmp"),
        b"partial",
    )
    .expect("write tmp");

    let q = Querier::new(bucket.path());
    let r = q
        .run(req("a", None, None))
        .await
        .expect("run must not error");
    assert_eq!(
        r.rows, 0,
        "uncommitted .tmp-only tenant dir is empty, not an error"
    );
}

/// A `bucket_root` whose path contains a space still resolves —
/// the URL is built from the canonical path (`DataFusion`
/// URI-encodes it), not a raw `file://{display}` string that would
/// choke on the space.
#[tokio::test]
async fn bucket_path_with_spaces_resolves() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let bucket = tmp.path().join("ourios bucket with spaces");
    std::fs::create_dir_all(&bucket).expect("mkdir spaced bucket");
    write_all(&bucket, &[rec("a", 1, TS0), rec("a", 1, TS0 + 1_000_000)]);

    let q = Querier::new(&bucket);
    let r = q
        .run(req("a", None, None))
        .await
        .expect("run over spaced path");
    assert_eq!(r.rows, 2, "spaced bucket path queries correctly");
}

/// A real I/O error reading the tenant directory surfaces as
/// `QueryError::Storage` — it is NOT silently masked as an empty
/// result (which would be a wrong zero-row answer). Induced
/// portably: place a regular *file* where the tenant directory is
/// expected, so `read_dir` fails with `ENOTDIR` (not `NotFound`).
#[tokio::test]
async fn read_dir_error_surfaces_as_storage_not_empty() {
    let bucket = tempfile::TempDir::new().expect("temp");
    // tenant "x" → data/tenant_id=x; make it a file, not a dir.
    let data = bucket.path().join("data");
    std::fs::create_dir_all(&data).expect("mkdir data");
    std::fs::write(data.join("tenant_id=x"), b"not a directory").expect("write file");

    let q = Querier::new(bucket.path());
    let result = q.run(req("x", None, None)).await;
    assert!(
        matches!(result, Err(QueryError::Storage { .. })),
        "a non-NotFound read_dir error must surface as Storage, got {result:?}",
    );
}
