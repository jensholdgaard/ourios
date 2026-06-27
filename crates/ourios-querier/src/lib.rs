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
//! **Structured query surface.** [`QueryRequest`] is intentionally
//! minimal — just the predicates B1/B2 need. The logs DSL (RFC 0002,
//! now `specified`) lands in [`dsl`]: a Branch-B parser + a structured
//! surface that both compile to one IR in front of this layer. The DSL
//! is the stable user-facing contract; `QueryRequest` remains the
//! internal execution request it targets.

#![deny(unsafe_code)]

mod alias_store;
mod audit_scan;
mod compile;
mod drift;
pub mod dsl;
mod log_row;
mod row_decode;
mod template_registry;

pub use drift::{DriftResult, DriftRow};
pub use log_row::{LogBody, LogRow, render_log_body};
pub use template_registry::{TemplateRegistry, derive_template_registry};

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
use ourios_parquet::columns;
use ourios_parquet::hour_partition_in_window;
use ourios_parquet::percent_encode_tenant;
use ourios_parquet::{MANIFEST_FILENAME, Manifest, Store, StoreConfig};

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
    /// Optional `[start, end)` bounds over the **effective** timestamp
    /// (`effective_time_unix_nano`, falling back to `time_unix_nano` for
    /// pre-amendment files — RFC 0005 §3.2 / §3.9, amendment 2026-06-11).
    pub time_range: Option<(u64, u64)>,
    /// Optional template-exact filter (B2 — `template_id` equality).
    pub template_id: Option<u64>,
    /// Optional `severity_text` equality filter — the B1 `level='ERROR'`
    /// query shape (RFC 0005 §3.2 `severity_text` column). The
    /// structured counterpart to the B1 reference's `grep ERROR`: rows
    /// whose severity is null or anything else don't match.
    pub severity_text: Option<String>,
    /// Optional cap on returned rows (RFC 0017 §3.4). `Some(n)` populates
    /// `QueryResult.records` with up to `n` rendered [`LogRow`]s; `None` is
    /// count-only (`records` stays empty). The count (`rows`) is unaffected
    /// (always the full matching total), and `stats` continues to report the
    /// count/pruning scan only — the extra IO to materialise the (≤ `n`) record
    /// rows is **not** folded into `bytes_read`.
    pub limit: Option<usize>,
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

/// Result of a query: the matching-row count (`rows`) and the scan's pruning
/// [`QueryStats`] the B1/B2 gates assert on, plus — when the query carried a
/// `limit` — the rendered [`LogRow`] payload (`records`, RFC 0017 §3.3/§3.4).
/// All fields are Ourios-owned; no arrow `RecordBatch` / `DataFusion` type
/// crosses this boundary (§4.6 / RFC0017.7).
///
/// Marked `#[non_exhaustive]` so further additive fields stay non-breaking
/// (RFC 0017 §3.4 — the field addition itself is the accepted one-time break).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct QueryResult {
    /// Number of matching rows (the count). Unchanged by RFC 0017 — B1/B2 and
    /// existing tests read this. Free of arrow `RecordBatch` leakage (§4.6).
    pub rows: u64,
    pub stats: QueryStats,
    /// The returned rows, rendered (RFC 0017 §3.3/§3.4) — at most the query's
    /// `limit`. Empty when no `limit` was given (count-only). Each [`LogRow`]
    /// is fully Ourios-owned (no engine type — RFC0017.7).
    pub records: Vec<LogRow>,
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
            Self::Storage { .. } => write!(f, "failed to read storage"),
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

