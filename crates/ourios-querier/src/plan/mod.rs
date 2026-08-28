//! Compile the RFC 0002 query IR ([`dsl::Query`]) to the `DataFusion`
//! execution layer (RFC 0002 §6.5).
//!
//! This module is the bridge between the surface-independent IR and pillar
//! #3 (`DataFusion`). It is **internal**: no `datafusion`/`arrow`/SQL type
//! crosses a public boundary (hazard `CLAUDE.md` §4.6 / RFC0002.3); the only
//! public entry is [`Querier::run_query`](crate::Querier::run_query), which
//! returns the Ourios-owned [`QueryResult`](crate::QueryResult) /
//! [`QueryError`].
//!
//! ## Field → column mapping (RFC 0002 §6.2 / §6.3)
//!
//! First-class fields resolve to the RFC 0005 columns:
//!
//! | DSL field | RFC 0005 column | type |
//! |---|---|---|
//! | `ts` | `time_unix_nano` | `Timestamp(ns, UTC)` |
//! | `observed_ts` | `observed_time_unix_nano` | `Timestamp(ns, UTC)` |
//! | `severity` | `severity_number` | `UInt8` (via the §6.1 floor map) |
//! | `scope` | `scope_name` | `Utf8` |
//! | `trace_id` / `span_id` | the dedicated byte columns | `FixedSizeBinary` |
//! | `body` | `body` | `Binary` |
//! | `template_id` | `template_id` | `UInt64` |
//! | `confidence` | `confidence` | `Float32` |
//! | `lossy` | `lossy_flag` | `Boolean` |
//! | `flags` | `flags` | `UInt32` |
//!
//! The `range(...)` time window is **not** the bare `ts` field: it compiles
//! against the derived `effective_time_unix_nano` column (RFC 0002 §6.2
//! amendment 2026-06-11) via [`crate::time_window_filter`], with the
//! RFC 0005 §3.9 `effective := time_unix_nano` fallback for files that
//! predate the column.
//!
//! `service`, `resource.<k>`, and `attr.<k>` are attribute-backed:
//! resource/log attributes are stored as a single Ourios-canonical-JSON
//! `Utf8` column (`resource_attributes` / `attributes`), plus — for keys in
//! the RFC 0022 promoted set — a dedicated `OPTIONAL Utf8` column named
//! after the DSL path (`resource.<k>` / `attr.<k>`). When the scanned union
//! schema carries a key's promoted column, [`attr_match`] compiles the full
//! `cmp_op` set against it (§3.3's two-arm form for `==`/`!=`, typed-arm
//! only for ordering/regex) and the typed arm prunes row groups. Otherwise
//! the key compiles to a substring/`LIKE` match against the JSON column
//! using a needle built from the canonical
//! `{"key":…,"value":{"stringValue":…}}` shape — honest about the storage,
//! a `Filter` with no row-group-pruning claim (RFC 0002 §5 RFC0002.6),
//! limited to string equality; ordering/regex on a non-promoted
//! (JSON-encoded) attribute stay rejected.
//!
//! ## Absent OPTIONAL columns (RFC 0005 §3.9 / RFC0007.4)
//!
//! A leaf predicate over an OPTIONAL column absent from the (post-union)
//! schema compiles to `false`: an absent column reads as all-NULL, so any
//! comparison is NULL ⇒ no match. Substituting the leaf — rather than the
//! whole query — keeps `and`/`or`/`not` semantics correct, and avoids the
//! planning error that filtering an unknown column would otherwise raise.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use datafusion::arrow::datatypes::{DataType, TimeUnit};
use datafusion::common::{Column, DFSchema, ScalarValue};
use datafusion::dataframe::DataFrame;
use datafusion::functions::expr_fn::{coalesce, get_field, regexp_like, starts_with};
use datafusion::functions_aggregate::expr_fn::{avg, max, min, sum};
use datafusion::functions_nested::expr_fn::array_element;
use datafusion::logical_expr::{Expr, cast, is_not_true, not, try_cast};
use datafusion::prelude::{col, lit};

use ourios_core::alias::AliasMap;
use ourios_core::tenant::TenantId;

