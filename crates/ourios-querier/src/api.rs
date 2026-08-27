//! The public request/result/error surface of the querier crate
//! (epic #745 wave 1; moved verbatim from the crate root).

// Split from the crate root (epic #745 wave 1); the parent scope is
// the import surface so every pre-split `crate::X` path resolves
// unchanged.
#[allow(clippy::wildcard_imports)]
use super::*;

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
    /// rows is **not** folded into `bytes_read`; it is reported additively as
    /// [`QueryResult::materialize_bytes_read`] /
    /// [`QueryResult::registry_bytes_read`] (RFC 0031 §3.6).
    pub limit: Option<usize>,
}

/// Pruning / IO accounting for one query, surfaced so B1
/// (RFC0007.1) can assert pushdown actually skipped data rather
/// than scanning it. Plain integers — no `DataFusion`/arrow types
/// cross this boundary (hazard §4.6).
///
/// Marked `#[non_exhaustive]` so further additive fields (like
/// `rows_excluded`, RFC0002.15) stay non-breaking.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueryStats {
    /// Row groups `DataFusion` read.
    pub row_groups_scanned: u64,
    /// Row groups skipped via partition/statistics pruning. The
    /// B1 pruned fraction is
    /// `row_groups_pruned / (row_groups_scanned + row_groups_pruned)`.
    pub row_groups_pruned: u64,
    /// Bytes read from object storage by the count/pruning scan. `0` when
    /// the scan was elided ([`QueryOptions::elide_count_scan`]) — the
    /// materialization pass's IO stays on
    /// [`QueryResult::materialize_bytes_read`].
    pub bytes_read: u64,
    /// Rows excluded from an executed aggregation because a group key was
    /// NULL — either `param(n)` on a `params` list shorter than `n + 1` /
    /// a NULL slot, or an `OPTIONAL` field group term (e.g. `bucket`'s
    /// underlying timestamp, `trace_id`) absent on the row (RFC 0002
    /// §6.3 amendment 2026-07-15 / RFC0002.15). Such rows contribute to
    /// no group and there is no synthetic "absent" bucket; this tally
    /// keeps the exclusion observable, not silent. Always `0` for a
    /// non-aggregation query.
    pub rows_excluded: u64,
}

/// Additive execution options for [`Querier::run_query_with`]. The
/// `Default` is byte-for-byte the [`Querier::run_query`] behavior.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct QueryOptions {
    /// Single-pass execution for limited queries (RFC 0031 §3.6): run the
    /// row-materialization scan **first** and, when it proves the result
    /// complete (fewer rows returned than the `limit`, so the limit clipped
    /// nothing), skip the count/pruning scan — `rows` is the returned row
    /// count, and re-scanning the same row groups to count them would be
    /// pure redundant IO. A possibly-truncated result (returned == `limit`)
    /// falls back to the count scan, so `rows` is the full matching total
    /// in every case. Count-only queries (no `limit`) are unaffected.
    ///
    /// Under elision, `stats` diverges from the two-pass shape in exactly
    /// one field: `row_groups_scanned` / `row_groups_pruned` still carry
    /// the count-scan values (the materialize plan prunes by the same
    /// predicate over the same file set, and a limit that was never reached
    /// cannot stop the scan early, so the counts are the ones the count
    /// scan would have reported), but `stats.bytes_read` is `0` — the count
    /// scan genuinely read nothing. The three-component sum with
    /// [`QueryResult::materialize_bytes_read`] /
    /// [`QueryResult::registry_bytes_read`] therefore remains the honest
    /// total IO. Callers needing the pinned "a limited query's `stats`
    /// equal a count-only query's" shape (RFC 0017 §3.4) keep the default.
    pub elide_count_scan: bool,
    /// RFC 0047 §3.4 layer-2 visibility: `None` = no rewrite (open mode,
    /// or a resolver without the graph); `Some` = the caller's two-step
    /// decision, applied at plan time (see [`Visibility`]).
    pub visibility: Option<Visibility>,
}

impl QueryOptions {
    /// The RFC 0031 single-pass profile: elide the count scan whenever the
    /// materialized result is complete.
    #[must_use]
    pub const fn single_pass() -> Self {
        Self {
            elide_count_scan: true,
            visibility: None,
        }
    }

