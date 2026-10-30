//! `ourios-querier` — RFC 0007 querier (pillar #3, `DataFusion`).
//!
//! This crate is the **read path**: it runs queries against the
//! RFC 0005 Parquet store — scoped to the tenant's partition directory,
//! with `template_id` / `time_unix_nano` column filters (RFC 0005
//! §3.3/§3.6) — and returns results **without** leaking `DataFusion`
//! or SQL through the public API (hazard `AGENTS.md` §4.6). It needs
//! neither the WAL nor the receiver.
//!
//! - **Logs DSL** (RFC 0002) — [`dsl`] holds the string parser and the
//!   structured JSON surface, both producing one IR;
//!   [`Querier::run_query`] compiles that IR to `DataFusion` and runs it,
//!   returning the match count, grouped `count [by …]` aggregates, and
//!   row-group pruning stats ([`QueryStats`]). Tenant isolation
//!   (RFC0007.5), B1 pruning (RFC0007.1 — a selective query provably
//!   skips row groups via statistics) and B2 (RFC0007.2 — scanned row
//!   groups + bytes read track the result size, not the corpus size)
//!   are tested here.
//! - **Rendering** (RFC 0017) — returned rows come back as [`LogRow`]s.
//!   String bodies render from the tenant's template registry; structured
//!   bodies decode from their stored canonical JSON. The
//!   registry and the RFC 0001 alias map are both derived from the audit
//!   stream ([`derive_template_registry`], [`derive_alias_map`]).
//! - **Drift** (RFC 0010) — [`Querier::run_drift`] folds the audit
//!   stream's widening / type-expansion events per template.
//! - **Visibility** (RFC 0047) — [`visibility`] applies a caller-made
//!   authorization decision as a plan-time rewrite: an extra predicate
//!   over a promoted column, or column masking on returned rows.
//!
//! Partition-level *time* pruning: a query with a time range skips
//! whole `year/month/day/hour` partitions whose span can't overlap the
//! window (`hour_partition_in_window`) before `DataFusion` opens any
//! footer, so scanned row groups stay flat as the corpus's time span
//! grows. It layers on the `time_unix_nano` column predicate (still the
//! row-level correctness authority); the pruning is conservative and
//! never drops an in-window partition.
//!
//! The DSL is the stable user-facing contract; [`QueryRequest`] is the
//! internal execution request it targets, and [`Querier::run`] runs one
//! directly.

#![deny(unsafe_code)]

mod alias_store;
mod audit_fetch;
mod audit_scan;
mod body_match;
mod drift;
pub mod dsl;
mod fold_state;
mod local_publish;
mod log_row;
mod map_flight;
mod map_refresh;
mod phase;
mod plan;
mod schema_adapt;
mod template_map;
#[cfg(feature = "testing")]
mod template_map_hooks;
mod template_registry;
pub mod visibility;

pub use alias_store::derive_alias_map;
pub use audit_scan::StoreRef;
pub use drift::{DriftResult, DriftRow};
pub use fold_state::{FoldState, INCREMENTAL_DEPTH_CAP};
pub use log_row::{LogBody, LogRow, render_log_body};
pub use map_refresh::TemplateMapRefresher;
#[cfg(feature = "testing")]
#[doc(hidden)]
pub use map_refresh::testing::PassLog;
pub use template_map::{
    ArtifactRead, CacheOutcome, MissReason, PublishOutcome, TEMPLATE_MAP_FILENAME, TemplateMap,
    derive_template_map, load_or_derive,
};
pub use template_registry::{TemplateRegistry, derive_template_registry};
pub use visibility::{ScopedIds, SelfMatch, Visibility};

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
use datafusion::prelude::{SessionContext, col, lit};
use ourios_core::tenant::TenantId;
use ourios_parquet::columns;
use ourios_parquet::hour_partition_in_window;
use ourios_parquet::percent_encode_tenant;
use ourios_parquet::{MANIFEST_FILENAME, Manifest, Store, StoreConfig};

mod api;
mod decode;
mod exec;
mod fetch_count;
mod file_set;
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzzing;
#[cfg(test)]
mod stale_resolution_tests;
mod stats;
#[cfg(test)]
mod test_support;

// Scope glue for the split (epic #745 wave 1): every pre-split
// `crate::X` path — the sibling modules' imports included — resolves
// unchanged through these re-exports.
pub use api::{AggregateGroup, QueryError, QueryOptions, QueryRequest, QueryResult, QueryStats};
#[allow(unused_imports, clippy::wildcard_imports)]
pub(crate) use decode::*;
#[allow(unused_imports, clippy::wildcard_imports)]
pub(crate) use exec::*;
#[allow(unused_imports, clippy::wildcard_imports)]
pub(crate) use file_set::*;
#[allow(unused_imports, clippy::wildcard_imports)]
pub(crate) use stats::*;

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
fn has_column(schema: &datafusion::common::DFSchema, column: &str) -> bool {
    schema.fields().iter().any(|f| f.name() == column)
}

/// The (post-union) type of `column`, when present. Since RFC 0042 the
/// promoted columns' union type is the declared class's type
/// ([`schema_adapt::merge_scanned_schemas`]), so this is how the DSL
/// compiler learns a key's class.
fn column_type(
    schema: &datafusion::common::DFSchema,
    column: &str,
) -> Option<datafusion::arrow::datatypes::DataType> {
    schema
        .fields()
        .iter()
        .find(|f| f.name() == column)
        .map(|f| f.data_type().clone())
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
    if !has_column(df.schema(), columns::EFFECTIVE_TIME_UNIX_NANO) {
        return Ok(ts_window);
    }
    let eff = || col(columns::EFFECTIVE_TIME_UNIX_NANO);
    let eff_window = eff().gt_eq(lo).and(eff().lt(hi));
    Ok(eff_window.or(eff().is_null().and(ts_window)))
}