use crate::body_match::{BodyLiteralMatch, body_literal_candidates};
use crate::dsl::ir::{
    AggFn, Call, CmpOp, Field, GroupTerm, OrdOp, Predicate, Query, SeverityValue, Stage, Time,
    Value,
};
use crate::template_registry::TemplateRegistry;
use crate::{QueryError, column_type, has_column, time_bound_scalar};
use ourios_parquet::{columns, promoted};

/// A compiled query: the resolved time window (drives both the
/// directory-level partition pruning and the row-level time filter) and the
/// predicate IR, deferred so the `Expr` is built once the union schema is
/// known (for the absent-column guard). Plus the row `limit`, if any.
///
/// `alias_classes` is the eagerly-resolved RFC 0001 §6.7 alias expansion for
/// every `resolves_to(n)` in the predicate: id → the sorted equivalence class
/// (`{n}` when `n` is in no class). It is captured here at `compile` time so
/// `apply`/`compile_predicate` need neither the [`AliasMap`] nor the tenant —
/// the per-tenant resolution has already happened.
pub(crate) struct Plan {
    pub(crate) window: (u64, u64),
    predicate: Predicate,
    alias_classes: BTreeMap<u64, BTreeSet<u64>>,
    /// Per distinct `body ==`/`!=` string literal: the plan-time template
    /// resolution (RFC 0044 §3.1) — candidates from the tenant registry
    /// plus the literal's own separator sequence. Empty when the query has
    /// no body equality.
    body_equalities: BTreeMap<String, BodyEqualityPlan>,
    pub(crate) limit: Option<usize>,
    pub(crate) aggregate: Option<Aggregate>,
    /// RFC 0047 §3.4 layer-2 visibility, applied in [`apply`] as one more
    /// filter over the promoted columns (so it prunes like any predicate).
    visibility: Option<crate::Visibility>,
}

/// A validated aggregation stage (RFC 0002 §6.3/§6.5 amendment 2026-07-15 for
/// `count`; the 2026-07-23 amendment RFC0002.17 for `sum`/`min`/`max`/`avg`):
/// the group terms, already checked against the pinning rule, the
/// positive-bucket-width rule, and the duplicate-term rules.
#[derive(Debug, Clone)]
pub(crate) struct Aggregate {
    pub(crate) by: Vec<GroupTerm>,
    /// The scalar aggregate `func(path)` for a `sum`/`min`/`max`/`avg` stage;
    /// `None` for the bare `count` family. The path is a promoted attribute
    /// read as `Float64` — `Utf8` promoted columns are cast at query time, so
    /// an unparseable value reads as NULL and is excluded (RFC0002.17).
    pub(crate) scalar: Option<(AggFn, Field)>,
}

/// The alias each grouping expression carries in the aggregation plan
/// (`group_0`, `group_1`, …, one per `by` term in query order) — the result
/// decoder addresses the key columns by these names.
pub(crate) fn group_column_name(i: usize) -> String {
    format!("group_{i}")
}

/// The alias of the aggregation plan's count column.
pub(crate) const COUNT_COLUMN: &str = "n";

/// The alias of the aggregation plan's scalar-value column (`sum`/`min`/`max`/
/// `avg`), present only when the stage carries a scalar aggregate.
pub(crate) const VALUE_COLUMN: &str = "v";

/// [`validate`]'s output: the resolved window and limit (as before) plus the
/// validated aggregation stage, if any.
pub(crate) struct Validated {
    pub(crate) window: (u64, u64),
    pub(crate) limit: Option<usize>,
    pub(crate) aggregate: Option<Aggregate>,
}

/// Nanoseconds per duration unit (RFC 0002 §7 `duration`: `s`/`m`/`h`/`d`/`w`).
const NS_PER_SECOND: u64 = 1_000_000_000;

