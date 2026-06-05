//! `ourios-querier` — RFC 0007 querier (pillar #3, `DataFusion`).
//!
//! **Status: execution slice 3.** [`Querier::run`] executes a
//! minimal query — tenant scope + optional time range + optional
//! template-exact id + optional `severity_text` (the B1 `level='ERROR'`
//! filter) — against the RFC 0005 Parquet store via `DataFusion`,
//! returning a matching-row count **and the scan's row-group pruning
//! stats** ([`QueryStats`]). Tenant isolation
//! (RFC0007.5), B1 pruning (RFC0007.1 — a selective query provably
//! skips row groups via statistics) and B2 (RFC0007.2 — the work
//! the engine does tracks the result size, not the corpus size;
//! scanned row groups + bytes read stay flat as the corpus grows,
//! the growth absorbed by pruning) are live + tested.
//!
//! This crate is the **read path**: it runs the query against the
//! RFC 0005 store — scoped to the tenant's partition directory,
//! with `template_id` / `time_unix_nano` column filters (RFC 0005
//! §3.3/§3.6) — and returns results **without** leaking
//! `DataFusion` or SQL through the public API (hazard `CLAUDE.md`
//! §4.6). It reads the shipped RFC 0005 store; it needs neither
//! the WAL nor the receiver.
//!
//! Partition-level *time* pruning is live: a query with a time range
//! skips whole `year/month/day/hour` partitions whose span can't
//! overlap the window (`hour_partition_in_window`) before `DataFusion`
//! opens any footer, so scanned row groups stay flat as the corpus's
//! time span grows. It layers on the `time_unix_nano` column predicate
//! (still the row-level correctness authority); the pruning is
//! conservative and never drops an in-window partition.
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
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::ScalarValue;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{
    ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl,
};
use datafusion::error::DataFusionError;
use datafusion::functions_aggregate::expr_fn::count;
use datafusion::physical_plan::metrics::{MetricValue, MetricsSet};
use datafusion::physical_plan::{ExecutionPlan, collect};
use datafusion::prelude::{SessionContext, col, lit};
use ourios_core::tenant::TenantId;
use ourios_parquet::Manifest;
use ourios_parquet::columns;
use ourios_parquet::hour_partition_in_window;
use ourios_parquet::percent_encode_tenant;

/// A logs query to execute. **Throwaway surface** while the query
/// thesis (B1/B2) is unproven — per the maintainer decision, DSL
/// contracts (RFC 0002) are deferred until B1/B2 say the querier
/// is worth a stable language. So this carries only the minimal
/// predicates B1/B2 need: tenant scope, optional time bounds,
/// optional template-exact id, and an optional `severity_text`
/// equality (the B1 `level='ERROR'` filter) — exactly the RFC 0005
/// §3.3 pushdown keys.
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
    /// Optional `severity_text` equality filter — the B1 `level='ERROR'`
    /// query shape (RFC 0005 §3.2 `severity_text` column). The
    /// structured counterpart to the B1 reference's `grep ERROR`: rows
    /// whose severity is null or anything else don't match.
    pub severity_text: Option<String>,
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

/// Resolve the live data files a query must read under `dir` (a
/// tenant's partition root), honouring the RFC 0009 §3.4
/// per-partition manifest. Recursive because the data is nested
/// `year=/month=/day=/hour=/`.
///
/// For each partition directory: if it holds a `manifest.json`, the
/// manifest is authoritative and contributes exactly the files it
/// names (files present on disk but not listed — orphans awaiting GC,
/// or a writer's uncommitted `*.parquet.tmp` — are ignored). With no
/// manifest (every partition today, pre-compaction) it falls back to
/// all committed `*.parquet` in that directory; `*.parquet.tmp` has
/// extension `tmp`, so the poisoned-writer case contributes nothing.
///
/// An empty result means the tenant has nothing queryable. A missing
/// directory (`NotFound`) is empty; any *other* I/O error (permission
/// denied, transient failure) is propagated as [`QueryError::Storage`]
/// rather than silently masked as "no data" — a wrong zero-row answer
/// is worse than a surfaced error.
fn resolve_live_files(
    dir: &std::path::Path,
    window: Option<(u64, u64)>,
) -> Result<Vec<PathBuf>, QueryError> {
    let io_err = |op: &str, p: &std::path::Path, e: &std::io::Error| QueryError::Storage {
        detail: format!("{op} {}: {e}", p.display()),
    };
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(entries) => entries,
            // The dir (or a subdir, lost to a concurrent housekeeping
            // unlink) simply isn't there → not data, not an error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err("read_dir", &d, &e)),
        };
        let mut subdirs = Vec::new();
        let mut parquets = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_err("read_dir entry", &d, &e))?;
            let path = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => subdirs.push(path),
                Ok(_) if path.extension().is_some_and(|x| x == "parquet") => parquets.push(path),
                Ok(_) => {}
                Err(e) => return Err(io_err("file_type", &path, &e)),
            }
        }
        // Partition-level time pruning (RFC 0007): when the query has a
        // time range, skip a leaf partition whose `hour=HH` span can't
        // overlap it — so DataFusion never opens those footers. This is
        // a pure optimisation layered on the row-level time column
        // predicate (which stays the correctness authority);
        // `hour_partition_in_window` is conservative, never pruning a
        // path it can't prove out of range, so no in-window data is lost.
        let keep = window.is_none_or(|(start, end)| hour_partition_in_window(&d, start, end));
        if keep {
            match Manifest::read(&d).map_err(|e| QueryError::Storage {
                detail: format!("manifest in {}: {e}", d.display()),
            })? {
                // Manifest is authoritative: only its named files are live.
                Some(manifest) => {
                    files.extend(manifest.files.into_iter().map(|name| d.join(name)));
                }
                // No manifest → glob fallback for this partition.
                None => files.append(&mut parquets),
            }
        }
        stack.extend(subdirs);
    }
    Ok(files)
}