/// Lower a validated aggregation stage over the filtered frame: the grouped
/// `Aggregate` node `execute_aggregate` runs (`n`, plus `v` for a scalar
/// stage).
fn aggregate_frame(
    df: datafusion::dataframe::DataFrame,
    agg: &plan::Aggregate,
    tenant: &TenantId,
) -> Result<datafusion::dataframe::DataFrame, QueryError> {
    // Row-level tenant backstop (`AGENTS.md` §3.7), mirroring the drift
    // aggregation: the scan is scoped to the tenant's partition prefix,
    // but an aggregation returns group *values* and has no per-row
    // check like `collect_records`' — a misplaced row under the prefix
    // must neither skew the counts nor leak a foreign param value into
    // a group key.
    let df = df
        .filter(col(columns::TENANT_ID).eq(lit(tenant.as_str())))
        .map_err(storage_err)?;
    let group_exprs = plan::group_exprs(&agg.by, df.schema())?;
    // Always compute COUNT(*); add the scalar aggregate (sum/min/max/avg of
    // the CAST-to-Float64 promoted column) when the stage carries one.
    let mut aggr_exprs = vec![count(lit(1_i64)).alias(plan::COUNT_COLUMN)];
    if let Some((func, path)) = &agg.scalar {
        aggr_exprs.push(plan::scalar_agg_expr(*func, path, df.schema())?.alias(plan::VALUE_COLUMN));
    }
    df.aggregate(group_exprs, aggr_exprs).map_err(storage_err)
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
        if !has_column(df.schema(), columns::SEVERITY_TEXT) {
            return Ok(None);
        }
        df = df
            .filter(col(columns::SEVERITY_TEXT).eq(lit(severity_text.as_str())))
            .map_err(storage_err)?;
    }
    Ok(Some(df))
}

/// Which backend a [`Querier`] reads (RFC 0019 §3.3). The local variant holds
/// only the path: building a `Store::local` **canonicalizes** the prefix and so
/// fails on a not-yet-created bucket, which would break the infallible
/// [`Querier::new`] contract and the long-standing "query a fresh bucket ⇒
/// empty result, never an error" behaviour. The local read paths walk `std::fs`
/// directly and tolerate `NotFound`, so the local branch never needs a
/// constructed `Store` for I/O — only the S3 branch holds an eager [`Store`].
#[derive(Debug, Clone)]
pub(crate) enum Backend {
    /// Local-filesystem store rooted at the path (the `data/`-and-`audit/`
    /// parent). Held as a path so construction is infallible and a missing dir
    /// is tolerated at query time.
    Local(PathBuf),
    /// S3 / S3-compatible store, constructed eagerly (the read paths address it
    /// via `Store::list_blocking` / `object_store`).
    Remote(Store),
}

impl Backend {
    /// Borrow as the [`StoreRef`] selector the reader-side derivations
    /// (`audit_scan` / alias / registry / drift) take — the hybrid-scan branch
    /// is then a single exhaustive `match` with no "can't happen" arm.
    pub(crate) fn store_ref(&self) -> StoreRef<'_> {
        match self {
            Self::Local(root) => StoreRef::Local(root),
            Self::Remote(store) => StoreRef::Remote(store),
        }
    }

    /// This backend, its store calls attributed to `caller`.
    pub(crate) fn with_caller(self, caller: ourios_parquet::Caller) -> Self {
        match self {
            Self::Local(root) => Self::Local(root),
            Self::Remote(store) => Self::Remote(store.with_caller(caller)),
        }
    }
}

/// The query engine. One per querier process; reads the RFC 0005
/// Parquet + audit store through the `ourios-parquet` [`Store`] seam,
/// so the same engine targets a local-filesystem store (dev / test /
/// the regression guard) or an S3-compatible bucket (production,
/// `AGENTS.md` §3.6).
///
/// The backend (local vs S3) drives the hybrid scan: a local backend addresses
/// files by absolute local path and walks `std::fs` (unchanged from before
/// RFC 0019, missing dirs tolerated as empty); an S3 backend registers the
/// [`Store`]'s `object_store` on the `SessionContext`, addresses tables by
/// object-store URL, and resolves the live-file set through
/// [`Store::list_blocking`] (RFC 0019 §3.3).
#[derive(Debug, Clone)]
pub struct Querier {
    backend: Backend,
    /// The deployment's declared promoted set (RFC 0042 §3.3): drives
    /// the scan schema's type for declared promoted columns, so files
    /// written under other declarations read those columns as absent.
    /// Defaults to the implicit-`service.name`-only set, under which
    /// the scan stays purely schema-driven (the RFC 0022 behaviour).
    promoted: ourios_parquet::PromotedAttributes,
    /// The per-tenant single-flight over the template-map acquisition,
    /// shared by every clone: concurrent queries at one frontier wait on one
    /// acquisition instead of each running their own.
    template_maps: Arc<map_flight::TemplateMapFlights>,
}

/// The object-store URL scheme/authority the S3 scan registers its
/// [`Store`] under and addresses tables by — `ourios://store/<key>`.
/// The host carries no meaning beyond keying the `SessionContext`'s
/// object-store registry (the real bucket/prefix is inside the
/// registered store); using a private scheme keeps these synthetic URLs
/// from colliding with any real `s3://` / `file://` addressing.
const STORE_URL: &str = "ourios://store";

/// [`Querier::collect_records`]'s output: the rendered rows plus the
/// materialization pass's own IO accounting (RFC 0031 §3.6). Defaults to
/// all-empty/zero — the count-only case.
#[derive(Default)]
struct CollectedRecords {
    records: Vec<LogRow>,
    /// The materialization scan's own stats: `bytes_read` feeds
    /// [`QueryResult::materialize_bytes_read`]; the row-group counts serve
    /// the single-pass elision, which reports them as the query's pruning
    /// stats ([`QueryOptions::elide_count_scan`]).
    scan: QueryStats,
    registry_bytes_read: u64,
    /// Template-map bytes this pass fetched itself acquiring lazily; `0`
    /// when it reused a compile-time acquisition or joined another query's.
    map_fetched: u64,
}

