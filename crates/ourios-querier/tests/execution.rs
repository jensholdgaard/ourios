//! Execution tests — `Querier::run` against a real RFC 0005
//! Parquet store written by `ourios-parquet`. Covers the minimal
//! predicate set (tenant + time range + template-exact), RFC0007.5
//! tenant isolation, B1 row-group pruning (RFC0007.1) and B2 —
//! template-exact work tracks result size, not corpus size
//! (RFC0007.2).

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

/// One hour in nanoseconds — bump a record into the next
/// `hour=` partition (a distinct file ⇒ a distinct row group).
const HOUR_NS: u64 = 3_600_000_000_000;

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

/// Lay down a corpus for tenant "a": the **target** template
/// (`result_rows` rows in hour 10, one file ⇒ one row group) plus
/// `n_filler` distinct non-target templates, each its own template
/// in its own hour (so each is a separate file with a `template_id`
/// min/max that excludes the target). A `template_id = target`
/// query must therefore read only the target file's row group and
/// prune every filler row group — regardless of `n_filler`.
fn corpus_with_filler(bucket: &Path, target: u64, result_rows: u64, n_filler: u64) {
    let tgt: Vec<MinedRecord> = (0..result_rows)
        .map(|i| rec("a", target, TS0 + i * 1_000_000))
        .collect();
    write_all(bucket, &tgt);
    for k in 0..n_filler {
        // Distinct template id (never the target) in a distinct
        // hour, so each filler is its own file / row group.
        let template = target + 1 + k;
        let base = TS0 + (k + 1) * HOUR_NS;
        let fill: Vec<MinedRecord> = (0..result_rows)
            .map(|i| rec("a", template, base + i * 1_000_000))
            .collect();
        write_all(bucket, &fill);
    }
}

fn req(tenant: &str, time_range: Option<(u64, u64)>, template_id: Option<u64>) -> QueryRequest {
    QueryRequest {
        tenant: TenantId::new(tenant),
        time_range,
        template_id,
        severity_text: None,
    }
}

