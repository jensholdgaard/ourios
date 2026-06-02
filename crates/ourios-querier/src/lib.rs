//! `ourios-querier` — RFC 0007 querier (pillar #3, `DataFusion`).
//!
//! **Status: execution slice 1.** [`Querier::run`] executes a
//! minimal query — tenant scope + optional time range + optional
//! template-exact id — against the RFC 0005 Parquet store via
//! `DataFusion`, returning a matching-row count. Tenant isolation
//! (RFC0007.5) is live + tested; the row-group pruning stats that
//! prove B1 (RFC0007.1) and the B2 latency bench come next.
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

use datafusion::arrow::datatypes::DataType;
use datafusion::common::ScalarValue;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{
    ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl,
};
use datafusion::error::DataFusionError;
use datafusion::prelude::{SessionContext, col, lit};
use ourios_core::tenant::TenantId;
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
    Storage { detail: String },
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TenantRequired => write!(f, "query has no tenant scope"),
            Self::InvalidQuery { detail } => write!(f, "invalid query: {detail}"),
            Self::Storage { detail } => write!(f, "storage read failed: {detail}"),
        }
    }
}

impl std::error::Error for QueryError {}

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
        // No directory ⇒ the tenant has written nothing ⇒ empty
        // result (not an error; a valid query over an empty range).
        if !tenant_dir.is_dir() {
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
                    col("time_unix_nano")
                        .gt_eq(lit(to_ts(start)?))
                        .and(col("time_unix_nano").lt(lit(to_ts(end)?))),
                )
                .map_err(storage_err)?;
        }
        if let Some(template_id) = request.template_id {
            df = df
                .filter(col("template_id").eq(lit(template_id)))
                .map_err(storage_err)?;
        }

        // `count()` aggregates without materialising the matched
        // rows' columns — so the heavy `attributes` / `params` /
        // `body` columns are never read for a count, and Parquet
        // projection pushdown applies. (`collect()` would buffer
        // every column of every match.)
        let rows = df.count().await.map_err(storage_err)? as u64;
        // QueryStats (row-group pruning / bytes) is slice 2 — it
        // comes from the ParquetExec metrics, not the row count.
        Ok(QueryResult {
            rows,
            stats: QueryStats::default(),
        })
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
        assert_eq!(
            QueryError::Storage {
                detail: "s3 timeout".into(),
            }
            .to_string(),
            "storage read failed: s3 timeout",
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