/// Compile the IR to a [`Plan`]: resolve the time window (the `range(...)`
/// stage, or the tenant default `[now - W, now]` when absent — RFC 0002 §4
/// P5, never unbounded) and capture the predicate + `limit` for deferred
/// `Expr` building.
/// The map-independent half of [`compile`]: stage support, window
/// resolution, and the limit bound. `run_query` calls this *before*
/// deriving the alias map so an invalid query fails with its compile
/// error rather than first paying (or surfacing errors from) the
/// audit-tree scan; `compile` runs it again internally — it is pure
/// and cheap, and one source of truth beats a split.
pub(crate) fn validate(
    query: &Query,
    now_unix_nano: u64,
    default_window_nanos: u64,
) -> Result<Validated, QueryError> {
    // This slice executes `range` (time window), `limit`, `count [by …]`
    // (RFC 0002 amendment 2026-07-15), and the `sum`/`min`/`max`/`avg` scalar
    // aggregates (RFC0002.17, 2026-07-23). The remaining sort / projection /
    // render stages parse into a valid IR but are not yet wired to execution;
    // reject them explicitly so a query asking for one fails fast rather than
    // silently returning a plain filtered row set.
    let mut aggregate = None;
    for stage in &query.stages {
        let unsupported = match stage {
            Stage::Range(..) | Stage::Limit(_) => None,
            // A second aggregation stage of either family — the first may have
            // been a `count` or a scalar `sum`/`min`/`max`/`avg` (RFC0002.19).
            Stage::Count { .. } | Stage::Agg { .. } if aggregate.is_some() => {
                return Err(QueryError::InvalidQuery {
                    detail: "a query takes at most one aggregation stage".to_string(),
                });
            }
            Stage::Count { by } => {
                validate_group_terms(by, &query.predicate)?;
                aggregate = Some(Aggregate {
                    by: by.clone(),
                    scalar: None,
                });
                None
            }
            Stage::Agg { func, path, by } => {
                validate_group_terms(by, &query.predicate)?;
                validate_agg_path(path)?;
                aggregate = Some(Aggregate {
                    by: by.clone(),
                    scalar: Some((*func, path.clone())),
                });
                None
            }
            Stage::Sort { .. } => Some("sort"),
            Stage::Project(_) => Some("project"),
            Stage::Render => Some("render"),
        };
        if let Some(name) = unsupported {
            return Err(QueryError::InvalidQuery {
                detail: format!("the `{name}` stage is not yet supported by the querier"),
            });
        }
    }
    let window = resolve_window(&query.stages, now_unix_nano, default_window_nanos)?;
    let limit = query.stages.iter().rev().find_map(|s| match s {
        Stage::Limit(n) => Some(*n),
        _ => None,
    });
    let limit = match limit {
        Some(n) => Some(usize::try_from(n).map_err(|_| QueryError::InvalidQuery {
            detail: format!("limit {n} is too large"),
        })?),
        None => None,
    };
    // `Terminal::Aggregate` executes its own grouped-count scan and never
    // consults `plan.limit` (the aggregation *is* the result — group-limiting
    // semantics are not implemented), so a `count [by …] | limit n` pipeline
    // would silently drop the `limit` instead of applying it. Reject the
    // combination rather than execute the wrong query.
    if limit.is_some() && aggregate.is_some() {
        return Err(QueryError::InvalidQuery {
            detail: "a query with a `count` stage does not support `limit`; \
                     group-limiting semantics are not implemented yet"
                .to_string(),
        });
    }
    Ok(Validated {
        window,
        limit,
        aggregate,
    })
}

