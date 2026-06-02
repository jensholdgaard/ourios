//! `ourios-querier` — RFC 0007 querier (pillar #3, `DataFusion`).
//!
//! **Status: execution slice 2.** [`Querier::run`] executes a
//! minimal query — tenant scope + optional time range + optional
//! template-exact id — against the RFC 0005 Parquet store via
//! `DataFusion`, returning a matching-row count **and the scan's
//! row-group pruning stats** ([`QueryStats`]). Tenant isolation
//! (RFC0007.5) and B1 pruning (RFC0007.1 — a selective query
//! provably skips row groups via statistics) are live + tested.
//! The B2 latency-vs-corpus-size bench (RFC0007.2) comes next.
//!
//! This crate is the **read path**: it runs the query against the
//! RFC 0005 store — scoped to the tenant's partition directory,
//! with `template_id` / `time_unix_nano` column filters (RFC 0005
//! §3.3/§3.6) — and returns results **without** leaking
//! `DataFusion` or SQL through the public API (hazard `CLAUDE.md`
//! §4.6). It reads the shipped RFC 0005 store; it needs neither
//! the WAL nor the receiver.
//!
//! (Partition-level *time* pruning — deriving `year/month/day/hour`
//! path bounds from the time range so whole directories are
//! skipped — is a later refinement; today the time bound is a
//! column predicate, and the row-group skipping it enables is what
//! slice 2 / B1 measures.)
//!
//! **Throwaway query surface.** [`QueryRequest`] is intentionally
//! minimal — just the predicates B1/B2 need. The real logs DSL
//! (RFC 0002) is deferred until B1/B2 prove the query thesis is
//! worth a stable language; until then this surface may change
//! freely (maintainer decision).

#![deny(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use datafusion::arrow::array::{Array, Int64Array};
use datafusion::arrow::datatypes::DataType;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::ScalarValue;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{
    ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl,
};
use datafusion::error::DataFusionError;
use datafusion::functions_aggregate::expr_fn::count;
use datafusion::physical_plan::metrics::MetricValue;
use datafusion::physical_plan::{ExecutionPlan, collect};
use datafusion::prelude::{SessionContext, col, lit};
use ourios_core::tenant::TenantId;
use ourios_parquet::columns;
use ourios_parquet::percent_encode_tenant;

/// A logs query to execute. **Throwaway surface** while the query
/// thesis (B1/B2) is unproven — per the maintainer decision, DSL
/// contracts (RFC 0002) are deferred until B1/B2 say the querier
/// is worth a stable language. So this carries only the minimal
/// predicates B1/B2 need: tenant scope, optional time bounds, and
/// optional template-exact id — exactly the RFC 0005 §3.3
/// pushdown keys.
#[derive(Debug, Clone)]
pub struct QueryRequest {
    /// Tenant whose data the query is scoped to. Enforced
    /// structurally — the querier only ever reads under this
    /// tenant's partition directory (`CLAUDE.md` §3.7; RFC0007.5).
    pub tenant: TenantId,
    /// Optional `[start, end)` `time_unix_nano` bounds.
    pub time_range: Option<(u64, u64)>,
    /// Optional template-exact filter (B2 — `template_id` equality).
    pub template_id: Option<u64>,
}

/// Pruning / IO accounting for one query, surfaced so B1
/// (RFC0007.1) can assert pushdown actually skipped data rather
/// than scanning it. Plain integers — no `DataFusion`/arrow types
/// cross this boundary (hazard §4.6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueryStats {
    /// Row groups `DataFusion` read.
    pub row_groups_scanned: u64,
    /// Row groups skipped via partition/statistics pruning. The
    /// B1 pruned fraction is
    /// `row_groups_pruned / (row_groups_scanned + row_groups_pruned)`.
    pub row_groups_pruned: u64,
    /// Bytes read from object storage for the query.
    pub bytes_read: u64,
}

