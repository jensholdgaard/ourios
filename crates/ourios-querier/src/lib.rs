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
mod body_match;
mod drift;
pub mod dsl;
mod log_row;
mod plan;
mod schema_adapt;
mod template_map;
mod template_registry;
pub mod visibility;

pub use alias_store::derive_alias_map;
pub use audit_scan::StoreRef;
pub use drift::{DriftResult, DriftRow};
pub use log_row::{LogBody, LogRow, render_log_body};
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
mod file_set;
mod stats;

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
}

/// The query engine. One per querier process; reads the RFC 0005
/// Parquet + audit store through the `ourios-parquet` [`Store`] seam,
/// so the same engine targets a local-filesystem store (dev / test /
/// the regression guard) or an S3-compatible bucket (production,
/// `CLAUDE.md` §3.6).
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
}

/// How [`Querier::execute`] terminates the filtered scan (RFC 0002 §6.5):
/// count + optional row materialization, or one grouped-count aggregation
/// returning the map. An enum so an aggregation carrying a row limit /
/// elision option is unrepresentable — the map is the whole result.
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

/// One per-query template-map acquisition (RFC 0033): the artifact-or-fold
/// [`TemplateMap`] plus the bytes fetched acquiring it
/// ([`QueryResult::registry_bytes_read`]). Acquired at most once per query
/// — at compile time when the DSL uses `resolves_to` (the alias fold),
/// otherwise lazily by [`Querier::collect_records`] when there are rows to
/// render — so the alias map and the registry can never come from
/// different frontiers within one query (§3.1's one-artifact rationale).
struct AcquiredTemplateMap {
    map: TemplateMap,
    acquisition_bytes: u64,
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
                Backend::Remote(store)
            }
        };
        Ok(Self {
            backend,
            promoted: ourios_parquet::PromotedAttributes::default(),
        })
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
        // Error precedence: stage-support and window/limit validation
        // runs before the alias-map derivation below, so those query
        // errors surface without paying the audit-tree IO (or its
        // Storage errors). Predicate compilation needs the map, so its
        // errors necessarily come after. `compile` re-runs the same
        // pure validation internally — one source of truth, negligible
        // cost.
        plan::validate(query, now_unix_nano, default_window_nanos)?;
        // RFC 0047 §3.4: a metadata-only reader's query must not touch a
        // content column — rejected before any IO, naming the column.
        if let Some(visibility) = &options.visibility {
            visibility.validate(query)?;
        }
        // A `body ==`/`!=` needs the RFC 0017 registry for the RFC 0044
        // template arm; the `resolves_to` alias fold needs the alias map.
        // Both ride the one RFC 0033 cached-map acquisition (artifact hit or
        // fresh fold + write-through), so the two needs share one map (and
        // one frontier) per query — and the acquisition is skipped entirely
        // when neither is in the predicate. The blocking IO (S3 GETs / local
        // `std::fs`) offloads off the runtime worker, mirroring `run_drift`.
        let needs_registry = plan::uses_body_equality(&query.predicate);
        let needs_alias_fold = alias_map.is_none() && plan::uses_resolves_to(&query.predicate);
        let mut acquired: Option<AcquiredTemplateMap> = None;
        if needs_alias_fold || needs_registry {
            let (template_map, acquisition_bytes, _outcome) = self
                .spawn_blocking_audit({
                    let backend = self.backend.clone();
                    let tenant = tenant.clone();
                    move || template_map::load_or_derive(backend.store_ref(), &tenant)
                })
                .await?;
            acquired = Some(AcquiredTemplateMap {
                map: template_map,
                acquisition_bytes,
            });
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
            plan::apply(df, plan)
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
        self.spawn_blocking_audit({
            let backend = self.backend.clone();
            let tenant = tenant.clone();
            move || derive_template_registry(backend.store_ref(), &tenant)
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
        mut acquired: Option<AcquiredTemplateMap>,
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

        let ctx = crate::exec::session();
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
            return Ok(empty_result(terminal.aggregate()));
        }

        // Union schema + promoted no-coercion adapter — the rationale
        // lives on `SchemaMode::Union` (epic #745 wave 2).
        let base =
            register_listing_table(&ctx, "logs", urls, SchemaMode::Union(&self.promoted)).await?;
        // A provably-empty filter (absent OPTIONAL column) ⇒ no scan.
        let Some(df) = build_filter(base)? else {
            return Ok(empty_result(terminal.aggregate()));
        };

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
                });
            }
            early = Some(collected);
        }

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
        let (batches, stats) = execute_plan(counted, ctx.task_ctx()).await?;
        let rows = count_value(&batches)?;

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
        // Row-level tenant backstop (`CLAUDE.md` §3.7), mirroring the drift
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
            aggr_exprs
                .push(plan::scalar_agg_expr(*func, path, df.schema())?.alias(plan::VALUE_COLUMN));
        }
        let aggregated = df.aggregate(group_exprs, aggr_exprs).map_err(storage_err)?;
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
        acquired: Option<AcquiredTemplateMap>,
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
        // The single per-query template-map acquisition, measured
        // (RFC 0031 §3.6 / RFC 0033): reuse the compile-time alias fold's
        // map when the query already acquired one, else resolve through
        // the cached read path — the same blocking-pool offload as
        // [`Self::template_registry`].
        let AcquiredTemplateMap {
            map,
            acquisition_bytes,
        } = if let Some(acquired) = acquired {
            acquired
        } else {
            let (map, acquisition_bytes, _outcome) = self
                .spawn_blocking_audit({
                    let backend = self.backend.clone();
                    let tenant = tenant.clone();
                    move || template_map::load_or_derive(backend.store_ref(), &tenant)
                })
                .await?;
            AcquiredTemplateMap {
                map,
                acquisition_bytes,
            }
        };
        Ok(CollectedRecords {
            // The batch constructor memoises per-(id, version) work —
            // one lookup + one formatted string per distinct template,
            // not per row.
            records: LogRow::from_records(&mined, map.registry()),
            scan,
            registry_bytes_read: acquisition_bytes,
        })
    }

    /// Run a blocking audit derivation (`derive_alias_map` /
    /// `derive_template_registry`) on the tokio blocking pool so the async query
    /// path doesn't tie up a runtime worker on the S3 `get_blocking` (or local
    /// `std::fs`) reads — the same offload `run_drift` applies to the listing.
    /// The closure owns its captured `Backend` / `TenantId` clones so it
    /// satisfies the `'static + Send` bound.
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
    /// - **Local backend** ([`Backend::Local`]): walk `std::fs` under
    ///   `<root>/<prefix>` honouring the RFC 0009 §3.4 manifest, then address
    ///   each file by its absolute local path — byte-for-byte the pre-RFC-0019
    ///   read path, with the canonical-path tenant-isolation backstop intact.
    /// - **S3 backend** ([`Backend::Remote`]): list the keys under `prefix`
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
        match &self.backend {
            Backend::Local(root) => {
                let tenant_dir = root.join(prefix);
                let live_files = resolve_live_files(&tenant_dir, window)?;
                local_file_urls(&tenant_dir, &live_files)
            }
            Backend::Remote(store) => {
                let live_keys = resolve_live_keys(store, prefix, window)?;
                object_store_urls(ctx, store, &live_keys)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