/// The S3 analog of [`resolve_live_files`]: resolve the live data-file **keys**
/// under the tenant's `prefix` through the [`Store`] seam (RFC 0019 §3.3),
/// honouring partition-level time pruning + the RFC 0009 §3.4 per-partition
/// manifest. Returns store-relative keys (the same key space `Store::get`/`put`
/// take), addressed as object-store URLs by the caller.
///
/// [`Store::list_blocking`] returns every key under `prefix` recursively, in
/// lexicographic order, segment-wise prefix-scoped to this tenant (RFC0019.5).
/// The keys are grouped by their partition directory (everything up to the last
/// `/`); for each partition: skip it when an `hour=HH` window prune proves it
/// out of range, then if it carries a `manifest.json` the manifest is
/// authoritative (only its named files are live, joined onto the partition key),
/// otherwise fall back to the partition's committed `*.parquet` keys
/// (`*.parquet.tmp` is excluded — it does not end in `.parquet`).
fn resolve_live_keys(
    store: &Store,
    prefix: &str,
    window: Option<(u64, u64)>,
) -> Result<Vec<String>, QueryError> {
    let keys = store
        .list_blocking(Some(prefix))
        .map_err(|e| QueryError::Storage {
            detail: format!("list data prefix {prefix}: {e}"),
        })?;
    // Group keys by partition directory (the key up to its last `/`).
    let mut by_partition: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    for key in &keys {
        let (dir, _) = key.rsplit_once('/').unwrap_or(("", key.as_str()));
        by_partition.entry(dir).or_default().push(key);
    }

    let mut live = Vec::new();
    for (dir, partition_keys) in by_partition {
        // Partition-level time pruning (RFC 0007), conservative — never prunes a
        // partition it can't prove out of range. `hour_partition_in_window`
        // parses the trailing Hive segments off a path, so build one from the
        // partition-dir key.
        if let Some((start, end)) = window
            && !hour_partition_in_window(&PathBuf::from(dir), start, end)
        {
            continue;
        }
        let manifest_key = format!("{dir}/{MANIFEST_FILENAME}");
        // Only read the manifest when its key is actually in the listing: the
        // partition is already enumerated, so a `read_with_etag` for an absent
        // manifest is a wasted (404) GET per un-compacted partition on S3.
        // Absent ⇒ no manifest ⇒ all committed files live (same as today's
        // glob fallback). `list_blocking` returns store-relative keys, so this
        // compares like-for-like.
        let manifest = if partition_keys.iter().any(|k| *k == manifest_key) {
            Manifest::read_with_etag(store, &manifest_key).map_err(|e| QueryError::Storage {
                detail: format!("manifest {manifest_key}: {e}"),
            })?
        } else {
            None
        };
        match manifest {
            // Manifest is authoritative: only its named files are live (joined
            // onto the partition key as `<dir>/<name>`).
            Some((manifest, _etag)) => {
                live.extend(
                    manifest
                        .files
                        .into_iter()
                        .map(|name| format!("{dir}/{name}")),
                );
            }
            // No manifest → glob fallback for this partition's committed files.
            None => live.extend(
                partition_keys
                    .into_iter()
                    .filter(|k| k.ends_with(".parquet"))
                    .map(ToOwned::to_owned),
            ),
        }
    }
    Ok(live)
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

/// A `time_unix_nano` literal: the RFC 0005 column is
/// `Timestamp(Nanosecond, "UTC")`, so the literal type must match exactly
/// or `DataFusion` rejects the comparison. Shared by the `QueryRequest`
/// path and the DSL compiler.
fn time_bound_scalar(v: u64) -> Result<ScalarValue, QueryError> {
    let ns = i64::try_from(v).map_err(|_| QueryError::InvalidQuery {
        detail: format!("time bound {v} exceeds i64 nanoseconds"),
    })?;
    Ok(ScalarValue::TimestampNanosecond(
        Some(ns),
        Some("UTC".into()),
    ))
}

/// True iff `column` is present in `df`'s (post-union) schema. An OPTIONAL
/// RFC 0005 column absent from every file in the set is omitted from the
/// inferred union schema; filtering on it would fail planning, so callers
/// short-circuit to an empty result instead (RFC 0005 §3.9 / RFC0007.4).
fn has_column(df: &datafusion::dataframe::DataFrame, column: &str) -> bool {
    df.schema().fields().iter().any(|f| f.name() == column)
}

/// The row-level time-window filter `[start, end)` over the **effective**
/// timestamp (RFC 0002 §6.2 / RFC 0005 §3.2, amendment 2026-06-11), with the
/// §3.9 rule-2 carve-out for files that predate the
/// `effective_time_unix_nano` column. Shared by the `QueryRequest` path and
/// the DSL compiler so both windows have identical semantics.
///
/// The carve-out is the explicit exception to the
/// absent-OPTIONAL-column ⇒ predicate-false convention (RFC0007.4): for
/// pre-amendment files the window applies `effective := time_unix_nano` —
/// exactly the pre-amendment behaviour — because compiling the window to
/// `false` would silently hide every old file from every query.
///
/// - Column absent from the (post-union) schema ⇒ every file predates the
///   amendment ⇒ filter `time_unix_nano` directly (prunable, as before).
/// - Column present ⇒ a *mixed* scan is still possible: `DataFusion` fills
///   the column with NULL for files that lack it, and NULL fails both window
///   comparisons — the forbidden silent-hiding outcome. Post-amendment
///   writers always populate the column (§3.2: NULL appears only in
///   pre-amendment files), so `IS NULL` identifies exactly the rows needing
///   the `time_unix_nano` fallback. The `OR` shape (rather than a
///   `coalesce`) keeps the predicate inside `DataFusion`'s pruning grammar:
///   min/max statistics prune the effective branch and null counts collapse
///   the fallback branch on post-amendment row groups — the B1 mechanism
///   (RFC 0005 §3.2 rule 3).
fn time_window_filter(
    df: &datafusion::dataframe::DataFrame,
    start: u64,
    end: u64,
) -> Result<datafusion::logical_expr::Expr, QueryError> {
    let lo = lit(time_bound_scalar(start)?);
    let hi = lit(time_bound_scalar(end)?);
    let ts = || col(columns::TIME_UNIX_NANO);
    let ts_window = ts().gt_eq(lo.clone()).and(ts().lt(hi.clone()));
    if !has_column(df, columns::EFFECTIVE_TIME_UNIX_NANO) {
        return Ok(ts_window);
    }
    let eff = || col(columns::EFFECTIVE_TIME_UNIX_NANO);
    let eff_window = eff().gt_eq(lo).and(eff().lt(hi));
    Ok(eff_window.or(eff().is_null().and(ts_window)))
}

/// Apply the [`QueryRequest`] predicate set as `DataFusion` filters. Returns
/// `Ok(None)` when a `severity_text` filter targets an absent OPTIONAL column
/// (provably empty — short-circuit).
fn apply_request_filters(
    mut df: datafusion::dataframe::DataFrame,
    request: &QueryRequest,
) -> Result<Option<datafusion::dataframe::DataFrame>, QueryError> {
    if let Some((start, end)) = request.time_range {
        let window = time_window_filter(&df, start, end)?;
        df = df.filter(window).map_err(storage_err)?;
    }
    if let Some(template_id) = request.template_id {
        df = df
            .filter(col(columns::TEMPLATE_ID).eq(lit(template_id)))
            .map_err(storage_err)?;
    }
    if let Some(severity_text) = &request.severity_text {
        // An absent OPTIONAL `severity_text` reads as all-NULL, so
        // `= X` matches nothing: empty result, not a planning error.
        if !has_column(&df, columns::SEVERITY_TEXT) {
            return Ok(None);
        }
        df = df
            .filter(col(columns::SEVERITY_TEXT).eq(lit(severity_text.as_str())))
            .map_err(storage_err)?;
    }
    Ok(Some(df))
}

/// The query engine. One per querier process; reads the RFC 0005
/// Parquet + audit store through the `ourios-parquet` [`Store`] seam,
/// so the same engine targets a local-filesystem store (dev / test /
/// the regression guard) or an S3-compatible bucket (production,
/// `CLAUDE.md` §3.6).
///
/// `local_root` is `Some` exactly when the backend is local: the bulk
/// `DataFusion` scan then addresses files by absolute local path
/// (unchanged from before RFC 0019), and `Manifest::read` /
/// `resolve_live_files` walk `std::fs`. When the backend is S3
/// (`local_root == None`) the scan registers the [`Store`]'s
/// `object_store` on the `SessionContext` and addresses tables by
/// object-store URL, and the live-file set is resolved through
/// [`Store::list_blocking`] (RFC 0019 §3.3).
#[derive(Debug, Clone)]
pub struct Querier {
    store: Store,
    /// `Some(root)` for the local backend (the `data/`-and-`audit/`
    /// parent directory), `None` for S3. Drives the hybrid scan branch.
    local_root: Option<PathBuf>,
}

/// The object-store URL scheme/authority the S3 scan registers its
/// [`Store`] under and addresses tables by — `ourios://store/<key>`.
/// The host carries no meaning beyond keying the `SessionContext`'s
/// object-store registry (the real bucket/prefix is inside the
/// registered store); using a private scheme keeps these synthetic URLs
/// from colliding with any real `s3://` / `file://` addressing.
const STORE_URL: &str = "ourios://store";

impl Querier {
    /// Create a querier reading the RFC 0005 store under the **local**
    /// `bucket_root` (the same root the `ourios-parquet` writer writes
    /// `data/tenant_id=…/year=…/…` under). The default constructor —
    /// the local backend is the test/dev default and the RFC 0019
    /// regression guard.
    ///
    /// # Panics
    ///
    /// Panics if a `Store::local` cannot be built for `bucket_root` —
    /// only on a resource failure constructing the trivial
    /// `LocalFileSystem` backend, which the prior `PathBuf`-only
    /// constructor could not hit. Use [`Self::from_store_config`] for
    /// fallible construction.
    pub fn new(bucket_root: impl Into<PathBuf>) -> Self {
        let root = bucket_root.into();
        // `LocalFileSystem::new_with_prefix` only fails on a resource
        // error (it does not require the path to exist), so this keeps
        // the infallible `new(path)` contract the 49 call sites rely on.
        let store = Store::local(&root).unwrap_or_else(|e| {
            panic!("local store for {}: {e}", root.display());
        });
        Self {
            store,
            local_root: Some(root),
        }
    }

    /// Create a querier from a resolved [`StoreConfig`] (RFC 0019 §3.2)
    /// — the S3-capable constructor the server wires the querier role
    /// through. A `Local` config is equivalent to [`Self::new`]; an
    /// `S3` config drives the object-store scan branch.
    ///
    /// # Errors
    ///
    /// [`QueryError::Storage`] if the backend cannot be constructed
    /// (e.g. an invalid S3 config — see [`StoreConfig::open`]).
    pub fn from_store_config(config: &StoreConfig) -> Result<Self, QueryError> {
        let store = config.open().map_err(|e| QueryError::Storage {
            detail: format!("open store: {e}"),
        })?;
        let local_root = match config {
            StoreConfig::Local(root) => Some(root.clone()),
            StoreConfig::S3(_) => None,
        };
        Ok(Self { store, local_root })
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
        let tenant = request.tenant.clone();
        let window = request.time_range;
        let row_limit = request.limit;
        self.execute(&tenant, window, row_limit, |df| {
            apply_request_filters(df, &request)
        })
        .await
    }

    /// Compile a parsed DSL [`Query`](dsl::Query) IR (RFC 0002) to the
    /// `DataFusion` execution layer and run it against the tenant's RFC 0005
    /// store, returning the matching row count + pruning stats — without
    /// leaking `DataFusion`/arrow/SQL (hazard `CLAUDE.md` §4.6 / RFC0002.3).
    ///
    /// `now_unix_nano` is the wall-clock reference the relative `range(...)`
    /// bounds (`-1h`, `now`) and the default window resolve against; the
    /// caller supplies it so compilation is deterministic and testable.
    /// `default_window_nanos` is the tenant's default look-back: a query with
    /// no `range(...)` stage compiles with the time filter
    /// `[now - default_window_nanos, now]` (RFC 0002 §4 P5 — **never** an
    /// unbounded scan).
    ///
    /// `alias_map` selects where the RFC 0001 §6.7 alias projection comes
    /// from. `None` — the production default — derives the requesting
    /// tenant's map from its audit stream at compile time per RFC 0005
    /// §3.7.1 (the audit stream is the alias store in v1; the scan is
    /// skipped entirely when the query has no `resolves_to`).
    /// `Some(map)` injects a caller-held projection instead — the
    /// test/operator override, bypassing storage. Either way,
    /// `resolves_to(n)` expands through
    /// [`AliasMap::resolves`](ourios_core::alias::AliasMap::resolves) for
    /// `tenant`, so a `template_id` an operator aliased matches its whole
    /// equivalence class; an id in no class resolves to `{id}` — a singleton
    /// `template_id IN (n)`, behaviorally identical to a bare
    /// `template_id == n`.
    ///
    /// # Errors
    ///
    /// [`QueryError::InvalidQuery`] if a literal can't be resolved (a malformed
    /// duration/timestamp the parser admitted lexically); otherwise see
    /// [`QueryError`].
    pub async fn run_query(
        &self,
        query: &dsl::Query,
        tenant: &TenantId,
        now_unix_nano: u64,
        default_window_nanos: u64,
        alias_map: Option<&ourios_core::alias::AliasMap>,
    ) -> Result<QueryResult, QueryError> {
        // Error precedence: stage-support and window/limit validation
        // runs before the alias-map derivation below, so those query
        // errors surface without paying the audit-tree IO (or its
        // Storage errors). Predicate compilation needs the map, so its
        // errors necessarily come after. `compile` re-runs the same
        // pure validation internally — one source of truth, negligible
        // cost.
        compile::validate(query, now_unix_nano, default_window_nanos)?;
        let derived;
        let map = match alias_map {
            Some(map) => map,
            None if compile::uses_resolves_to(&query.predicate) => {
                // Offload the blocking audit derivation (S3 `get_blocking` /
                // the local `std::fs` reads) off the runtime worker, mirroring
                // `run_drift` — the derivation is deeply sync, so clone the
                // cheap handles into the blocking task.
                derived = self
                    .spawn_blocking_audit({
                        let store = self.store.clone();
                        let local_root = self.local_root.clone();
                        let tenant = tenant.clone();
                        move || {
                            alias_store::derive_alias_map(&store, local_root.as_deref(), &tenant)
                        }
                    })
                    .await?;
                &derived
            }
            // No `resolves_to` ⇒ the map is never consulted; an empty
            // projection avoids the audit-tree scan.
            None => {
                derived = ourios_core::alias::AliasMap::new();
                &derived
            }
        };
        let plan = compile::compile(query, tenant, now_unix_nano, default_window_nanos, map)?;
        // The DSL `limit` (RFC 0002) doubles as the RFC 0017 row cap; read it
        // before `plan` moves into the filter closure.
        let row_limit = plan.limit;
        self.execute(tenant, Some(plan.window), row_limit, move |df| {
            compile::apply(df, plan)
        })
        .await
    }

    /// Execute a RFC 0010 `drift` query against the tenant's RFC 0005 `audit/`
    /// stream and return the per-template [`DriftRow`]s + pruning stats —
    /// without leaking `DataFusion`/arrow/SQL (hazard `CLAUDE.md` §4.6 /
    /// RFC0010.8).
    ///
    /// Drift is the audit-stream sibling of [`run_query`](Self::run_query): it
    /// scans `audit/tenant_id=<tenant>/`, filters to the widening /
    /// type-expansion events in the half-open window `[from, to)`, and folds
    /// them per `template_id` (RFC 0010 §6.3). Tenant isolation is a partition
    /// prune on the `audit/tenant_id=…` Hive root (RFC0010.4 / §3.7); a drift
    /// query with no tenant is unrepresentable (the `tenant` argument is
    /// required). An empty window or a tenant with no qualifying events yields
    /// an empty [`DriftResult`], never an error (RFC0010.5).
    ///
    /// `now_unix_nano` is the wall-clock reference the relative `from`/`to`
    /// bounds (`-7d`, `now`) resolve against; the caller supplies it so
    /// execution is deterministic and testable.
    ///
    /// # Errors
    ///
    /// See [`QueryError`].
    pub async fn run_drift(
        &self,
        query: &dsl::DriftQuery,
        tenant: &TenantId,
        now_unix_nano: u64,
    ) -> Result<DriftResult, QueryError> {
        drift::run_drift(
            &self.store,
            self.local_root.as_deref(),
            query,
            tenant,
            now_unix_nano,
        )
        .await
    }

    /// Shared scan path for both [`run`](Self::run) and
    /// [`run_query`](Self::run_query): resolve the tenant's live file set
    /// (honouring partition-level time pruning + the RFC 0009 §3.4 manifest),
    /// build the listing table with tenant isolation enforced, apply the
    /// caller's filter, and count via an aggregate so the heavy columns are
    /// never materialised. `partition_window` drives the directory-level time
    /// pruning only; row-level correctness stays with the filter.
    async fn execute<F>(
        &self,
        tenant: &TenantId,
        partition_window: Option<(u64, u64)>,
        // RFC 0017 §3.4 — when `Some(n)`, collect up to `n` matching rows into
        // `QueryResult.records` (rendered via the read-time registry); `None`
        // is count-only. The count + stats are taken the same way regardless.
        row_limit: Option<usize>,
        build_filter: F,
    ) -> Result<QueryResult, QueryError>
    where
        // `Ok(None)` ⇒ the filter is provably empty (an absent OPTIONAL
        // column, RFC 0005 §3.9), so the query short-circuits to an empty
        // result rather than planning a scan that matches nothing.
        F: FnOnce(
            datafusion::dataframe::DataFrame,
        ) -> Result<Option<datafusion::dataframe::DataFrame>, QueryError>,
    {
        let enc = percent_encode_tenant(tenant.as_str());
        let data_prefix = format!("data/tenant_id={enc}");

        let ctx = SessionContext::new();
        // Resolve the live file set under the tenant's `data/` prefix,
        // honouring the RFC 0009 §3.4 manifest (glob-fallback when absent),
        // and produce the per-file table URLs (local absolute path, or
        // object-store URL on S3). An empty set ⇒ the tenant has nothing
        // queryable ⇒ empty result (not an error). Covers the missing-dir
        // case and a partition holding only `*.parquet.tmp` (a poisoned /
        // crashed writer) — where building a table over zero files would
        // otherwise error and wrongly fail the query.
        let urls = self.resolve_data_urls(&ctx, &data_prefix, partition_window)?;
        if urls.is_empty() {
            return Ok(QueryResult::default());
        }

        // Read Parquet string/binary columns as `Utf8` / `Binary`, not the
        // `Utf8View` / `BinaryView` DataFusion forces by default — the RFC 0017
        // row decoder (`row_decode`) downcasts to the non-view array types, and
        // the count/filter path is indifferent to which representation it gets.
        // Set on the `ParquetFormat` itself (a bare `ParquetFormat::default()`
        // ignores the session config's parquet options).
        let mut parquet_options = datafusion::common::config::TableParquetOptions::default();
        parquet_options.global.schema_force_view_types = false;
        let parquet_format = ParquetFormat::default().with_options(parquet_options);
        let options = ListingOptions::new(Arc::new(parquet_format)).with_file_extension(".parquet");
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

        let base = ctx.table("logs").await.map_err(storage_err)?;
        // A provably-empty filter (absent OPTIONAL column) ⇒ no scan.
        let Some(df) = build_filter(base)? else {
            return Ok(QueryResult::default());
        };

        // Count via an aggregate so the heavy `attributes` /
        // `params` / `body` columns are never materialised
        // (projection pushdown). We build + execute the physical
        // plan ourselves (rather than `df.count()`) so we can read
        // the scan's pruning metrics off the retained plan. Clone
        // `df` first so the (RFC 0017) row collection below reads the
        // same filtered frame.
        let counted = df
            .clone()
            .aggregate(vec![], vec![count(lit(1_i64)).alias("n")])
            .map_err(storage_err)?;
        let plan = counted.create_physical_plan().await.map_err(storage_err)?;
        let batches = collect(Arc::clone(&plan), ctx.task_ctx())
            .await
            .map_err(storage_err)?;
        let rows = count_value(&batches)?;
        let stats = scan_stats(plan.as_ref());

        // RFC 0017 §3.3/§3.4 — when a `row_limit` is requested, materialise the
        // matching rows (the same filtered frame, capped at the limit), decode
        // them to `MinedRecord`s, and render each into a `LogRow` via the
        // read-time template registry. Heavy columns are only materialised for
        // these (≤ limit) rows. `None` ⇒ count-only (records stays empty).
        let records = match row_limit {
            Some(n) => self.collect_records(df, n, tenant).await?,
            None => Vec::new(),
        };
        Ok(QueryResult {
            rows,
            stats,
            records,
        })
    }

    /// Materialise up to `limit` matching rows from the filtered `df`, decode
    /// them, and render each into a [`LogRow`] (RFC 0017 §3.3). The template
    /// registry is derived once (from the tenant's audit stream) only when
    /// there are rows to render.
    async fn collect_records(
        &self,
        df: datafusion::dataframe::DataFrame,
        limit: usize,
        tenant: &TenantId,
    ) -> Result<Vec<LogRow>, QueryError> {
        let limited = df.limit(0, Some(limit)).map_err(storage_err)?;
        let batches = limited.collect().await.map_err(storage_err)?;
        let mined = row_decode::batches_to_mined_records(&batches)?;
        if mined.is_empty() {
            return Ok(Vec::new());
        }
        // Row-level tenant backstop (`CLAUDE.md` §3.7 / RFC 0005 §3.9
        // row-vs-path): the scan is scoped to the tenant's partition prefix
        // (and, on the local backend, canonical-path-checked under it), but a
        // misplaced / corrupt Parquet file could still carry a row for another
        // tenant. Returning row *contents*, refuse to render such a row rather
        // than expose another tenant's data — fail loudly, mirroring the
        // alias-map / template-registry derivations.
        for record in &mined {
            if record.tenant_id != *tenant {
                return Err(QueryError::Storage {
                    detail: format!(
                        "a returned row carries tenant {} under tenant {}'s partition root",
                        record.tenant_id.as_str(),
                        tenant.as_str(),
                    ),
                });
            }
        }
        // Offload the blocking registry derivation (S3 `get_blocking` / local
        // `std::fs` reads) off the runtime worker, like the alias derivation.
        let registry = self
            .spawn_blocking_audit({
                let store = self.store.clone();
                let local_root = self.local_root.clone();
                let tenant = tenant.clone();
                move || derive_template_registry(&store, local_root.as_deref(), &tenant)
            })
            .await?;
        Ok(mined
            .iter()
            .map(|record| LogRow::from_record(record, &registry))
            .collect())
    }

    /// Run a blocking audit derivation (`derive_alias_map` /
    /// `derive_template_registry`) on the tokio blocking pool so the async query
    /// path doesn't tie up a runtime worker on the S3 `get_blocking` (or local
    /// `std::fs`) reads — the same offload `run_drift` applies to the listing.
    /// The closure owns its captured `Store` / `local_root` / `TenantId` clones
    /// so it satisfies the `'static + Send` bound.
    async fn spawn_blocking_audit<T, F>(&self, derive: F) -> Result<T, QueryError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, QueryError> + Send + 'static,
    {
        tokio::task::spawn_blocking(derive)
            .await
            .map_err(|e| QueryError::Storage {
                detail: format!("audit derivation task: {e}"),
            })?
    }

    /// Resolve the live data files under the tenant's `data/` prefix and turn
    /// them into the `DataFusion` table URLs for the hybrid scan (RFC 0019 §3.3):
    ///
    /// - **Local backend** (`local_root == Some`): walk `std::fs` under
    ///   `<root>/<prefix>` honouring the RFC 0009 §3.4 manifest, then address
    ///   each file by its absolute local path — byte-for-byte the pre-RFC-0019
    ///   read path, with the canonical-path tenant-isolation backstop intact.
    /// - **S3 backend** (`local_root == None`): list the keys under `prefix`
    ///   through [`Store::list_blocking`] (segment-wise prefix-scoped, the
    ///   RFC0019.5 tenant guarantee), resolve the per-partition manifest through
    ///   the [`Store`], register the store on `ctx`, and address each key by the
    ///   `ourios://store/<key>` object-store URL.
    fn resolve_data_urls(
        &self,
        ctx: &SessionContext,
        prefix: &str,
        window: Option<(u64, u64)>,
    ) -> Result<Vec<ListingTableUrl>, QueryError> {
        if let Some(root) = &self.local_root {
            let tenant_dir = root.join(prefix);
            let live_files = resolve_live_files(&tenant_dir, window)?;
            local_file_urls(&tenant_dir, &live_files)
        } else {
            let live_keys = resolve_live_keys(&self.store, prefix, window)?;
            object_store_urls(ctx, &self.store, &live_keys)
        }
    }
}

/// Build the `DataFusion` table URLs for the **local** backend: every resolved
/// file must canonicalize *under* the tenant's canonical partition root before
/// it is addressed, the tenant-isolation backstop (RFC0007.5 / §3.7). The
/// manifest's entries are already validated as partition-local names
/// (`Manifest::validate`), but a symlinked `*.parquet` could still resolve
/// outside — this `starts_with` check fails such a path loudly rather than
/// reading another tenant's data. Canonical paths are de-duplicated so a
/// manifest naming the same file twice can't double-count its rows.
///
/// Each URL is the canonical absolute path: `DataFusion` 53 treats an absolute
/// filesystem path as local and URI-encodes it internally, so spaces / reserved
/// characters are handled without a hand-built `file://…` string.
/// `year/month/day/hour` stay path-only (not file columns) and the query
/// filters only data columns, so no table partition columns are declared.
fn local_file_urls(
    tenant_dir: &std::path::Path,
    live_files: &[PathBuf],
) -> Result<Vec<ListingTableUrl>, QueryError> {
    if live_files.is_empty() {
        return Ok(Vec::new());
    }
    let tenant_root = tenant_dir.canonicalize().map_err(|e| QueryError::Storage {
        detail: format!("canonicalize {}: {e}", tenant_dir.display()),
    })?;
    let mut seen = std::collections::HashSet::new();
    let mut urls = Vec::with_capacity(live_files.len());
    for file in live_files {
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
        if seen.insert(abs.clone()) {
            urls.push(ListingTableUrl::parse(abs.display().to_string()).map_err(storage_err)?);
        }
    }
    Ok(urls)
}

/// Build the `DataFusion` table URLs for the **S3** backend: register the
/// [`Store`]'s `object_store` on `ctx` under the [`STORE_URL`] scheme/authority
/// and address each store-relative key by an `ourios://store/<key>` URL
/// (RFC 0019 §3.3). Tenant isolation is the segment-wise prefix scope of the
/// listing that produced `keys` (RFC0019.5) — the object key space has no
/// symlinks, so there is no canonical-path escape to backstop here (the §3.7
/// row-level backstop in the consumers stays). De-duplicates keys so a manifest
/// naming the same file twice can't double-count its rows.
fn object_store_urls(
    ctx: &SessionContext,
    store: &Store,
    keys: &[String],
) -> Result<Vec<ListingTableUrl>, QueryError> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let store_url = datafusion::execution::object_store::ObjectStoreUrl::parse(STORE_URL)
        .map_err(storage_err)?;
    ctx.register_object_store(store_url.as_ref(), store.object_store());
    // `Store::object_store()` is the RAW backend (prefix NOT applied), whereas
    // `list_blocking`/`get_blocking` operate in the store-relative key space
    // under `Store::prefix()` (the `OURIOS_S3_PREFIX` root). So the URLs handed
    // to DataFusion — which reads the raw backend directly — must carry the FULL
    // key: the store prefix segments followed by the relative key. With no
    // prefix (the local default) this is just the key.
    let prefix: Vec<String> = store
        .prefix()
        .parts()
        .map(|p| p.as_ref().to_owned())
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut urls = Vec::with_capacity(keys.len());
    for key in keys {
        if seen.insert(key.clone()) {
            urls.push(
                ListingTableUrl::parse(object_store_url_for_key(&prefix, key))
                    .map_err(storage_err)?,
            );
        }
    }
    Ok(urls)
}