/// How many times [`Querier::execute`] resolves and scans before a read error
/// over a file set that keeps changing is returned (RFC0009.18).
const SCAN_ATTEMPTS: u32 = 3;

/// One resolve-and-scan attempt of [`Querier::execute`].
struct ScanAttempt<'a, F> {
    scope: file_set::DataScope<'a>,
    terminal: Terminal,
    /// The query's one compile-time template-map acquisition, handed to
    /// every attempt so filter and rendering use the same map (RFC 0033).
    acquired: Option<map_flight::MapAcquisition>,
    build_filter: &'a F,
    /// The last attempt returns its error rather than retrying.
    last: bool,
}

/// How one [`ScanAttempt`] ended.
enum Attempt {
    Done(Result<QueryResult, QueryError>),
    /// The resolution went stale under a concurrent commit: try again.
    Stale,
}

/// How [`Querier::execute`] terminates the filtered scan (RFC 0002 §6.5):
/// count + optional row materialization, or one grouped-count aggregation
/// returning the map. An enum so an aggregation carrying a row limit /
/// elision option is unrepresentable — the map is the whole result.
#[derive(Clone)]
enum Terminal {
    /// Count + up-to-`limit` rendered rows (RFC 0017 §3.4).
    Rows {
        limit: Option<usize>,
        options: QueryOptions,
    },
    /// A validated `count [by …]` stage (RFC 0002 amendment 2026-07-15).
    Aggregate(plan::Aggregate),
}

impl Terminal {
    fn aggregate(&self) -> Option<&plan::Aggregate> {
        match self {
            Self::Aggregate(agg) => Some(agg),
            Self::Rows { .. } => None,
        }
    }
}

impl Querier {
    /// Create a querier reading the RFC 0005 store under the **local**
    /// `bucket_root` (the same root the `ourios-parquet` writer writes
    /// `data/tenant_id=…/year=…/…` under). The default constructor —
    /// the local backend is the test/dev default and the RFC 0019
    /// regression guard.
    ///
    /// Infallible and side-effect-free: it only records the path (no I/O, no
    /// `Store` construction), so it never panics and never requires the bucket
    /// to exist. A query against a not-yet-created bucket yields an empty result
    /// (the `std::fs` read paths tolerate `NotFound`), exactly as before
    /// RFC 0019.
    pub fn new(bucket_root: impl Into<PathBuf>) -> Self {
        Self {
            backend: Backend::Local(bucket_root.into()),
            promoted: ourios_parquet::PromotedAttributes::default(),
            template_maps: Arc::default(),
        }
    }

    /// Declare the deployment's promoted set (RFC 0042 §3.3): a
    /// declared key's class fixes its scan-schema column type, so
    /// files written under a different declaration read that column
    /// as absent instead of erroring the scan or coercing values.
    /// Without this, the scan is purely schema-driven (the RFC 0022
    /// behaviour, and the default for tests/dev).
    #[must_use]
    pub fn with_promoted_attributes(
        mut self,
        promoted: ourios_parquet::PromotedAttributes,
    ) -> Self {
        self.promoted = promoted;
        self
    }

    /// Create a querier from a resolved [`StoreConfig`] (RFC 0019 §3.2)
    /// — the S3-capable constructor the server wires the querier role
    /// through. A `Local` config is equivalent to [`Self::new`]; an
    /// `S3` config drives the object-store scan branch.
    ///
    /// # Errors
    ///
    /// [`QueryError::Storage`] if the S3 backend cannot be constructed
    /// (e.g. an invalid S3 config — see [`StoreConfig::open`]). A local
    /// config is infallible (it defers to [`Self::new`]).
    pub fn from_store_config(config: &StoreConfig) -> Result<Self, QueryError> {
        let backend = match config {
            StoreConfig::Local(root) => Backend::Local(root.clone()),
            StoreConfig::S3(_) => {
                let store = config.open().map_err(|e| QueryError::Storage {
                    detail: format!("open store: {e}"),
                })?;
                Backend::Remote(store.with_caller(ourios_parquet::Caller::Querier))
            }
        };
        Ok(Self {
            backend,
            promoted: ourios_parquet::PromotedAttributes::default(),
            template_maps: Arc::default(),
        })
    }

    /// The background template-map refresh task over this querier's store
    /// (RFC 0060), sharing its per-tenant single-flight: a query for a
    /// tenant the task is folding joins that fold.
    #[must_use]
    pub fn template_map_refresher(&self) -> TemplateMapRefresher {
        TemplateMapRefresher::new(Arc::clone(&self.template_maps), self.backend.clone())
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
        self.execute(
            &tenant,
            window,
            Terminal::Rows {
                limit: row_limit,
                options: QueryOptions::default(),
            },
            None,
            |df| apply_request_filters(df, &request),
        )
        .await
    }