    /// These options with the RFC 0047 §3.4 visibility decision attached.
    #[must_use]
    pub fn with_visibility(mut self, visibility: Visibility) -> Self {
        self.visibility = Some(visibility);
        self
    }
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
    /// Bytes read from storage by the row-materialization pass that fetched
    /// the ≤ `limit` returned `records` (RFC 0031 §3.6). `0` for a
    /// count-only query. Additive to `stats.bytes_read`, which keeps its
    /// count/pruning-scan-only meaning (B1/B2 gates and the RFC 0016
    /// metrics depend on that semantics) — a caller wanting the honest
    /// total IO for one query sums the three components.
    pub materialize_bytes_read: u64,
    /// The executed aggregation's grouped-count map (RFC 0002 §6.5
    /// amendment 2026-07-15) — `Some` iff the query carried a `count [by …]`
    /// stage. Each group's `key` holds one entry per `by` term in query
    /// order (a bare `count` is one group with an empty key); groups are
    /// sorted by key so the output is deterministic. This is the
    /// `(bucket, group_key) → count` map RFC 0031 §3.5 compares for L4
    /// equivalence; the plain-string shape keeps it engine-free (§4.6) and
    /// directly serializable on the RFC 0016 endpoint.
    pub aggregate: Option<Vec<AggregateGroup>>,
    /// **Template-map acquisition bytes** (RFC 0033 §3.6, amending the
    /// pre-0033 audit-stream-only meaning): the total bytes fetched to
    /// obtain the body-rendering capability behind the returned `records`,
    /// whatever the source — the audit-stream fold on a cache miss
    /// (byte-for-byte the pre-0033 RFC 0017 §3.2 registry derivation) or
    /// the `template_map.v2.json.zst` artifact GET on a cache hit. One
    /// per-query acquisition serves both the registry and, for
    /// `resolves_to` queries, the alias map. `0` when no rows were
    /// rendered. Same additive contract as `materialize_bytes_read`
    /// (RFC 0031 §3.6).
    pub registry_bytes_read: u64,
}

/// One group of an executed `count [by …]` stage (RFC 0002 §6.3/§6.5
/// amendment 2026-07-15). Plain owned strings — no `datafusion`/`arrow`
/// type crosses this boundary (hazard §4.6).
///
/// Marked `#[non_exhaustive]` so further additive fields stay
/// non-breaking, matching `QueryResult`/`QueryStats`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct AggregateGroup {
    /// The group-key values, one per `by` term in query order: a field or
    /// `param(n)` as its stored string form, `bucket(width)` as the RFC 3339
    /// UTC start of the half-open window `[k·width, (k+1)·width)`. Empty for a
    /// bare aggregation with no `by` — a `count` or a scalar `sum`/`min`/`max`/
    /// `avg` (the single group folding every matching row).
    pub key: Vec<String>,
    /// The number of matching rows in this group (always populated — a scalar
    /// aggregate also carries its group's `COUNT(*)`).
    pub count: u64,
    /// The scalar aggregate outcome for this group. The two nested `Option`s
    /// carry two distinct facts the result surface must keep apart
    /// (RFC0002.18 / RFC0002.20):
    /// - `None` — a bare `count` query; no scalar was requested (the surface
    ///   omits the field entirely).
    /// - `Some(None)` — a `sum`/`min`/`max`/`avg` whose inputs were all NULL /
    ///   unparseable, so the scalar itself is NULL (surfaced as `null`).
    /// - `Some(Some(v))` — the scalar value.
    ///
    /// `f64` (a `Utf8` promoted column is cast to `Float64`), so this type is
    /// not `Eq`.
    pub value: Option<Option<f64>>,
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
    /// The query reads a column the principal may not read (RFC 0047
    /// §3.4 masking — a filter or aggregation on a content column). Names
    /// the column: it is configuration, not data.
    Forbidden { column: String },
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
            Self::Forbidden { column } => {
                write!(f, "column `{column}` is not readable by this principal")
            }
        }
    }
}

impl std::error::Error for QueryError {}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::super::*;

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