/// Result of a query. The typed-row payload lands with the
/// execution slice (its shape follows the RFC 0002 projection +
/// RFC 0005 schema, and must stay free of arrow `RecordBatch`
/// leakage per §4.6); the scaffold carries the [`QueryStats`]
/// the B1/B2 gates assert on.
#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    /// Number of matching rows. (The typed-row payload — projected
    /// columns — lands when there's a query thesis worth shaping it
    /// around; B1/B2 only need the count + stats. Stays free of
    /// arrow `RecordBatch` leakage per §4.6.)
    pub rows: u64,
    pub stats: QueryStats,
}

/// Errors from [`Querier::run`]. Ourios-owned — no
/// `datafusion::*` / `arrow::*` / SQL types appear here or in
/// any public signature (hazard §4.6; RFC0007.3).
///
/// Marked `#[non_exhaustive]` because the execution slice will
/// add failure modes (parse/validation/auth) — matching the
/// `TokenizeError` / `BenchError` convention so downstream
/// matches don't break when variants land.
#[derive(Debug)]
#[non_exhaustive]
pub enum QueryError {
    /// The query referenced no tenant (cross-tenant scans are
    /// not expressible — RFC0007.5).
    TenantRequired,
    /// The query failed to compile from the logs DSL (RFC 0002).
    InvalidQuery { detail: String },
    /// Object-storage / Parquet read failure during execution.
    /// `detail` carries the underlying engine message for
    /// `Debug`/logs **only** — it is deliberately *not* rendered
    /// by `Display`, because `DataFusion`/arrow error text leaks
    /// implementation specifics the public surface must not expose
    /// (hazard §4.6 / RFC0007.3).
    Storage { detail: String },
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TenantRequired => write!(f, "query has no tenant scope"),
            Self::InvalidQuery { detail } => write!(f, "invalid query: {detail}"),
            // No `detail` here on purpose: the underlying engine
            // message would leak `DataFusion`/SQL specifics (§4.6).
            // The detail is preserved on the variant for `Debug`.
            Self::Storage { .. } => write!(f, "failed to read the log store"),
        }
    }
}

impl std::error::Error for QueryError {}

/// Whether `dir` (a tenant's partition root) holds at least one
/// published `*.parquet` file anywhere beneath it. Recursive
/// because the data is nested `year=/month=/day=/hour=/`. Files
/// the writer hasn't committed (`*.parquet.tmp`) have extension
/// `tmp`, so they don't count — the poisoned-writer case we treat
/// as "empty", not error.
///
/// A missing directory (`NotFound`) is "empty" (`Ok(false)`); any
/// *other* I/O error (permission denied, transient failure) is
/// propagated as [`QueryError::Storage`] rather than silently
/// masked as "no data" — a wrong zero-row answer is worse than a
/// surfaced error.
fn has_published_parquet(dir: &std::path::Path) -> Result<bool, QueryError> {
    let io_err = |op: &str, p: &std::path::Path, e: &std::io::Error| QueryError::Storage {
        detail: format!("{op} {}: {e}", p.display()),
    };
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(entries) => entries,
            // The dir (or a subdir, lost to a concurrent
            // housekeeping unlink) simply isn't there → not data,
            // not an error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err("read_dir", &d, &e)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| io_err("read_dir entry", &d, &e))?;
            let path = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(_) if path.extension().is_some_and(|x| x == "parquet") => return Ok(true),
                Ok(_) => {}
                Err(e) => return Err(io_err("file_type", &path, &e)),
            }
        }
    }
    Ok(false)
}

/// Pull the single aggregate count out of the result batches.
fn count_value(batches: &[RecordBatch]) -> u64 {
    batches
        .first()
        .filter(|b| b.num_rows() > 0)
        .and_then(|b| b.column(0).as_any().downcast_ref::<Int64Array>())
        .map_or(0, |a| u64::try_from(a.value(0)).unwrap_or(0))
}

/// Walk the executed physical plan and accumulate the scan
/// pruning / IO metrics into a [`QueryStats`]. Recursive — the
/// Parquet scan is a leaf under the aggregate.
fn scan_stats(plan: &dyn ExecutionPlan) -> QueryStats {
    let mut stats = QueryStats::default();
    accumulate_scan_stats(plan, &mut stats);
    stats
}

