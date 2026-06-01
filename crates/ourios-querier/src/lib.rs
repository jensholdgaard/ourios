//! `ourios-querier` — RFC 0007 querier (pillar #3, `DataFusion`).
//!
//! **Status: red gate (scaffold).** The public API surface from
//! RFC 0007 §4.1 is in place but [`Querier::run`] returns
//! `unimplemented!()`. The `#[ignore]`'d acceptance tests under
//! `tests/` enumerate the RFC 0007 §5 scenarios (RFC0007.1
//! through .5) the implementation must satisfy before the RFC
//! moves `specified → red → green`.
//!
//! This crate is the **read path**: it lowers a logs-DSL query
//! (RFC 0002) to a `DataFusion` `LogicalPlan`, executes it against
//! the RFC 0005 Parquet store with predicate pushdown
//! (partition pruning + row-group skipping on `template_id` /
//! `time_unix_nano` / severity, RFC 0005 §3.3/§3.6), and returns
//! typed results — **without** leaking `DataFusion` or SQL through
//! the public API (hazard `CLAUDE.md` §4.6). It depends on the
//! shipped RFC 0005 reader, not on the WAL or receiver.
//!
//! **Deferred (execution slice):** the DSL→plan lowering and
//! query execution need RFC 0002's still-undecided Branch A/B
//! syntax. RFC 0007 scopes *this* layer — surface, pushdown
//! contract, B1/B2 criteria — as branch-independent, so the
//! scaffold + criteria land now; execution follows RFC 0002.

#![deny(unsafe_code)]

use ourios_core::tenant::TenantId;

/// A logs-DSL query to execute. Per RFC 0007 §3.1 the querier
/// consumes the AST that RFC 0002 produces; that parsed-query
/// field is deferred until RFC 0002's Branch A/B decision lands,
/// so the request currently carries only the tenant scope and
/// optional time bounds (the partition-prune keys).
#[derive(Debug, Clone)]
pub struct QueryRequest {
    /// Tenant whose data the query is scoped to. A query without
    /// a tenant is a usage error, not a cross-tenant scan
    /// (`CLAUDE.md` §3.7; RFC0007.5).
    pub tenant: TenantId,
    /// Optional `[start, end)` `time_unix_nano` bounds — a
    /// partition-prune key (RFC 0005 §3.3 pushdown set).
    pub time_range: Option<(u64, u64)>,
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
    pub stats: QueryStats,
}

/// Errors from [`Querier::run`]. Ourios-owned — no
/// `datafusion::*` / `arrow::*` / SQL types appear here or in
/// any public signature (hazard §4.6; RFC0007.3).
#[derive(Debug)]
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

/// The query engine. One per querier process; reads the RFC 0005
/// Parquet store from object storage. Construction wiring (object
/// store handle, `DataFusion` session) lands with the execution
/// slice.
#[derive(Debug, Default)]
pub struct Querier {}

impl Querier {
    /// Create a querier. (Configuration — object-store root,
    /// session tuning — is added with the execution slice.)
    #[must_use]
    pub fn new() -> Self {
        Self {}
    }

    /// Execute `request` and return the matching rows + pruning
    /// stats. Lowers the query to a `DataFusion` `LogicalPlan`,
    /// runs it against the RFC 0005 store with predicate
    /// pushdown, and returns results without exposing `DataFusion`
    /// (§4.6).
    ///
    /// # Errors
    ///
    /// See [`QueryError`].
    // `async` is part of the RFC 0007 §4.1 API contract (`DataFusion`
    // execution is async); the red-gate stub has no `.await` yet,
    // which is the only reason `clippy::unused_async` fires.
    #[allow(clippy::unused_async)]
    pub async fn run(&self, _request: QueryRequest) -> Result<QueryResult, QueryError> {
        unimplemented!("RFC 0007 red gate — execution pending (§4; blocked on RFC 0002 DSL)");
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