/// Validate a `by`-list against the §6.3 amendment rules: `param(n)` only
/// under a single-template pin, positive bucket widths, at most one
/// `bucket(…)`, and at most one `param(n)` per `n`.
fn validate_group_terms(by: &[GroupTerm], predicate: &Predicate) -> Result<(), QueryError> {
    let mut params = BTreeSet::new();
    let mut has_bucket = false;
    for term in by {
        match term {
            GroupTerm::Param(n) if pinned_template_id(predicate).is_none() => {
                // Params are positional *per template*, so grouping across
                // templates by position aggregates unrelated values (§6.3
                // amendment) — rejected, never silently computed.
                return Err(QueryError::InvalidQuery {
                    detail: format!(
                        "param({n}) requires the predicate to pin exactly one template: \
                         a top-level `template_id == <id>` conjunct, with every such \
                         comparison naming the same id (params are positional per \
                         template)"
                    ),
                });
            }
            GroupTerm::Param(n) => {
                if !params.insert(*n) {
                    return Err(QueryError::InvalidQuery {
                        detail: format!("param({n}) appears more than once in the `by` list"),
                    });
                }
            }
            GroupTerm::Bucket(_) if has_bucket => {
                return Err(QueryError::InvalidQuery {
                    detail: "a `by` list takes at most one bucket(...) term".to_string(),
                });
            }
            GroupTerm::Bucket(width) => {
                let nanos = duration_nanos(width)?;
                if nanos == 0 {
                    return Err(QueryError::InvalidQuery {
                        detail: format!("bucket({width}) width must be positive"),
                    });
                }
                // The execution lowering casts the width to `i64` (§6.5
                // floor-division), so a width beyond `i64::MAX` nanoseconds
                // must fail here — one compile-time contract, not a second
                // error path surfacing later during planning.
                if i64::try_from(nanos).is_err() {
                    return Err(QueryError::InvalidQuery {
                        detail: format!("bucket({width}) width exceeds i64 nanoseconds"),
                    });
                }
                has_bucket = true;
            }
            GroupTerm::Field(_) => {}
        }
    }
    Ok(())
}

/// The §6.3 amendment pinning rule, decidable on the associative-normalised
/// IR: the predicate must carry, at its top conjunctive level, at least one
/// `template_id == <N>` comparison, and all such comparisons must name the
/// same `N`. A comparison under `or`/`not` pins nothing, and `resolves_to`
/// does **not** pin (it expands to an alias *set* with no positional param
/// alignment across the class).
fn pinned_template_id(predicate: &Predicate) -> Option<u64> {
    fn pin_of(p: &Predicate) -> Option<u64> {
        match p {
            Predicate::Comparison {
                field: Field::TemplateId,
                op: CmpOp::Ord(OrdOp::Eq),
                value: Value::Int(n),
            } => u64::try_from(*n).ok(),
            _ => None,
        }
    }
    let pins: Vec<u64> = match predicate {
        Predicate::And(terms) => terms.iter().filter_map(pin_of).collect(),
        leaf => pin_of(leaf).into_iter().collect(),
    };
    match pins.split_first() {
        Some((first, rest)) if rest.iter().all(|n| n == first) => Some(*first),
        _ => None,
    }
}

pub(crate) fn compile(
    query: &Query,
    tenant: &TenantId,
    now_unix_nano: u64,
    default_window_nanos: u64,
    alias_map: &AliasMap,
    registry: &TemplateRegistry,
    visibility: Option<crate::Visibility>,
) -> Result<Plan, QueryError> {
    let Validated {
        window,
        limit,
        aggregate,
    } = validate(query, now_unix_nano, default_window_nanos)?;
    // Eagerly resolve every `resolves_to(n)` against the tenant's alias map
    // so the deferred predicate compilation in `apply` is tenant-agnostic.
    let mut alias_classes = BTreeMap::new();
    collect_alias_classes(&query.predicate, tenant, alias_map, &mut alias_classes);
    // Same eager rule for `body ==`/`!=` literals: the RFC 0044 template
    // arm resolves at plan time against the tenant registry the caller
    // acquired (empty when the query has no body equality).
    let mut body_equalities = BTreeMap::new();
    collect_body_equalities(&query.predicate, registry, &mut body_equalities);

    Ok(Plan {
        window,
        predicate: query.predicate.clone(),
        alias_classes,
        body_equalities,
        limit,
        aggregate,
        visibility,
    })
}

/// Whether the predicate contains any `resolves_to(n)` call. The caller uses
/// this to skip the RFC 0005 §3.7.1 alias-map derivation (an audit-tree scan)
/// for the queries that would never consult the map.
pub(crate) fn uses_resolves_to(p: &Predicate) -> bool {
    match p {
        Predicate::Call(Call::ResolvesTo(_)) => true,
        Predicate::Not(inner) => uses_resolves_to(inner),
        Predicate::And(terms) | Predicate::Or(terms) => terms.iter().any(uses_resolves_to),
        Predicate::Bool(_)
        | Predicate::Comparison { .. }
        | Predicate::Severity { .. }
        | Predicate::Call(_) => false,
    }
}