fn accumulate_scan_stats(plan: &dyn ExecutionPlan, stats: &mut QueryStats) {
    if let Some(metrics) = plan.metrics() {
        // `aggregate_by_name` sums each metric across the scan's
        // per-file / per-partition instances.
        for metric in metrics.aggregate_by_name().iter() {
            match metric.value() {
                // `row_groups_pruned_statistics` is a PruningMetrics
                // carrying both pruned (skipped via min/max stats)
                // and matched (read) row-group counts — exactly the
                // B1 numerator + denominator.
                MetricValue::PruningMetrics {
                    name,
                    pruning_metrics,
                } if name == "row_groups_pruned_statistics" => {
                    stats.row_groups_pruned += pruning_metrics.pruned() as u64;
                    stats.row_groups_scanned += pruning_metrics.matched() as u64;
                }
                MetricValue::Count { name, count } if name == "bytes_scanned" => {
                    stats.bytes_read += count.value() as u64;
                }
                _ => {}
            }
        }
    }
    for child in plan.children() {
        accumulate_scan_stats(child.as_ref(), stats);
    }
}

/// Map a `DataFusion` error to the Ourios-owned [`QueryError`] so
/// no `datafusion`/`arrow` type crosses the public boundary (§4.6).
// Takes the error by value so it drops in cleanly as
// `.map_err(storage_err)`, which hands an owned error.
#[allow(clippy::needless_pass_by_value)]
fn storage_err(e: DataFusionError) -> QueryError {
    QueryError::Storage {
        detail: e.to_string(),
    }
}

/// The query engine. One per querier process; reads the RFC 0005
/// Parquet store rooted at `bucket_root` (the writer's
/// `<bucket_root>/data/...` layout).
#[derive(Debug, Clone)]
pub struct Querier {
    bucket_root: PathBuf,
}

impl Querier {
    /// Create a querier reading the RFC 0005 store under
    /// `bucket_root` (the same root the `ourios-parquet` writer
    /// writes `data/tenant_id=…/year=…/…` under).
    pub fn new(bucket_root: impl Into<PathBuf>) -> Self {
        Self {
            bucket_root: bucket_root.into(),
        }
    }