/// A record with an explicit severity (for the B1 `level='ERROR'`
/// query shape) — `severity_text` and the canonical OTLP
/// `severity_number` are set coherently, so a fixture keyed off
/// either column agrees — otherwise identical to [`rec`].
fn rec_sev(tenant: &str, template_id: u64, ts_ns: u64, severity: &str) -> MinedRecord {
    // OTLP severity-number ranges (lower bound of each band).
    let severity_number: u8 = match severity {
        "TRACE" => 1,
        "DEBUG" => 5,
        "INFO" => 9,
        "WARN" => 13,
        "ERROR" => 17,
        "FATAL" => 21,
        _ => 0, // UNSPECIFIED
    };
    MinedRecord {
        severity_text: Some(severity.to_string()),
        severity_number,
        ..rec(tenant, template_id, ts_ns)
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

/// RFC0007.1 (B1) — a selective `template_id` query prunes row
/// groups via Parquet statistics rather than scanning them.
/// Tenant "a" gets two files in different hours: one holds only
/// template 1, the other only template 2. A `template_id = 1`
/// query must skip the template-2 file's row group (its
/// `template_id` min/max can't satisfy `= 1`), so
/// `row_groups_pruned > 0` and the pruned fraction is positive.
#[tokio::test]
async fn rfc0007_1_pushdown_prunes_row_groups() {
    let bucket = tempfile::TempDir::new().expect("temp");
    // Different hours ⇒ different partition files; each file's
    // single row group carries a distinct template_id min/max.
    write_all(bucket.path(), &[rec("a", 1, TS0)]); // hour 10 → template 1
    write_all(bucket.path(), &[rec("a", 2, TS0 + 3_600_000_000_000)]); // hour 11 → template 2

    let q = Querier::new(bucket.path());
    let r = q
        .run(req("a", None, Some(1)))
        .await
        .expect("template-exact query");

    assert_eq!(r.rows, 1, "only the one template-1 row matches");
    let total = r.stats.row_groups_scanned + r.stats.row_groups_pruned;
    // pruned fraction `pruned / total` is positive iff both are
    // non-zero — assert that integer-wise (no float).
    assert!(
        r.stats.row_groups_pruned >= 1,
        "the template-2 file's row group must be pruned by statistics; stats={:?}",
        r.stats,
    );
    assert!(
        total > r.stats.row_groups_pruned,
        "at least one row group was also scanned (matched); stats={:?}",
        r.stats,
    );
    // The `bytes_scanned` extraction must stay wired — a regression
    // to always-0 would otherwise pass the row-group asserts above.
    assert!(
        r.stats.bytes_read > 0,
        "the scanned row group reads bytes; stats={:?}",
        r.stats,
    );
}

/// RFC0007.1 (B1) — the `level='ERROR'` query shape. A
/// `severity_text = 'ERROR'` filter both counts correctly and
/// prunes via Parquet statistics: an INFO-only file in another hour
/// has a `severity_text` min/max that can't satisfy `= 'ERROR'`, so
/// its row group is skipped. This is the structured predicate that
/// the B1 reference (`zstdcat | grep ERROR`) does by scanning.
#[tokio::test]
async fn rfc0007_1_severity_filter_counts_and_prunes() {
    let bucket = tempfile::TempDir::new().expect("temp");
    // hour 10: two ERROR rows + one INFO row (mixed file).
    write_all(
        bucket.path(),
        &[
            rec_sev("a", 1, TS0, "ERROR"),
            rec_sev("a", 1, TS0 + 1_000_000, "ERROR"),
            rec_sev("a", 1, TS0 + 2_000_000, "INFO"),
        ],
    );
    // hour 11: an INFO-only file ⇒ its row group's severity_text
    // min/max is INFO..INFO, which `= 'ERROR'` can prune.
    write_all(bucket.path(), &[rec_sev("a", 1, TS0 + HOUR_NS, "INFO")]);

    let q = Querier::new(bucket.path());
    let r = q
        .run(QueryRequest {
            tenant: TenantId::new("a"),
            time_range: None,
            template_id: None,
            severity_text: Some("ERROR".to_string()),
        })
        .await
        .expect("severity query");

    assert_eq!(r.rows, 2, "only the two ERROR rows match");
    assert!(
        r.stats.row_groups_pruned >= 1,
        "the INFO-only file's row group is pruned by severity_text stats; stats={:?}",
        r.stats,
    );
}

/// RFC0007.2 (B2) — the inverted-index-collapse claim, measured
/// structurally instead of by wall clock: for a fixed-result
/// template-exact query, the *work the engine does* tracks the
/// result size, not the corpus size. Two corpora hold the **same**
/// target file (5 rows of template 1) but differ ~8× in total size
/// (3 vs 30 filler templates, each its own row group). A
/// `template_id = 1` query reads the same row group in both —
/// `row_groups_scanned` and `bytes_read` are flat — while the
/// extra corpus is absorbed entirely by pruning.
#[tokio::test]
async fn rfc0007_2_template_exact_work_scales_with_result_not_corpus() {
    let small = tempfile::TempDir::new().expect("temp small");
    let large = tempfile::TempDir::new().expect("temp large");
    corpus_with_filler(small.path(), 1, 5, 3); //  4 files
    corpus_with_filler(large.path(), 1, 5, 30); // 31 files (~8× the corpus)

    let s = Querier::new(small.path())
        .run(req("a", None, Some(1)))
        .await
        .expect("small query");
    let l = Querier::new(large.path())
        .run(req("a", None, Some(1)))
        .await
        .expect("large query");

    // Same fixed result in both corpora.
    assert_eq!(s.rows, 5);
    assert_eq!(l.rows, 5, "result size is fixed regardless of corpus");

    // The headline: the work scanned for the fixed result is FLAT
    // across an ~8× larger corpus — only the target row group is
    // read in either case.
    assert_eq!(
        s.stats.row_groups_scanned, l.stats.row_groups_scanned,
        "row groups scanned tracks result, not corpus; small={:?} large={:?}",
        s.stats, l.stats,
    );
    assert_eq!(
        s.stats.bytes_read, l.stats.bytes_read,
        "bytes read for the fixed result is flat across corpus sizes; small={:?} large={:?}",
        s.stats, l.stats,
    );

    // …and the corpus growth is absorbed entirely by pruning: the
    // larger corpus prunes strictly more row groups, and its total
    // row-group count is far larger while scanned stays flat.
    assert!(
        l.stats.row_groups_pruned > s.stats.row_groups_pruned,
        "the larger corpus prunes strictly more row groups; small={:?} large={:?}",
        s.stats,
        l.stats,
    );
    let s_total = s.stats.row_groups_scanned + s.stats.row_groups_pruned;
    let l_total = l.stats.row_groups_scanned + l.stats.row_groups_pruned;
    assert!(
        l_total >= s_total + 20,
        "the large corpus genuinely has many more row groups; small_total={s_total} large_total={l_total}",
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