/// Build the `ourios://store/<prefix>/<key>` URL for a store-relative `key`
/// under the store's `prefix` segments, percent-encoding each path segment.
///
/// Two reasons the full path matters:
/// - **Prefix** — `Store::object_store()` is the un-scoped raw backend, so the
///   URL must carry the store's `OURIOS_S3_PREFIX` root (`prefix`) ahead of the
///   relative key, or `DataFusion` would address an un-prefixed (not-found) path.
/// - **Encoding** — `ListingTableUrl::parse` URL-**decodes** the path, and a
///   key carries literal `%` (the partition dir is `tenant_id=<percent-encoded>`,
///   e.g. `tenant_id=tenant%20ABC`), so an un-encoded segment would be
///   double-decoded into a wrong path. Encoding every non-unreserved byte per
///   segment (and re-joining with `/`) makes the parse round-trip back to the
///   exact full key. `NON_ALPHANUMERIC` over-encodes harmlessly (`=`, `-`, `.`
///   round-trip the same); the only structural byte we keep is the `/`
///   separator, preserved by the per-segment split.
fn object_store_url_for_key(prefix: &[String], key: &str) -> String {
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    let encode = |segment: &str| utf8_percent_encode(segment, NON_ALPHANUMERIC).to_string();
    let encoded = prefix
        .iter()
        .map(|p| encode(p))
        .chain(key.split('/').map(encode))
        .collect::<Vec<_>>()
        .join("/");
    format!("{STORE_URL}/{encoded}")
}