    /// Execute `request` against the RFC 0005 store with predicate
    /// pushdown and return the matching row count + pruning stats,
    /// without exposing `DataFusion` (§4.6).
    ///
    /// Tenant isolation is structural: the listing table is rooted
    /// at the request tenant's `data/tenant_id=<enc>/` directory,
    /// so no other tenant's rows are reachable (RFC0007.5). A
    /// tenant with no data on disk yields an empty result.
    ///
    /// # Errors
    ///
    /// See [`QueryError`].
    pub async fn run(&self, request: QueryRequest) -> Result<QueryResult, QueryError> {
        let enc = percent_encode_tenant(request.tenant.as_str());
        let tenant_dir = self
            .bucket_root
            .join("data")
            .join(format!("tenant_id={enc}"));
        // No published `*.parquet` under the tenant dir ⇒ the
        // tenant has nothing queryable ⇒ empty result (not an
        // error). Covers both the missing-dir case and a dir that
        // holds only `*.parquet.tmp` (a poisoned/crashed writer) or
        // empty partition dirs — where `infer_schema` would
        // otherwise error and wrongly fail the query.
        if !has_published_parquet(&tenant_dir)? {
            return Ok(QueryResult::default());
        }

        let ctx = SessionContext::new();
        // Build the table URL from the canonical absolute path,
        // scheme-less with a trailing slash. DataFusion 53 treats
        // an absolute filesystem path as local and URI-encodes it
        // internally — so spaces / reserved characters in the
        // bucket path are handled, unlike a hand-built `file://…`
        // string. `canonicalize` is safe: we just confirmed the
        // directory exists. The trailing slash marks it a
        // directory (not a single object).
        let abs = tenant_dir.canonicalize().map_err(|e| QueryError::Storage {
            detail: format!("canonicalize {}: {e}", tenant_dir.display()),
        })?;
        let url = ListingTableUrl::parse(format!("{}/", abs.display())).map_err(storage_err)?;
        // `year/month/day/hour` are path-only Hive partition cols
        // (parsed from the directory names); `tenant_id` is *not*
        // listed — relative to this tenant-scoped root it's a plain
        // file column, and the rooting is what enforces isolation.
        let options = ListingOptions::new(Arc::new(ParquetFormat::default()))
            .with_file_extension(".parquet")
            .with_table_partition_cols(vec![
                ("year".to_string(), DataType::Utf8),
                ("month".to_string(), DataType::Utf8),
                ("day".to_string(), DataType::Utf8),
                ("hour".to_string(), DataType::Utf8),
            ]);
        let schema = options
            .infer_schema(&ctx.state(), &url)
            .await
            .map_err(storage_err)?;
        let config = ListingTableConfig::new(url)
            .with_listing_options(options)
            .with_schema(schema);
        let table = ListingTable::try_new(config).map_err(storage_err)?;
        ctx.register_table("logs", Arc::new(table))
            .map_err(storage_err)?;

        let mut df = ctx.table("logs").await.map_err(storage_err)?;
        if let Some((start, end)) = request.time_range {
            // `time_unix_nano` is Timestamp(Nanosecond, "UTC")
            // (RFC 0005 schema); match the literal type exactly.
            let to_ts = |v: u64| -> Result<ScalarValue, QueryError> {
                let ns = i64::try_from(v).map_err(|_| QueryError::InvalidQuery {
                    detail: format!("time bound {v} exceeds i64 nanoseconds"),
                })?;
                Ok(ScalarValue::TimestampNanosecond(
                    Some(ns),
                    Some("UTC".into()),
                ))
            };
            df = df
                .filter(
                    col(columns::TIME_UNIX_NANO)
                        .gt_eq(lit(to_ts(start)?))
                        .and(col(columns::TIME_UNIX_NANO).lt(lit(to_ts(end)?))),
                )
                .map_err(storage_err)?;
        }
        if let Some(template_id) = request.template_id {
            df = df
                .filter(col(columns::TEMPLATE_ID).eq(lit(template_id)))
                .map_err(storage_err)?;
        }

        // Count via an aggregate so the heavy `attributes` /
        // `params` / `body` columns are never materialised
        // (projection pushdown). We build + execute the physical
        // plan ourselves (rather than `df.count()`) so we can read
        // the scan's pruning metrics off the retained plan.
        let counted = df
            .aggregate(vec![], vec![count(lit(1_i64)).alias("n")])
            .map_err(storage_err)?;
        let plan = counted.create_physical_plan().await.map_err(storage_err)?;
        let batches = collect(Arc::clone(&plan), ctx.task_ctx())
            .await
            .map_err(storage_err)?;
        let rows = count_value(&batches);
        let stats = scan_stats(plan.as_ref());
        Ok(QueryResult { rows, stats })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The operator-facing `QueryError` messages are a contract
    /// (hazard §4.6: no DataFusion/SQL leakage, so operators rely
    /// on these); pin them so a refactor can't silently reword.
    #[test]
    fn query_error_display_messages_are_stable() {
        assert_eq!(
            QueryError::TenantRequired.to_string(),
            "query has no tenant scope",
        );
        assert_eq!(
            QueryError::InvalidQuery {
                detail: "bad filter".into(),
            }
            .to_string(),
            "invalid query: bad filter",
        );
        // Storage Display is intentionally generic — the engine
        // `detail` is NOT surfaced (it would leak DataFusion/SQL
        // specifics, §4.6 / RFC0007.3).
        assert_eq!(
            QueryError::Storage {
                detail: "Error during planning: SQL ...".into(),
            }
            .to_string(),
            "failed to read the log store",
        );
    }

    /// An empty result reports zero pruning/IO — the B1 baseline
    /// the execution slice fills in.
    #[test]
    fn default_result_has_zeroed_stats() {
        let r = QueryResult::default();
        assert_eq!(r.stats, QueryStats::default());
        assert_eq!(r.stats.row_groups_scanned, 0);
        assert_eq!(r.stats.row_groups_pruned, 0);
        assert_eq!(r.stats.bytes_read, 0);
    }
}