/// The plan-time half of a `body ==`/`!=` literal (RFC 0044 §3.1): the
/// registry candidates the literal unifies with, and the literal's own
/// separator sequence (`tokenize` is lossless, so `separators` is exactly
/// what a byte-identical record must have stored).
#[derive(Debug, Clone)]
pub(crate) struct BodyEqualityPlan {
    candidates: Vec<BodyLiteralMatch>,
    separators: Vec<Vec<u8>>,
}

/// Whether the predicate contains a `body ==`/`!=` string comparison. The
/// caller uses this to decide whether the RFC 0033 template-map acquisition
/// (which carries the RFC 0017 registry) is needed at all.
pub(crate) fn uses_body_equality(p: &Predicate) -> bool {
    match p {
        Predicate::Comparison {
            field: Field::Body,
            op: CmpOp::Ord(OrdOp::Eq | OrdOp::Ne),
            value: Value::Str(_),
        } => true,
        Predicate::Not(inner) => uses_body_equality(inner),
        Predicate::And(terms) | Predicate::Or(terms) => terms.iter().any(uses_body_equality),
        Predicate::Bool(_)
        | Predicate::Comparison { .. }
        | Predicate::Severity { .. }
        | Predicate::Call(_) => false,
    }
}

/// Resolve every distinct `body ==`/`!=` literal against `registry`
/// (RFC 0044 §3.1). A literal the tokenizer rejects resolves to no
/// candidates and an empty separator list — the physical arm alone is
/// exact for it (the parse-failure ingest path retains such bodies).
fn collect_body_equalities(
    p: &Predicate,
    registry: &TemplateRegistry,
    out: &mut BTreeMap<String, BodyEqualityPlan>,
) {
    match p {
        Predicate::Comparison {
            field: Field::Body,
            op: CmpOp::Ord(OrdOp::Eq | OrdOp::Ne),
            value: Value::Str(literal),
        } => {
            if !out.contains_key(literal) {
                let separators = ourios_miner::tokenize::tokenize(literal).map_or_else(
                    |_| Vec::new(),
                    |tk| {
                        tk.separators
                            .iter()
                            .map(|s| s.as_bytes().to_vec())
                            .collect()
                    },
                );
                out.insert(
                    literal.clone(),
                    BodyEqualityPlan {
                        candidates: body_literal_candidates(registry, literal),
                        separators,
                    },
                );
            }
        }
        Predicate::Not(inner) => collect_body_equalities(inner, registry, out),
        Predicate::And(terms) | Predicate::Or(terms) => {
            for term in terms {
                collect_body_equalities(term, registry, out);
            }
        }
        Predicate::Bool(_)
        | Predicate::Comparison { .. }
        | Predicate::Severity { .. }
        | Predicate::Call(_) => {}
    }
}

/// Walk the predicate IR and, for each `resolves_to(n)`, record the tenant's
/// alias expansion `n → resolves(tenant, n)` (RFC 0001 §6.7). Per-tenant
/// resolution `[§3.7]` happens here once; the result rides the [`Plan`].
fn collect_alias_classes(
    p: &Predicate,
    tenant: &TenantId,
    alias_map: &AliasMap,
    out: &mut BTreeMap<u64, BTreeSet<u64>>,
) {
    match p {
        Predicate::Call(Call::ResolvesTo(n)) => {
            out.entry(*n)
                .or_insert_with(|| alias_map.resolves(tenant, *n));
        }
        Predicate::Not(inner) => collect_alias_classes(inner, tenant, alias_map, out),
        Predicate::And(terms) | Predicate::Or(terms) => {
            for term in terms {
                collect_alias_classes(term, tenant, alias_map, out);
            }
        }
        Predicate::Bool(_)
        | Predicate::Comparison { .. }
        | Predicate::Severity { .. }
        | Predicate::Call(_) => {}
    }
}