    /// Compile a parsed DSL [`Query`](dsl::Query) IR (RFC 0002) to the
    /// `DataFusion` execution layer and run it against the tenant's RFC 0005
    /// store, returning the matching row count + pruning stats — without
    /// leaking `DataFusion`/arrow/SQL (hazard `AGENTS.md` §4.6 / RFC0002.3).
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
    /// from. `None` — the production default — resolves the requesting
    /// tenant's map at compile time per RFC 0005 §3.7.1 through the
    /// RFC 0033 cached template map (the audit stream stays the source of
    /// truth: an artifact hit reflects exactly the fresh fold, and every
    /// non-hit disposition *is* the fresh fold; the acquisition is
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
        self.run_query_with(
            query,
            tenant,
            now_unix_nano,
            default_window_nanos,
            alias_map,
            QueryOptions::default(),
        )
        .await
    }

    /// [`run_query`](Self::run_query) with explicit execution
    /// [`QueryOptions`] — the additive opt-in surface;
    /// `QueryOptions::default()` is exactly `run_query`.
    ///
    /// # Errors
    ///
    /// As [`run_query`](Self::run_query).
    pub async fn run_query_with(
        &self,
        query: &dsl::Query,
        tenant: &TenantId,
        now_unix_nano: u64,
        default_window_nanos: u64,
        alias_map: Option<&ourios_core::alias::AliasMap>,
        options: QueryOptions,
    ) -> Result<QueryResult, QueryError> {
        // Error precedence: authorization first, so a metadata-only
        // reader's query on a content column is `Forbidden` (RFC0047.8,
        // RFC 0047 §3.4) whatever else is wrong with it. Then stage-support
        // and window/limit validation, before the alias-map derivation
        // below, so those query errors surface without paying the
        // audit-tree IO (or its Storage errors). Predicate compilation
        // needs the map, so its errors necessarily come after. `compile`
        // re-runs the same pure validation internally — one source of
        // truth, negligible cost.
        if let Some(visibility) = &options.visibility {
            visibility.validate(query)?;
        }
        plan::validate(query, now_unix_nano, default_window_nanos)?;
        // A `body ==`/`!=` needs the RFC 0017 registry for the RFC 0044
        // template arm; the `resolves_to` alias fold needs the alias map.
        // Both ride the one RFC 0033 cached-map acquisition (artifact hit or
        // fresh fold + write-through, single-flighted per tenant), so the two
        // needs share one map (and one frontier) per query — and the
        // acquisition is skipped entirely when neither is in the predicate.
        // The blocking IO (S3 GETs / local `std::fs`) offloads off the
        // runtime worker, mirroring `run_drift`.
        let needs_registry = plan::uses_body_equality(&query.predicate);
        let needs_alias_fold = alias_map.is_none() && plan::uses_resolves_to(&query.predicate);
        let mut acquired: Option<map_flight::MapAcquisition> = None;
        if needs_alias_fold || needs_registry {
            let acquisition = self
                .template_maps
                .acquire_fetched(&self.backend, tenant, template_map::AcquireTrigger::Query)
                .await?;
            acquired = Some(acquisition);
        }
        let derived;
        let map = match (alias_map, &acquired) {
            // A caller-held projection — the test/operator override —
            // always wins (RFC 0005 §3.7.1).
            (Some(map), _) => map,
            (None, Some(a)) => a.map.alias_map(),
            // Never consulted: an empty projection, no audit-tree scan.
            (None, None) => {
                derived = ourios_core::alias::AliasMap::new();
                &derived
            }
        };
        let empty_registry;
        let registry = match &acquired {
            Some(a) if needs_registry => a.map.registry(),
            // Never consulted: no body equality in the predicate.
            _ => {
                empty_registry = TemplateRegistry::new();
                &empty_registry
            }
        };
        let plan = plan::compile(
            query,
            tenant,
            now_unix_nano,
            default_window_nanos,
            map,
            registry,
            options.visibility.clone(),
        )?;
        // The DSL `limit` (RFC 0002) doubles as the RFC 0017 row cap; read it
        // — and the aggregation stage — before `plan` moves into the filter
        // closure. An aggregation query terminates in the grouped-count scan:
        // the map is the whole result, so the row cap and the count-scan
        // elision option have nothing to apply to.
        let terminal = match plan.aggregate.clone() {
            Some(aggregate) => Terminal::Aggregate(aggregate),
            None => Terminal::Rows {
                limit: plan.limit,
                options,
            },
        };
        self.execute(tenant, Some(plan.window), terminal, acquired, move |df| {
            plan::apply(df, plan.clone())
        })
        .await
    }

    /// Derive `tenant`'s template registry (RFC 0017 §3.2) — the
    /// `(template_id, version) → tokens` fold of its audit stream — with
    /// the blocking store reads offloaded like every other derivation.
    /// The RFC 0027 `list_templates` tool serves this directly; the JSON
    /// API consumes it internally via row rendering.
    ///
    /// # Errors
    ///
    /// [`QueryError::Storage`] as [`derive_template_registry`].
    pub async fn template_registry(
        &self,
        tenant: &TenantId,
    ) -> Result<TemplateRegistry, QueryError> {
        self.spawn_blocking_io({
            let backend = self.backend.clone();
            let tenant = tenant.clone();
            move || derive_template_registry(backend.store_ref(), &tenant)
        })
        .await
    }

    /// Execute a RFC 0010 `drift` query against the tenant's RFC 0005 `audit/`
    /// stream and return the per-template [`DriftRow`]s + pruning stats —
    /// without leaking `DataFusion`/arrow/SQL (hazard `AGENTS.md` §4.6 /
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
        drift::run_drift(self.backend.store_ref(), query, tenant, now_unix_nano).await
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
        // How the filtered scan terminates: `Terminal::Rows` counts and —
        // when it carries a limit — collects up to that many rows into
        // `QueryResult.records` (RFC 0017 §3.4), with the count-scan
        // elision per its `options`; `Terminal::Aggregate` runs the
        // grouped-count scan and returns the map (RFC 0002 §6.5 amendment).
        terminal: Terminal,
        // A template map already acquired at query-compile time (the
        // `resolves_to` alias fold, RFC 0033) — handed to the row-rendering
        // pass so one acquisition serves both folds. `None` ⇒ rendering
        // acquires lazily.
        acquired: Option<map_flight::MapAcquisition>,
        build_filter: F,
    ) -> Result<QueryResult, QueryError>
    where
        // `Ok(None)` ⇒ the filter is provably empty (an absent OPTIONAL
        // column, RFC 0005 §3.9), so the query short-circuits to an empty
        // result rather than planning a scan that matches nothing.
        F: Fn(
            datafusion::dataframe::DataFrame,
        ) -> Result<Option<datafusion::dataframe::DataFrame>, QueryError>,
    {
        let enc = percent_encode_tenant(tenant.as_str());
        let data_prefix = format!("data/tenant_id={enc}");
        let scope = file_set::DataScope {
            tenant,
            prefix: &data_prefix,
            window: partition_window,
        };
        // A compaction that commits while this query runs can delete files
        // the resolution named; their rows are already in a file the new
        // manifest names, so a read error over a file set that has since
        // changed is a stale resolution, not a loss. Resolve and scan again,
        // a bounded number of times (RFC 0009 §3.4, RFC0009.18). A result
        // is only ever returned whole, so no rows have been sent on a retry.
        // The compile-time acquisition's bytes are counted once, here.
        let mut fetched = acquired.as_ref().map_or(0, |a| a.fetched);
        let mut attempt = 1;
        loop {
            let ctx = crate::exec::session();
            let scan = ScanAttempt {
                scope,
                terminal: terminal.clone(),
                acquired: acquired.clone(),
                build_filter: &build_filter,
                last: attempt == SCAN_ATTEMPTS,
            };
            match self.attempt_scan(&ctx, scan).await {
                Attempt::Done(outcome) => {
                    return outcome
                        .map(|result| crate::fetch_count::with_fetched(result, &ctx, fetched));
                }
                Attempt::Stale => {
                    fetched += crate::fetch_count::fetched(&ctx);
                    attempt += 1;
                }
            }
        }
    }

    /// Resolve and scan once. A storage error from the scan is a stale
    /// resolution, worth another attempt, when the file set has moved since
    /// it was resolved; an error that persists is returned by the last
    /// attempt. (A file that vanishes during the local resolution itself is
    /// re-listed inside it, see `file_set::resolve_local_urls`.)
    async fn attempt_scan<F>(&self, ctx: &SessionContext, scan: ScanAttempt<'_, F>) -> Attempt
    where
        F: Fn(
            datafusion::dataframe::DataFrame,
        ) -> Result<Option<datafusion::dataframe::DataFrame>, QueryError>,
    {
        let (scope, last) = (scan.scope, scan.last);
        let urls = match file_set::resolve_data_urls(&self.backend, ctx, scope).await {
            Ok(urls) => urls,
            Err(e) => return Attempt::Done(Err(e)),
        };
        match self.scan_resolved(ctx, urls.clone(), scan).await {
            Err(e @ QueryError::Storage { .. }) if !last => {
                match self.file_set_moved(ctx, scope, &urls).await {
                    Ok(true) => Attempt::Stale,
                    Ok(false) => Attempt::Done(Err(e)),
                    Err(recheck) => Attempt::Done(Err(recheck)),
                }
            }
            outcome => Attempt::Done(outcome),
        }
    }

    /// Whether `scope` now resolves to a different file set than `used`: a
    /// commit has replaced files since `used` was resolved. The recheck runs
    /// on the failed attempt's session, so the manifest bytes it reads count
    /// toward the query's `bytes_fetched`.
    async fn file_set_moved(
        &self,
        ctx: &SessionContext,
        scope: file_set::DataScope<'_>,
        used: &[ListingTableUrl],
    ) -> Result<bool, QueryError> {
        let now = file_set::recheck_data_urls(&self.backend, ctx, scope).await?;
        Ok(now != used)
    }

    /// Scan the resolved file set `urls` once. An empty set ⇒ the tenant has
    /// nothing queryable ⇒ empty result (not an error). Covers the
    /// missing-dir case and a partition holding only `*.parquet.tmp` (a
    /// poisoned / crashed writer) — where building a table over zero files
    /// would otherwise error and wrongly fail the query.
    async fn scan_resolved<F>(
        &self,
        ctx: &SessionContext,
        urls: Vec<ListingTableUrl>,
        scan: ScanAttempt<'_, F>,
    ) -> Result<QueryResult, QueryError>
    where
        F: Fn(
            datafusion::dataframe::DataFrame,
        ) -> Result<Option<datafusion::dataframe::DataFrame>, QueryError>,
    {
        let ScanAttempt {
            scope,
            terminal,
            acquired,
            build_filter,
            ..
        } = scan;
        let tenant = scope.tenant;
        if urls.is_empty() {
            return Ok(empty_result(terminal.aggregate()));
        }
        let files_opened = urls.len() as u64;

        // Union schema + promoted no-coercion adapter — the rationale
        // lives on `SchemaMode::Union` (epic #745 wave 2).
        let base = register_logs_table(ctx, urls, &self.promoted, tenant).await?;
        let mut result = match build_filter(base)? {
            // A provably-empty filter (absent OPTIONAL column) ⇒ no scan. The
            // files were still opened: schema inference read every footer.
            None => empty_result(terminal.aggregate()),
            Some(df) => {
                self.run_terminal(ctx, df, tenant, terminal, acquired)
                    .await?
            }
        };
        result.stats.files_opened = files_opened;
        Ok(result)
    }

    /// Run `terminal` over the filtered frame `df` (see [`Self::execute`],
    /// which sets the result's `files_opened` and `bytes_fetched`).
    async fn run_terminal(
        &self,
        ctx: &SessionContext,
        df: datafusion::dataframe::DataFrame,
        tenant: &TenantId,
        terminal: Terminal,
        mut acquired: Option<map_flight::MapAcquisition>,
    ) -> Result<QueryResult, QueryError> {
        // An aggregation query terminates in its own grouped-count scan
        // (RFC 0002 §6.5 amendment): the map is the result, so the
        // count/materialize passes below never run for it.
        let (row_limit, query_options) = match terminal {
            Terminal::Aggregate(agg) => {
                return self
                    .execute_aggregate(df, &agg, tenant, ctx.task_ctx())
                    .await;
            }
            Terminal::Rows { limit, options } => (limit, options),
        };

        // RFC 0031 §3.6 single-pass — with `elide_count_scan`, materialise
        // FIRST: a result that did not hit the limit is complete, so the
        // count is the returned row count and the count scan (which would
        // re-read the same row groups for information already in hand) is
        // skipped. The reported pruning counts are the materialize plan's —
        // identical to the count scan's, because both prune by the same
        // predicate over the same file set and an unreached limit never
        // stops the scan early — with `bytes_read = 0`: the count scan
        // genuinely read nothing. A possibly-truncated result
        // (returned == limit) falls through to the count scan below, so
        // `rows` stays the full matching total.
        let mut early = None;
        if let Some(n) = row_limit
            && query_options.elide_count_scan
        {
            let collected = self
                .collect_records(df.clone(), n, tenant, ctx.task_ctx(), acquired.take())
                .await?;
            if collected.records.len() < n {
                let mut records = collected.records;
                if let Some(visibility) = &query_options.visibility {
                    visibility.mask(&mut records);
                }
                return Ok(QueryResult {
                    rows: records.len() as u64,
                    stats: QueryStats {
                        bytes_read: 0,
                        ..collected.scan
                    },
                    records,
                    aggregate: None,
                    materialize_bytes_read: collected.scan.bytes_read,
                    registry_bytes_read: collected.registry_bytes_read,
                    bytes_fetched: collected.map_fetched,
                });
            }
            early = Some(collected);
        }

        // Clone `df` so the (RFC 0017) row collection below reads the same
        // filtered frame.
        let (rows, stats) = count_scan(df.clone(), ctx).await?;

        // RFC 0017 §3.3/§3.4 — when a `row_limit` is requested, materialise the
        // matching rows (the same filtered frame, capped at the limit), decode
        // them to `MinedRecord`s, and render each into a `LogRow` via the
        // read-time template registry. Heavy columns are only materialised for
        // these (≤ limit) rows. `None` ⇒ count-only (records stays empty). A
        // truncated single-pass run already materialised — reuse it.
        let collected = match (early, row_limit) {
            (Some(collected), _) => collected,
            (None, Some(n)) => {
                self.collect_records(df, n, tenant, ctx.task_ctx(), acquired.take())
                    .await?
            }
            (None, None) => CollectedRecords::default(),
        };
        let mut records = collected.records;
        if let Some(visibility) = &query_options.visibility {
            visibility.mask(&mut records);
        }
        Ok(QueryResult {
            rows,
            stats,
            records,
            aggregate: None,
            materialize_bytes_read: collected.scan.bytes_read,
            registry_bytes_read: collected.registry_bytes_read,
            bytes_fetched: collected.map_fetched,
        })
    }

    /// Execute a validated `count [by …]` stage over the filtered frame
    /// (RFC 0002 §6.5 amendment 2026-07-15): one grouped-count scan whose
    /// column reads are the user's predicate/window filters + the
    /// row-level tenant backstop (§3.7) + the group-term columns only —
    /// never `body`/`separators` — with zero row materialization and
    /// zero template-map acquisition (nothing is rendered), the
    /// RFC0002.16 honest-bytes shape. `rows` stays the total
    /// matching-row count (included + excluded), derived from the same
    /// scan.
    async fn execute_aggregate(
        &self,
        df: datafusion::dataframe::DataFrame,
        agg: &plan::Aggregate,
        tenant: &TenantId,
        task_ctx: Arc<datafusion::execution::TaskContext>,
    ) -> Result<QueryResult, QueryError> {
        let aggregated = aggregate_frame(df, agg, tenant)?;
        let (batches, scan) = execute_plan(aggregated, task_ctx).await?;
        let decoded = decode_aggregate(&batches, agg.by.len(), agg.scalar.is_some())?;
        Ok(QueryResult {
            rows: decoded.rows,
            stats: QueryStats {
                rows_excluded: decoded.excluded,
                ..scan
            },
            records: Vec::new(),
            aggregate: Some(decoded.groups),
            materialize_bytes_read: 0,
            registry_bytes_read: 0,
            bytes_fetched: 0,
        })
    }

    /// Materialise up to `limit` matching rows from the filtered `df`, decode
    /// them, and render each into a [`LogRow`] (RFC 0017 §3.3). The template
    /// map is acquired once — reusing `acquired` when the query-compile
    /// alias fold already paid for it, otherwise through the RFC 0033
    /// cached read path (artifact hit, or fresh fold + write-through) —
    /// and only when there are rows to render. Returns the rows plus this
    /// pass's own IO accounting (RFC 0031 §3.6), kept out of
    /// [`QueryStats`] so the count-scan figures B1/B2 assert on stay
    /// exactly the count scan.
    async fn collect_records(
        &self,
        df: datafusion::dataframe::DataFrame,
        limit: usize,
        tenant: &TenantId,
        task_ctx: Arc<datafusion::execution::TaskContext>,
        acquired: Option<map_flight::MapAcquisition>,
    ) -> Result<CollectedRecords, QueryError> {
        // Filter pushdown ("late materialization", off by default in
        // DataFusion 54): the scan evaluates the predicate during Parquet
        // decode and — via the writer's offset index — fetches only the
        // heavy-column (`body` / `params` / `attributes`) pages the selected
        // rows live in, instead of the whole page-index-matched window of
        // every unpruned chunk. This keeps the RFC 0031 §3.6 materialization
        // component proportional to the result size, not the partition size
        // (regression-pinned in `ourios-bench`'s
        // `one_row_materialization_reads_pages_not_whole_chunks`); the
        // session-level option reaches the directly-constructed
        // `ParquetFormat` because `ParquetSource::try_pushdown_filters`
        // honours table *or* session config. Scoped to this scan only: on
        // the count scan, pushdown lets statistics-fully-matched row groups
        // answer with **zero** bytes scanned, which would hollow out the
        // `stats.bytes_read` figure the B1/B2 gates and the RFC 0031 honest
        // total assert on — the count scan keeps the default.
        let (state, logical_plan) = df.into_parts();
        let mut config = state.config().clone();
        {
            let parquet = &mut config.options_mut().execution.parquet;
            parquet.pushdown_filters = true;
            parquet.reorder_filters = true;
        }
        let state = datafusion::execution::SessionStateBuilder::new_from_existing(state)
            .with_config(config)
            .build();
        let df = datafusion::dataframe::DataFrame::new(state, logical_plan);
        let limited = df.limit(0, Some(limit)).map_err(storage_err)?;
        // Plan + collect by hand (rather than `DataFrame::collect`) so this
        // scan's metrics can be read off the retained plan. The caller folds
        // only the bytes into its accounting — this scan's row-group counts
        // stay out of `QueryStats` so the B1 pruned fraction keeps its
        // count-scan-only meaning — except under count-scan elision, where
        // they *are* the count-scan counts (see `QueryOptions`).
        let (batches, scan) = execute_plan(limited, task_ctx).await?;
        // The single RFC 0005 decode path (RFC 0021 §3.1 / RFC0021.4):
        // `ShapeValidation::Skip` because `render_log_body` handles every
        // record shape safely — this path renders rather than rejects
        // (foreign/degraded files still serve queries; RFC 0017).
        let mut mined = Vec::new();
        let mut row_offset = 0usize;
        for batch in &batches {
            let records = ourios_parquet::batch_to_mined_records(
                batch,
                row_offset,
                ourios_parquet::ShapeValidation::Skip,
            )
            .map_err(|e| QueryError::Storage {
                detail: format!("decode rows: {e}"),
            })?;
            row_offset += batch.num_rows();
            mined.extend(records);
        }
        if mined.is_empty() {
            return Ok(CollectedRecords {
                scan,
                ..CollectedRecords::default()
            });
        }
        // Row-level tenant backstop (`AGENTS.md` §3.7 / RFC 0005 §3.9
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
        // The single per-query template-map acquisition, measured
        // (RFC 0031 §3.6 / RFC 0033): reuse the compile-time alias fold's
        // map when the query already acquired one, else resolve through
        // the per-tenant single-flight (`map_flight`).
        // A reused compile-time acquisition was already counted by `execute`.
        let (acquired, map_fetched) = if let Some(acquired) = acquired {
            (acquired, 0)
        } else {
            let acquired = self
                .template_maps
                .acquire_fetched(&self.backend, tenant, template_map::AcquireTrigger::Query)
                .await?;
            let fetched = acquired.fetched;
            (acquired, fetched)
        };
        Ok(CollectedRecords {
            // The batch constructor memoises per-(id, version) work —
            // one lookup + one formatted string per distinct template,
            // not per row.
            records: LogRow::from_records(&mined, acquired.map.registry()),
            scan,
            registry_bytes_read: acquired.bytes,
            map_fetched,
        })
    }

    /// Run blocking storage work — an audit derivation (`derive_alias_map` /
    /// `derive_template_registry` / the template-map acquisition) or the
    /// data file-set resolution — on the tokio blocking pool so the async
    /// query path doesn't tie up a runtime worker on the S3 bridge calls (or
    /// local `std::fs`) — the same offload `run_drift` applies to the listing.
    /// The closure owns its captured `Backend` / `TenantId` clones so it
    /// satisfies the `'static + Send` bound. The caller's span is re-entered on
    /// the blocking thread — span context does not cross `spawn_blocking`
    /// (RFC 0038 §3.3) — so logs the callee emits carry the query's trace
    /// context (RFC0038.1).
    async fn spawn_blocking_io<T, F>(&self, work: F) -> Result<T, QueryError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, QueryError> + Send + 'static,
    {
        let span = tracing::Span::current();
        tokio::task::spawn_blocking(move || span.in_scope(work))
            .await
            .map_err(|e| QueryError::Storage {
                detail: format!("blocking storage task: {e}"),
            })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A remote querier's store calls count as the querier's, and the
    /// template-map acquisition re-attributes its own.
    #[test]
    fn a_remote_querier_attributes_its_store_calls() {
        let config = StoreConfig::S3(
            ourios_parquet::S3Config::new("ourios-test")
                .with_endpoint("https://s3.example.invalid")
                .with_region("eu-central-1"),
        );
        let querier = Querier::from_store_config(&config).expect("querier");
        let caller = |backend: &Backend| match backend {
            Backend::Remote(store) => store.caller(),
            Backend::Local(root) => panic!("expected a remote backend, got {root:?}"),
        };
        assert_eq!(caller(&querier.backend), ourios_parquet::Caller::Querier);
        let map = querier
            .backend
            .clone()
            .with_caller(ourios_parquet::Caller::TemplateMap);
        assert_eq!(caller(&map), ourios_parquet::Caller::TemplateMap);
    }

    /// `Querier::new` is infallible and must not panic on a not-yet-created
    /// bucket: it only records the path (no `Store` construction, which would
    /// canonicalize and fail on a missing dir). Querying a non-existent local
    /// bucket returns an empty result — the `std::fs` read paths tolerate
    /// `NotFound` — exactly as before RFC 0019.
    #[tokio::test]
    async fn new_on_missing_dir_does_not_panic_and_queries_empty() {
        // A path under a temp dir that was never created — `Store::local` would
        // error on this (it canonicalizes the prefix), but `new` must not.
        let tmp = tempfile::tempdir().expect("temp");
        let missing = tmp.path().join("never/created/bucket");
        assert!(!missing.exists(), "precondition: the bucket dir is absent");

        let querier = Querier::new(&missing);
        let result = querier
            .run(QueryRequest {
                tenant: ourios_core::tenant::TenantId::new("acme"),
                time_range: None,
                template_id: None,
                severity_text: None,
                limit: None,
            })
            .await
            .expect("a query against a missing bucket is an empty result, not an error");
        assert_eq!(result.rows, 0, "no rows from a non-existent bucket");
        assert!(result.records.is_empty());
        assert_eq!(result.stats, QueryStats::default());
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

    /// #853 on the S3 path: which prefixes a query touches beyond the scan.
    mod storage_reach {
        use super::*;
        use crate::test_support::{Call, CountingStore};
        use ourios_core::audit::{
            AuditEvent, AuditPayload, AuditSink as _, TemplateChange, hash_triggering_line,
        };
        use ourios_core::record::{BodyKind, MinedRecord, Param};

        /// 2026-04-02T10:58:00 UTC.
        const TS0: u64 = 1_775_127_480_000_000_000;
        const MINUTE_NS: u64 = 60_000_000_000;

        fn record(ts_ns: u64) -> MinedRecord {
            MinedRecord {
                tenant_id: TenantId::new("acme"),
                template_id: 7,
                template_version: 1,
                severity_number: 9,
                severity_text: None,
                scope_name: None,
                scope_version: None,
                scope_attributes: Vec::new(),
                resource_schema_url: None,
                scope_schema_url: None,
                time_unix_nano: ts_ns,
                observed_time_unix_nano: None,
                attributes: Vec::new(),
                dropped_attributes_count: 0,
                resource_attributes: Vec::new(),
                trace_id: None,
                span_id: None,
                flags: 0,
                event_name: None,
                body_kind: BodyKind::String,
                params: vec![Param {
                    type_tag: ourios_core::audit::ParamType::Num,
                    value: "42".to_string(),
                }],
                separators: vec![String::new(), " ".to_string()],
                body: None,
                confidence: 1.0,
                lossy_flag: false,
            }
        }

        fn adopted(ts_ns: u64) -> AuditEvent {
            AuditEvent {
                tenant_id: TenantId::new("acme"),
                timestamp: std::time::UNIX_EPOCH + std::time::Duration::from_nanos(ts_ns),
                payload: AuditPayload::Template {
                    template_id: 7,
                    triggering_line_hash: hash_triggering_line(b"line"),
                    triggering_line_sample: None,
                    change: TemplateChange::Adopted {
                        template_version: 1,
                        new_template: "user <*>".to_string(),
                    },
                },
            }
        }

        /// A counted S3-path querier over a local store holding one hour of
        /// `acme` rows and an audit stream spread over many days.
        fn seeded(root: &std::path::Path) -> (Querier, CountingStore) {
            let store = Store::local(root).expect("local store");
            let rows: Vec<MinedRecord> = (0..3).map(|i| record(TS0 + i * 1_000)).collect();
            let partition = ourios_parquet::PartitionKey::derive(&rows[0]).expect("partition");
            let mut writer = ourios_parquet::Writer::open_in(&store, partition).expect("writer");
            writer.append_records(&rows).expect("append");
            writer.close().expect("close");
            let mut sink = ourios_parquet::ParquetAuditSink::new(store.clone());
            for day in 0..5 {
                sink.emit(adopted(TS0 - day * 24 * 60 * MINUTE_NS));
            }
            assert_eq!(sink.write_failures(), 0);
            let mut counter = None;
            let store = store.wrap_backend(|inner| {
                let counting = CountingStore::new(inner);
                counter = Some(counting.clone());
                Arc::new(counting)
            });
            let querier = Querier {
                backend: Backend::Remote(store),
                promoted: ourios_parquet::PromotedAttributes::default(),
                template_maps: Arc::default(),
            };
            (querier, counter.expect("wrapped"))
        }

        fn windowed(template_id: u64) -> QueryRequest {
            QueryRequest {
                tenant: TenantId::new("acme"),
                time_range: Some((TS0 - 5 * MINUTE_NS, TS0 + 5 * MINUTE_NS)),
                template_id: Some(template_id),
                severity_text: None,
                limit: Some(100),
            }
        }

        fn audit_calls(counter: &CountingStore) -> Vec<Call> {
            counter
                .calls()
                .into_iter()
                .filter(|call| match call {
                    Call::List(key)
                    | Call::ListDelimited(key)
                    | Call::Get(key)
                    | Call::GetRanges(key) => key.starts_with("audit/"),
                })
                .collect()
        }

        /// A query that renders no row never acquires the template map, so it
        /// never touches the tenant's audit stream — the shape of the #853
        /// query (`rows: 0`, no `body ==` / `resolves_to` in the predicate).
        #[tokio::test]
        async fn a_query_that_renders_no_row_does_not_touch_the_audit_stream() {
            let tmp = tempfile::tempdir().expect("temp");
            let (querier, counter) = seeded(tmp.path());

            let result = querier.run(windowed(999)).await.expect("query");

            assert_eq!(result.rows, 0);
            assert_eq!(result.registry_bytes_read, 0);
            assert!(
                audit_calls(&counter).is_empty(),
                "{:?}",
                audit_calls(&counter),
            );
        }

        /// A query that renders rows acquires the template map once. On an
        /// RFC 0033 cache hit that costs one listing of the tenant's whole
        /// audit prefix (the §3.3 freshness frontier) and one artifact GET, and
        /// no audit-file GET — the listing is per query and not bounded by the
        /// query window.
        #[tokio::test]
        async fn a_cache_hit_lists_the_whole_audit_prefix_once_and_reads_only_the_artifact() {
            let tmp = tempfile::tempdir().expect("temp");
            let (querier, counter) = seeded(tmp.path());
            let first = querier
                .run(windowed(7))
                .await
                .expect("miss: fold + publish");
            assert_eq!(first.records.len(), 3);
            counter.reset();

            let hit = querier.run(windowed(7)).await.expect("hit");

            assert_eq!(hit.records.len(), 3);
            let calls = audit_calls(&counter);
            let lists: Vec<&Call> = calls
                .iter()
                .filter(|c| matches!(c, Call::List(_) | Call::ListDelimited(_)))
                .collect();
            assert_eq!(lists, vec![&Call::List("audit/tenant_id=acme".to_string())]);
            let gets: Vec<&Call> = calls
                .iter()
                .filter(|c| matches!(c, Call::Get(_) | Call::GetRanges(_)))
                .collect();
            assert_eq!(gets.len(), 1, "only the artifact is read: {gets:?}");
            let audit_keys = counter
                .listed_keys()
                .into_iter()
                .filter(|k| k.starts_with("audit/"))
                .count();
            assert!(
                audit_keys >= 5,
                "every day's audit file is listed: {audit_keys}"
            );
        }
    }
}