/// Build the `DataFusion` table URLs for an **audit** scan (the drift query's
/// `ListingTable` over the audit stream) from a resolved [`AuditFiles`],
/// branching the same way as the bulk log scan (RFC 0019 §3.3):
///
/// - **Local** ([`AuditFiles::Local`]): the paths are already the
///   canonicalizing `std::fs` walk's output — absolute, canonical, deduped, and
///   tenant-isolation-checked (the symlink-escape / tenant-root backstops live
///   in [`audit_scan`]). Address each by its absolute local path.
/// - **S3** ([`AuditFiles::Remote`]): register the store on `ctx` and address
///   each key by its percent-encoded `ourios://store/<key>` object-store URL;
///   tenant isolation is the segment-wise prefix scope (RFC0019.5).
pub(crate) fn audit_table_urls(
    ctx: &SessionContext,
    store: &Store,
    files: &audit_scan::AuditFiles,
) -> Result<Vec<ListingTableUrl>, QueryError> {
    match files {
        // The walk already produced absolute canonical paths, so address them
        // directly — no `root.join`, no CWD-relative path.
        audit_scan::AuditFiles::Local(paths) => paths
            .iter()
            .map(|path| ListingTableUrl::parse(path.display().to_string()).map_err(storage_err))
            .collect(),
        audit_scan::AuditFiles::Remote(keys) => object_store_urls(ctx, store, keys),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The S3 object-store URL for a key prepends the store prefix and
    /// percent-encodes every segment, so `ListingTableUrl::parse`'s URL-decode
    /// round-trips back to the **full** key the raw backend expects
    /// (`OURIOS_S3_PREFIX` + the store-relative key). The partition dir carries
    /// a literal `%` (`tenant_id=tenant%20ABC`) that must survive the parse.
    #[test]
    fn object_store_url_prepends_prefix_and_round_trips() {
        let prefix = vec!["ourios".to_string()];
        let key = "data/tenant_id=tenant%20ABC/year=2026/h.parquet";
        let url = object_store_url_for_key(&prefix, key);
        // The parsed URL's object-store path must decode back to prefix + key,
        // not double-decode the literal `%20` into a space.
        let parsed = ListingTableUrl::parse(&url).expect("parse url");
        let decoded = percent_encoding::percent_decode_str(parsed.as_ref())
            .decode_utf8()
            .expect("utf8");
        assert!(
            decoded.ends_with("ourios/data/tenant_id=tenant%20ABC/year=2026/h.parquet"),
            "decoded URL must carry the full prefixed key verbatim: {decoded}",
        );
    }

    /// With no store prefix (the local default), the URL is just the key —
    /// the prefix prepend is a no-op.
    #[test]
    fn object_store_url_with_no_prefix_is_just_the_key() {
        let url = object_store_url_for_key(&[], "data/tenant_id=t/h.parquet");
        let parsed = ListingTableUrl::parse(&url).expect("parse url");
        let decoded = percent_encoding::percent_decode_str(parsed.as_ref())
            .decode_utf8()
            .expect("utf8");
        assert!(
            decoded.ends_with("data/tenant_id=t/h.parquet"),
            "no-prefix URL is the bare key: {decoded}",
        );
    }

    /// Engine/SQL substrings that must never appear in an
    /// operator-facing `QueryError` message (RFC0007.3 / §4.6).
    /// Lowercase — callers scan against the lowercased message.
    /// None of these collide with the generic Storage message
    /// ("failed to read storage").
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
            "failed to read storage",
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