/// Apply a compiled [`Plan`] to the base `DataFrame`: the time-window filter
/// and the compiled predicate (using the now-known union schema for the
/// absent-column guard). The `limit` is deliberately **not** applied here — it
/// caps the returned `records`, not the count (RFC 0017 §3.4; see the
/// destructure note below). Returns `Ok(None)` when the whole query is provably
/// empty.
pub(crate) fn apply(df: DataFrame, plan: Plan) -> Result<Option<DataFrame>, QueryError> {
    let Plan {
        window: (start, end),
        predicate,
        alias_classes,
        body_equalities,
        // `limit` is **not** applied to this (counted) frame: the count
        // (`QueryResult.rows`) is the total matching rows, and the limit caps
        // only the returned `records` — applied downstream in
        // `Querier::execute` via the `row_limit` it reads from `plan.limit`
        // (RFC 0017 §3.4). Applying it here would wrongly cap the count too.
        limit: _,
        // The aggregation stage is executed by `Querier::execute` on the
        // frame this returns (§6.5 amendment: the group terms lower inside
        // the `Aggregate` node, over the same filtered scan); the caller
        // reads it off the plan before handing the plan here.
        aggregate: _,
        visibility,
    } = plan;
    // The window filters the *effective* timestamp (RFC 0002 §6.2 amendment
    // 2026-06-11), with the RFC 0005 §3.9 fallback for pre-amendment files;
    // the bare `ts` field stays `time_unix_nano`, the verbatim wire value.
    let window_filter = crate::time_window_filter(&df, start, end)?;
    let mut df = df.filter(window_filter).map_err(crate::storage_err)?;

    match compile_predicate(&predicate, df.schema(), &alias_classes, &body_equalities)? {
        // `true` ⇒ match-all ⇒ no predicate filter (window only).
        PredExpr::All => {}
        // `false` ⇒ match-none ⇒ short-circuit to an empty result.
        PredExpr::None => return Ok(None),
        PredExpr::Filter(expr) => {
            df = df.filter(expr).map_err(crate::storage_err)?;
        }
    }

    // RFC 0047 §3.4: the scoped principal's `IN (…)` / self fast path — an
    // ordinary predicate over promoted columns, so it prunes; nothing to
    // see ⇒ an empty result, not an error.
    if let Some(visibility) = &visibility {
        match visibility.filter(&df)? {
            crate::visibility::VisibilityFilter::Nothing => return Ok(None),
            crate::visibility::VisibilityFilter::Everything => {}
            crate::visibility::VisibilityFilter::Only(expr) => {
                df = df.filter(expr).map_err(crate::storage_err)?;
            }
        }
    }

    Ok(Some(df))
}

fn resolve_window(
    stages: &[Stage],
    now: u64,
    default_window: u64,
) -> Result<(u64, u64), QueryError> {
    // The last `range(...)` wins (a later stage overrides an earlier one),
    // matching the left-to-right pipe semantics.
    let range = stages.iter().rev().find_map(|s| match s {
        Stage::Range(from, to) => Some((from, to)),
        _ => None,
    });
    match range {
        Some((from, to)) => {
            let start = resolve_time(from, now)?;
            let end = resolve_time(to, now)?;
            Ok((start.min(end), start.max(end)))
        }
        // No range ⇒ tenant default window `[now - W, now]` (never unbounded).
        None => Ok((now.saturating_sub(default_window), now)),
    }
}

mod group;
mod predicate;
#[cfg(test)]
mod tests;
mod time;

// The split keeps one module surface: the children reach the parent's
// items (and each other's, re-exported here) exactly as before the
// RFC-sized file was cut along its section markers (epic #745 wave 3).
#[allow(clippy::wildcard_imports)] // sibling re-glue after the file split
use group::*;
pub(crate) use group::{group_exprs, scalar_agg_expr};
#[allow(clippy::wildcard_imports)] // sibling re-glue after the file split
use predicate::*;
pub(crate) use time::resolve_time;
#[allow(clippy::wildcard_imports)] // sibling re-glue after the file split
use time::*;