/// Pull the single aggregate count out of the result batches. A
/// `COUNT(*)` with no grouping always returns exactly one
/// `Int64` row; anything else means the plan/return-type changed
/// out from under us, so it's a surfaced error rather than a
/// silent (and wrong) zero.
fn count_value(batches: &[RecordBatch]) -> Result<u64, QueryError> {
    let bad = |detail: String| QueryError::Storage {
        detail: format!("count aggregate: {detail}"),
    };
    if batches.len() != 1 {
        return Err(bad(format!(
            "expected exactly 1 result batch, got {}",
            batches.len(),
        )));
    }
    let batch = &batches[0];
    if batch.num_rows() != 1 || batch.num_columns() != 1 {
        return Err(bad(format!(
            "expected exactly 1 row × 1 column, got {}×{}",
            batch.num_rows(),
            batch.num_columns(),
        )));
    }
    let col = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| bad("count column is not Int64".to_string()))?;
    if col.is_null(0) {
        return Err(bad("count is null".to_string()));
    }
    u64::try_from(col.value(0)).map_err(|_| bad("negative count".to_string()))
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
        fold_metrics(&metrics.aggregate_by_name(), stats);
    }
    for child in plan.children() {
        accumulate_scan_stats(child.as_ref(), stats);
    }
}

/// Fold the `DataFusion`-version-sensitive scan metrics — the
/// `row_groups_pruned_statistics` `PruningMetrics` and the
/// `bytes_scanned` `Count` — into `stats`. Pulled out of
/// [`accumulate_scan_stats`] so the metric-name / value-shape
/// matching is unit-testable without a live plan (the names are an
/// engine contract that can drift across `DataFusion` releases).
fn fold_metrics(metrics: &MetricsSet, stats: &mut QueryStats) {
    for metric in metrics.iter() {
        match metric.value() {
            // `row_groups_pruned_statistics` is a PruningMetrics
            // carrying both pruned (skipped via min/max stats) and
            // matched (read) row-group counts — exactly the B1
            // numerator + denominator.
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
        // Resolve the live file set under the tenant dir, honouring
        // the RFC 0009 §3.4 manifest (glob-fallback when absent). An
        // empty set ⇒ the tenant has nothing queryable ⇒ empty result
        // (not an error). Covers the missing-dir case and a partition
        // holding only `*.parquet.tmp` (a poisoned/crashed writer) —
        // where building a table over zero files would otherwise
        // error and wrongly fail the query.
        let live_files = resolve_live_files(&tenant_dir, request.time_range)?;
        if live_files.is_empty() {
            return Ok(QueryResult::default());
        }

        let ctx = SessionContext::new();
        // Tenant isolation (RFC0007.5 / §3.7) is enforced here, not
        // just assumed structural: every resolved file must
        // canonicalize to a path *under* the tenant's canonical
        // partition root. The manifest's entries are already validated
        // as partition-local names (`Manifest::validate`), but a
        // symlinked `*.parquet` could still resolve outside — this
        // `starts_with` check is the backstop that fails such a path
        // loudly rather than reading another tenant's data.
        let tenant_root = tenant_dir.canonicalize().map_err(|e| QueryError::Storage {
            detail: format!("canonicalize {}: {e}", tenant_dir.display()),
        })?;
        // One table path per *live* data file (RFC 0009 §3.4 — the
        // manifest, not a directory glob, decides the file set, so a
        // query never sees a compaction's superseded inputs). Each is
        // the canonical absolute path: DataFusion 53 treats an
        // absolute filesystem path as local and URI-encodes it
        // internally, so spaces / reserved characters are handled
        // without a hand-built `file://…` string. `year/month/day/hour`
        // stay path-only (not file columns) and the query filters only
        // data columns, so no table partition columns are declared.
        let mut seen = std::collections::HashSet::new();
        let mut urls = Vec::with_capacity(live_files.len());
        for file in &live_files {
            let abs = file.canonicalize().map_err(|e| QueryError::Storage {
                detail: format!("canonicalize {}: {e}", file.display()),
            })?;
            if !abs.starts_with(&tenant_root) {
                return Err(QueryError::Storage {
                    detail: format!(
                        "resolved file {} escapes tenant partition root {}",
                        abs.display(),
                        tenant_root.display(),
                    ),
                });
            }
            // De-duplicate so a manifest naming the same file twice
            // can't double-count its rows.
            if seen.insert(abs.clone()) {
                urls.push(ListingTableUrl::parse(abs.display().to_string()).map_err(storage_err)?);
            }
        }
        let options =
            ListingOptions::new(Arc::new(ParquetFormat::default())).with_file_extension(".parquet");
        // `infer_schema` over the multi-path set merges the files'
        // schemas, so additive schema drift across files reads as the
        // union (RFC0007.4 / RFC 0005 §3.9).
        let config = ListingTableConfig::new_with_multi_paths(urls)
            .with_listing_options(options)
            .infer_schema(&ctx.state())
            .await
            .map_err(storage_err)?;
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
        if let Some(severity_text) = &request.severity_text {
            // `severity_text` is OPTIONAL (RFC 0005 §3.2). If a tenant's
            // entire file set predates the column, the inferred union
            // schema omits it — and filtering an unknown column would
            // fail planning (surfacing as a generic Storage error). An
            // absent OPTIONAL column reads as all-NULL (RFC 0005 §3.9 /
            // RFC0007.4), so `severity_text = X` matches nothing: return
            // an empty result rather than erroring. (Per-file drift,
            // where *some* file has the column, is handled by DataFusion's
            // schema union — see tests/forward_compat.rs.)
            let has_severity = df
                .schema()
                .fields()
                .iter()
                .any(|f| f.name() == columns::SEVERITY_TEXT);
            if !has_severity {
                return Ok(QueryResult::default());
            }
            df = df
                .filter(col(columns::SEVERITY_TEXT).eq(lit(severity_text.as_str())))
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
        let rows = count_value(&batches)?;
        let stats = scan_stats(plan.as_ref());
        Ok(QueryResult { rows, stats })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Engine/SQL substrings that must never appear in an
    /// operator-facing `QueryError` message (RFC0007.3 / §4.6).
    /// Lowercase — callers scan against the lowercased message.
    /// None of these collide with the generic Storage message
    /// ("failed to read the log store").
    const ENGINE_LEAK_TOKENS: &[&str] = &[
        "datafusion",
        "arrow",
        "parquet",
        "sql",
        "select",
        "schema",
        "logical plan",
        "logicalplan",
        "physical",
        "recordbatch",
        "listingtable",
        "during planning",
    ];

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

    /// RFC0007.3 (string-level boundary) — a `Storage` error
    /// wrapping engine/SQL text scrubs it from the operator-facing
    /// `Display` while preserving it in `Debug` for logs. A
    /// denylist scan (not an exact-string match) so a future
    /// reword can't let a *new* engine token slip through (§4.6).
    #[test]
    fn rfc0007_3_storage_display_leaks_no_engine_tokens() {
        let leaky = "Arrow error: Parquet error: SELECT failed; schema \
                     mismatch in LogicalPlan (datafusion physical_plan)";
        let err = QueryError::Storage {
            detail: leaky.to_string(),
        };

        let shown = err.to_string().to_ascii_lowercase();
        for token in ENGINE_LEAK_TOKENS {
            assert!(
                !shown.contains(token),
                "Storage Display leaked engine token {token:?}: {shown:?}",
            );
        }
        // The detail is preserved for logs (Debug) — scrubbing is a
        // deliberate Display choice, not data loss.
        assert!(
            format!("{err:?}").contains("Parquet"),
            "Debug must preserve the engine detail for logs",
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

    /// Pin the metric-name / value-shape contract `fold_metrics`
    /// depends on: a `row_groups_pruned_statistics` `PruningMetrics`
    /// maps to pruned/matched row-group counts, `bytes_scanned`
    /// `Count` maps to `bytes_read`, and any other metric is ignored.
    /// If a `DataFusion` bump renames or reshapes these, this fails
    /// locally rather than letting the live test silently report
    /// always-zero stats.
    #[test]
    fn fold_metrics_extracts_pruning_and_bytes() {
        use std::borrow::Cow;

        use datafusion::physical_plan::metrics::{Count, Metric, PruningMetrics};

        let pruning = PruningMetrics::new();
        pruning.add_pruned(3);
        pruning.add_matched(2);
        let bytes = Count::new();
        bytes.add(4096);
        // A metric we don't track — must be left untouched.
        let other = Count::new();
        other.add(99);

        let mut set = MetricsSet::new();
        set.push(Arc::new(Metric::new(
            MetricValue::PruningMetrics {
                name: Cow::Borrowed("row_groups_pruned_statistics"),
                pruning_metrics: pruning,
            },
            None,
        )));
        set.push(Arc::new(Metric::new(
            MetricValue::Count {
                name: Cow::Borrowed("bytes_scanned"),
                count: bytes,
            },
            None,
        )));
        set.push(Arc::new(Metric::new(
            MetricValue::Count {
                name: Cow::Borrowed("output_rows"),
                count: other,
            },
            None,
        )));

        let mut stats = QueryStats::default();
        fold_metrics(&set, &mut stats);
        assert_eq!(stats.row_groups_pruned, 3);
        assert_eq!(stats.row_groups_scanned, 2);
        assert_eq!(stats.bytes_read, 4096);
    }

    // --- resolve_live_files (RFC 0009 §3.4 manifest / glob fallback) ---

    /// Create `<root>/data/tenant_id=a/year=2026/.../hour=10` and
    /// return `(tenant_dir, partition_dir)`.
    fn tenant_and_partition(root: &std::path::Path) -> (PathBuf, PathBuf) {
        let tenant = root.join("data/tenant_id=a");
        let partition = tenant.join("year=2026/month=04/day=02/hour=10");
        std::fs::create_dir_all(&partition).expect("mkdir partition");
        (tenant, partition)
    }

    #[test]
    fn resolve_missing_tenant_dir_is_empty() {
        // Arrange — a tenant directory that was never written.
        let tmp = tempfile::tempdir().expect("temp");
        let ghost = tmp.path().join("data/tenant_id=ghost");

        // Act
        let files = resolve_live_files(&ghost, None).expect("resolve");

        // Assert
        assert!(files.is_empty());
    }

    #[test]
    fn resolve_tmp_only_partition_is_empty() {
        // Arrange — a partition holding only an uncommitted `.tmp`.
        let tmp = tempfile::tempdir().expect("temp");
        let (tenant, partition) = tenant_and_partition(tmp.path());
        std::fs::write(partition.join("x.parquet.tmp"), b"partial").expect("write tmp");

        // Act
        let files = resolve_live_files(&tenant, None).expect("resolve");

        // Assert
        assert!(files.is_empty(), "uncommitted .tmp files are not live");
    }

    #[test]
    fn resolve_globs_committed_parquet_without_a_manifest() {
        // Arrange — two committed files, no manifest.
        let tmp = tempfile::tempdir().expect("temp");
        let (tenant, partition) = tenant_and_partition(tmp.path());
        std::fs::write(partition.join("a.parquet"), b"a").expect("write a");
        std::fs::write(partition.join("b.parquet"), b"b").expect("write b");

        // Act
        let files = resolve_live_files(&tenant, None).expect("resolve");

        // Assert
        assert_eq!(
            files.len(),
            2,
            "both committed files are live without a manifest"
        );
    }

    #[test]
    fn resolve_manifest_is_authoritative() {
        // Arrange — two files on disk, a manifest naming only one.
        let tmp = tempfile::tempdir().expect("temp");
        let (tenant, partition) = tenant_and_partition(tmp.path());
        std::fs::write(partition.join("a.parquet"), b"a").expect("write a");
        std::fs::write(partition.join("b.parquet"), b"b").expect("write b");
        let manifest = ourios_parquet::Manifest {
            generation: 1,
            files: vec!["a.parquet".to_string()],
        };
        std::fs::write(
            partition.join(ourios_parquet::MANIFEST_FILENAME),
            manifest.to_json().unwrap(),
        )
        .expect("write manifest");

        // Act
        let files = resolve_live_files(&tenant, None).expect("resolve");

        // Assert
        assert_eq!(files.len(), 1, "only the manifest's file is live");
        assert!(files[0].ends_with("a.parquet"));
    }

    #[test]
    fn resolve_malformed_manifest_is_a_storage_error() {
        // Arrange — a manifest that isn't valid JSON.
        let tmp = tempfile::tempdir().expect("temp");
        let (tenant, partition) = tenant_and_partition(tmp.path());
        std::fs::write(partition.join("a.parquet"), b"a").expect("write a");
        std::fs::write(
            partition.join(ourios_parquet::MANIFEST_FILENAME),
            b"not json",
        )
        .expect("write manifest");

        // Act
        let result = resolve_live_files(&tenant, None);

        // Assert
        assert!(matches!(result, Err(QueryError::Storage { .. })));
    }
}
