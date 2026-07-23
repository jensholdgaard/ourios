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
use datafusion::common::{Column, ScalarValue};
use datafusion::dataframe::DataFrame;
use datafusion::functions::expr_fn::{coalesce, get_field, regexp_like, starts_with};
use datafusion::functions_aggregate::expr_fn::{avg, max, min, sum};
use datafusion::functions_nested::expr_fn::array_element;
use datafusion::logical_expr::{Expr, cast, not, try_cast};
use datafusion::prelude::{col, lit};

use ourios_core::alias::AliasMap;
use ourios_core::tenant::TenantId;

use crate::dsl::ir::{
    AggFn, Call, CmpOp, Field, GroupTerm, OrdOp, Predicate, Query, SeverityValue, Stage, Time,
    Value,
};
use crate::{QueryError, has_column, time_bound_scalar};
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
    pub(crate) limit: Option<usize>,
    pub(crate) aggregate: Option<Aggregate>,
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
            Stage::Count { .. } if aggregate.is_some() => {
                return Err(QueryError::InvalidQuery {
                    detail: "a query takes at most one `count` stage".to_string(),
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
            Stage::Agg { .. } if aggregate.is_some() => {
                return Err(QueryError::InvalidQuery {
                    detail: "a query takes at most one aggregation stage".to_string(),
                });
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

    Ok(Plan {
        window,
        predicate: query.predicate.clone(),
        alias_classes,
        limit,
        aggregate,
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
    } = plan;
    // The window filters the *effective* timestamp (RFC 0002 §6.2 amendment
    // 2026-06-11), with the RFC 0005 §3.9 fallback for pre-amendment files;
    // the bare `ts` field stays `time_unix_nano`, the verbatim wire value.
    let window_filter = crate::time_window_filter(&df, start, end)?;
    let mut df = df.filter(window_filter).map_err(crate::storage_err)?;

    match compile_predicate(&predicate, &df, &alias_classes)? {
        // `true` ⇒ match-all ⇒ no predicate filter (window only).
        PredExpr::All => {}
        // `false` ⇒ match-none ⇒ short-circuit to an empty result.
        PredExpr::None => return Ok(None),
        PredExpr::Filter(expr) => {
            df = df.filter(expr).map_err(crate::storage_err)?;
        }
    }

    Ok(Some(df))
}

/// Lower a validated `by`-list to the `Aggregate` node's grouping
/// expressions (§6.5 amendment 2026-07-15), one per term in query order,
/// aliased `group_0`, `group_1`, … ([`group_column_name`]). Deferred to
/// execution (like the predicate) because the lowering consults the scanned
/// union schema: the promoted `service` column, the effective-time column,
/// and the absent-OPTIONAL-column guard.
///
/// - A **field** lowers to its RFC 0005 column ([`column_of`]); `service`
///   to the RFC 0022 promoted `resource.service.name` column. An absent
///   OPTIONAL column lowers to a typed NULL literal — the column reads
///   all-NULL (RFC0007.4), so every row's key is NULL, i.e. excluded and
///   tallied rather than invented.
/// - **`param(n)`** lowers to a list-element extraction over the `params`
///   column: element `n` (`array_element` is 1-based) of the
///   `List<Struct{type_tag, value}>`, then the struct's `value` — the
///   stored string bytes (§6.3: the group key is the stored string form,
///   never a type promotion). A short list or NULL slot yields NULL — the
///   excluded disposition.
/// - **`bucket(width)`** lowers to floor division of the effective
///   timestamp by the width: the §6.2 `effective_time_unix_nano` column
///   with the §3.9 `time_unix_nano` fallback (`coalesce`), cast to whole
///   nanoseconds, floored to the window start `k·width`, and cast back to a
///   UTC timestamp. Stored timestamps are non-negative, so integer division
///   *is* the §6.5 floor division.
pub(crate) fn group_exprs(by: &[GroupTerm], df: &DataFrame) -> Result<Vec<Expr>, QueryError> {
    by.iter()
        .enumerate()
        .map(|(i, term)| {
            let expr = match term {
                GroupTerm::Field(field) => field_group_expr(field, df)?,
                GroupTerm::Param(n) => get_field(
                    array_element(col(columns::PARAMS), lit(i64::from(*n) + 1)),
                    "value",
                ),
                GroupTerm::Bucket(width) => bucket_expr(width, df)?,
            };
            Ok(expr.alias(group_column_name(i)))
        })
        .collect()
}

/// Lower a group-by on a promoted attribute column (RFC 0037 §3.3). Groups on
/// the promoted `resource.<key>` / `attr.<key>` column when it is present in
/// the scanned union schema; otherwise rejects with a hint pointing at
/// promotion, so grouping never silently degrades to a single NULL bucket or
/// an unpruned JSON scan.
fn group_by_promoted(column: &str, key: &str, df: &DataFrame) -> Result<Expr, QueryError> {
    let name = promoted_column_name(column, key);
    if has_column(df, &name) {
        Ok(Expr::Column(Column::new_unqualified(name)))
    } else {
        // Name the raw config key (no `attr.`/`resource.` prefix) and the
        // sublist it belongs under, so the hint points at the exact string to
        // add rather than the derived column name.
        let sublist = if column == columns::RESOURCE_ATTRIBUTES {
            "resource"
        } else {
            "log"
        };
        Err(QueryError::InvalidQuery {
            detail: format!(
                "grouping by '{name}' requires the attribute to be promoted to a column present \
                 in the queried range; add '{key}' to storage.promoted_attributes.{sublist}"
            ),
        })
    }
}

fn field_group_expr(field: &Field, df: &DataFrame) -> Result<Expr, QueryError> {
    match field {
        Field::Service => {
            let name =
                promoted_column_name(columns::RESOURCE_ATTRIBUTES, promoted::SERVICE_NAME_KEY);
            if has_column(df, &name) {
                Ok(Expr::Column(Column::new_unqualified(name)))
            } else {
                Ok(lit(ScalarValue::Utf8(None)))
            }
        }
        // RFC 0037 §3.3: group by a *promoted* attribute column. The column
        // is present in the scanned union schema exactly when ≥ 1 scanned
        // file promoted the key (DataFusion supplies per-file NULLs for any
        // pre-promotion partitions within a mixed scan — the typed-NULL
        // fallback happens for free). Absent from every scanned file, the key
        // is not a usable group key here: reject with a promotion hint rather
        // than collapse every row into one NULL bucket or group over an
        // unpruned JSON scan (hazard #6).
        Field::Resource(key) => group_by_promoted(columns::RESOURCE_ATTRIBUTES, key, df),
        Field::Attr(key) => group_by_promoted(columns::ATTRIBUTES, key, df),
        _ => {
            let (column, optional) = column_of(field);
            if optional && !has_column(df, column) {
                Ok(lit(null_scalar_for(field)))
            } else {
                Ok(col(column))
            }
        }
    }
}

/// Compile a `sum`/`min`/`max`/`avg(path)` scalar aggregate to its `DataFusion`
/// `Expr` (RFC0002.17). The path is a promoted attribute column, resolved like
/// a group key ([`group_by_promoted`] — same presence check and promotion-hint
/// error), then `try_cast` to `Float64` so a `Utf8` promoted column aggregates
/// numerically. `try_cast` (not `cast`) is deliberate: an unparseable value
/// yields NULL rather than erroring the query, and the aggregate skips NULLs,
/// so a dirty value neither fails the query nor contributes (RFC0002.18).
pub(crate) fn scalar_agg_expr(
    func: AggFn,
    path: &Field,
    df: &DataFrame,
) -> Result<Expr, QueryError> {
    let column = match path {
        Field::Attr(key) => group_by_promoted(columns::ATTRIBUTES, key, df)?,
        Field::Resource(key) => group_by_promoted(columns::RESOURCE_ATTRIBUTES, key, df)?,
        _ => return Err(agg_path_error()),
    };
    let numeric = try_cast(column, DataType::Float64);
    Ok(match func {
        AggFn::Sum => sum(numeric),
        AggFn::Min => min(numeric),
        AggFn::Max => max(numeric),
        AggFn::Avg => avg(numeric),
    })
}

/// The structural check (in [`validate`], before the union schema is known):
/// a scalar aggregate path must be a promoted attribute (`attr.<k>` /
/// `resource.<k>`). The promoted-presence check happens later in
/// [`scalar_agg_expr`], once the scanned schema is known.
fn validate_agg_path(path: &Field) -> Result<(), QueryError> {
    match path {
        Field::Attr(_) | Field::Resource(_) => Ok(()),
        _ => Err(agg_path_error()),
    }
}

fn agg_path_error() -> QueryError {
    QueryError::InvalidQuery {
        detail: "sum/min/max/avg require a promoted attribute path \
                 (attr.<key> or resource.<key>)"
            .to_string(),
    }
}

/// The typed NULL an absent `OPTIONAL` group-by column stands in for, so
/// the aggregate plan's output schema does not depend on which columns
/// happen to be present in the scanned union schema (every `optional`
/// arm of [`column_of`] must have a case here — the module doc's field
/// table §6.2 is the source of truth for these types).
fn null_scalar_for(field: &Field) -> ScalarValue {
    match field {
        Field::Body => ScalarValue::Binary(None),
        Field::ObservedTs => ScalarValue::TimestampNanosecond(None, Some("UTC".into())),
        Field::TraceId => ScalarValue::FixedSizeBinary(16, None),
        Field::SpanId => ScalarValue::FixedSizeBinary(8, None),
        Field::Scope | Field::EventName => ScalarValue::Utf8(None),
        // `column_of` never marks these `optional` (Service/Resource/Attr
        // are intercepted earlier, before this function is reached), so
        // this arm is unreachable from every real call site — kept only
        // for match exhaustiveness over `Field`.
        Field::Severity
        | Field::Ts
        | Field::Flags
        | Field::TemplateId
        | Field::Confidence
        | Field::Lossy
        | Field::Service
        | Field::Resource(_)
        | Field::Attr(_) => {
            unreachable!("{field:?} is never an OPTIONAL group-by column")
        }
    }
}

fn bucket_expr(width: &str, df: &DataFrame) -> Result<Expr, QueryError> {
    let w = i64::try_from(duration_nanos(width)?).map_err(|_| QueryError::InvalidQuery {
        detail: format!("bucket({width}) width exceeds i64 nanoseconds"),
    })?;
    let ts = col(columns::TIME_UNIX_NANO);
    let effective = if has_column(df, columns::EFFECTIVE_TIME_UNIX_NANO) {
        coalesce(vec![col(columns::EFFECTIVE_TIME_UNIX_NANO), ts])
    } else {
        ts
    };
    let ns = cast(effective, DataType::Int64);
    Ok(cast(
        ns / lit(w) * lit(w),
        DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
    ))
}

/// The result of compiling a predicate against a known schema.
enum PredExpr {
    /// Match-all (`true`) — no filter.
    All,
    /// Match-none (`false`) — provably empty.
    None,
    /// A `DataFusion` boolean filter expression.
    Filter(Expr),
}

impl PredExpr {
    /// Collapse to a concrete `Expr` for embedding inside `and`/`or`/`not`,
    /// where match-all is `true` and match-none is `false`.
    fn into_expr(self) -> Expr {
        match self {
            PredExpr::All => lit(true),
            PredExpr::None => lit(false),
            PredExpr::Filter(e) => e,
        }
    }
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

/// Resolve a §7 [`Time`] bound to absolute nanoseconds against `now`. Shared
/// with the RFC 0010 drift path (`crate::drift`), which reuses the same `time`
/// grammar for its window (RFC 0010 §6.5).
pub(crate) fn resolve_time(time: &Time, now: u64) -> Result<u64, QueryError> {
    match time {
        Time::Now => Ok(now),
        Time::Duration { neg, literal } => {
            let d = duration_nanos(literal)?;
            if *neg {
                Ok(now.saturating_sub(d))
            } else {
                Ok(now.saturating_add(d))
            }
        }
        Time::Timestamp(s) => timestamp_nanos(s),
    }
}

/// Parse a `<int><unit>` duration lexeme (the parser already validated its
/// shape) into nanoseconds.
fn duration_nanos(literal: &str) -> Result<u64, QueryError> {
    let invalid = || QueryError::InvalidQuery {
        detail: format!("duration {literal:?} is not resolvable"),
    };
    let (digits, unit) = literal.split_at(literal.len().checked_sub(1).ok_or_else(invalid)?);
    let n: u64 = digits.parse().map_err(|_| invalid())?;
    let per_unit = match unit {
        "s" => NS_PER_SECOND,
        "m" => 60 * NS_PER_SECOND,
        "h" => 3_600 * NS_PER_SECOND,
        "d" => 86_400 * NS_PER_SECOND,
        "w" => 7 * 86_400 * NS_PER_SECOND,
        _ => return Err(invalid()),
    };
    n.checked_mul(per_unit).ok_or_else(invalid)
}

/// Resolve an RFC 3339 timestamp lexeme to nanoseconds since the epoch.
fn timestamp_nanos(s: &str) -> Result<u64, QueryError> {
    let dt = chrono::DateTime::parse_from_rfc3339(s).map_err(|_| QueryError::InvalidQuery {
        detail: format!("timestamp {s:?} is not a resolvable RFC 3339 instant"),
    })?;
    let ns = dt
        .timestamp_nanos_opt()
        .ok_or_else(|| QueryError::InvalidQuery {
            detail: format!("timestamp {s:?} is out of the representable range"),
        })?;
    u64::try_from(ns).map_err(|_| QueryError::InvalidQuery {
        detail: format!("timestamp {s:?} predates the epoch"),
    })
}

fn compile_predicate(
    p: &Predicate,
    df: &DataFrame,
    alias_classes: &BTreeMap<u64, BTreeSet<u64>>,
) -> Result<PredExpr, QueryError> {
    match p {
        Predicate::Bool(true) => Ok(PredExpr::All),
        Predicate::Bool(false) => Ok(PredExpr::None),
        Predicate::Not(inner) => match compile_predicate(inner, df, alias_classes)? {
            PredExpr::All => Ok(PredExpr::None),
            PredExpr::None => Ok(PredExpr::All),
            PredExpr::Filter(e) => Ok(PredExpr::Filter(not(e))),
        },
        Predicate::And(terms) => combine(terms, df, alias_classes, true),
        Predicate::Or(terms) => combine(terms, df, alias_classes, false),
        Predicate::Comparison { field, op, value } => compile_comparison(field, *op, value, df),
        Predicate::Severity { op, value } => Ok(compile_severity(*op, value)),
        Predicate::Call(call) => compile_call(call, df, alias_classes),
    }
}

fn combine(
    terms: &[Predicate],
    df: &DataFrame,
    alias_classes: &BTreeMap<u64, BTreeSet<u64>>,
    is_and: bool,
) -> Result<PredExpr, QueryError> {
    let mut acc: Option<Expr> = None;
    for term in terms {
        match (compile_predicate(term, df, alias_classes)?, is_and) {
            // `x and true` = x ; `x or false` = x — drop the identity term.
            (PredExpr::All, true) | (PredExpr::None, false) => {}
            // `x and false` = false (whole conjunction is empty).
            (PredExpr::None, true) => return Ok(PredExpr::None),
            // `x or true` = true (whole disjunction is match-all).
            (PredExpr::All, false) => return Ok(PredExpr::All),
            (other, _) => {
                let e = other.into_expr();
                acc = Some(match acc {
                    Some(a) if is_and => a.and(e),
                    Some(a) => a.or(e),
                    None => e,
                });
            }
        }
    }
    Ok(match acc {
        Some(e) => PredExpr::Filter(e),
        // Empty after dropping identities: `and []` = true, `or []` = false.
        None if is_and => PredExpr::All,
        None => PredExpr::None,
    })
}

fn compile_severity(op: OrdOp, value: &SeverityValue) -> PredExpr {
    // `severity_number` is REQUIRED (always present), so no absent-column
    // guard is needed. Compare as i64; DataFusion coerces against UInt8.
    let sev = || col(columns::SEVERITY_NUMBER);
    // A bare name denotes a four-wide OTel band (`error` → 17..=20), so
    // membership (`==`/`!=`) is a range test, not a single-value compare. A
    // numeric RHS is exact, and ordering ops use the band floor either way
    // (RFC0002.5: ordering compares against the floor of the named band).
    let expr = match (value, op) {
        (SeverityValue::Name(name), OrdOp::Eq) => sev()
            .gt_eq(lit(name.floor()))
            .and(sev().lt_eq(lit(name.ceil()))),
        (SeverityValue::Name(name), OrdOp::Ne) => {
            sev().lt(lit(name.floor())).or(sev().gt(lit(name.ceil())))
        }
        (SeverityValue::Name(name), _) => ord_expr(sev(), op, lit(name.floor())),
        (SeverityValue::Number(n), _) => ord_expr(sev(), op, lit(*n)),
    };
    PredExpr::Filter(expr)
}

fn compile_comparison(
    field: &Field,
    op: CmpOp,
    value: &Value,
    df: &DataFrame,
) -> Result<PredExpr, QueryError> {
    match field {
        // Attribute-backed fields have no dedicated column (JSON storage).
        Field::Service => attr_match(columns::RESOURCE_ATTRIBUTES, "service.name", op, value, df),
        Field::Resource(key) => attr_match(columns::RESOURCE_ATTRIBUTES, key, op, value, df),
        Field::Attr(key) => attr_match(columns::ATTRIBUTES, key, op, value, df),
        _ => column_comparison(field, op, value, df),
    }
}

/// A comparison over a field that maps to a dedicated RFC 0005 column.
fn column_comparison(
    field: &Field,
    op: CmpOp,
    value: &Value,
    df: &DataFrame,
) -> Result<PredExpr, QueryError> {
    let (column, optional) = column_of(field);
    // Regex operators are defined only over text columns. A numeric /
    // boolean / binary / timestamp column has no regex semantics, so reject
    // it at compile (before the absent-column guard) rather than building a
    // doomed engine call.
    if matches!(op, CmpOp::Match | CmpOp::NotMatch) && !is_text_field(field) {
        return Err(QueryError::InvalidQuery {
            detail: format!(
                "the regex operators =~ / !~ are not defined on {}",
                field_name(field)
            ),
        });
    }
    // Absent OPTIONAL column ⇒ all-NULL ⇒ the leaf matches nothing.
    if optional && !has_column(df, column) {
        return Ok(PredExpr::None);
    }
    let expr = match op {
        CmpOp::Ord(ord) => ord_expr(col(column), ord, field_literal(field, value)?),
        CmpOp::Match => regexp_like(col(column), string_literal(field, value)?, None),
        CmpOp::NotMatch => not(regexp_like(
            col(column),
            string_literal(field, value)?,
            None,
        )),
    };
    Ok(PredExpr::Filter(expr))
}

/// Build the comparison literal for a first-class column field, mapping the
/// IR [`Value`] to the column's stored type.
fn field_literal(field: &Field, value: &Value) -> Result<Expr, QueryError> {
    let type_err = |want: &str| QueryError::InvalidQuery {
        detail: format!("{} expects a {want} literal", field_name(field)),
    };
    match field {
        Field::Ts | Field::ObservedTs => match value {
            Value::Timestamp(s) => Ok(lit(time_bound_scalar(timestamp_nanos(s)?)?)),
            Value::Int(n) => {
                let ns = u64::try_from(*n).map_err(|_| type_err("non-negative timestamp"))?;
                Ok(lit(time_bound_scalar(ns)?))
            }
            _ => Err(type_err("timestamp")),
        },
        Field::TraceId | Field::SpanId => match value {
            Value::Str(s) => Ok(lit(hex_bytes(field, s)?)),
            _ => Err(type_err("hex-string")),
        },
        Field::TemplateId => match value {
            Value::Int(n) => u64::try_from(*n)
                .map(lit)
                .map_err(|_| type_err("non-negative integer")),
            _ => Err(type_err("integer")),
        },
        Field::Confidence => match value {
            // `confidence` is a Float32 column; a DSL number literal narrows
            // to f32 to match it. The miner emits confidences in [0, 1], so
            // a comparison literal's precision narrowing is intended.
            #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
            Value::Float(f) => Ok(lit(*f as f32)),
            #[allow(clippy::cast_precision_loss)]
            Value::Int(n) => Ok(lit(*n as f32)),
            _ => Err(type_err("number")),
        },
        Field::Lossy => match value {
            Value::Bool(b) => Ok(lit(*b)),
            _ => Err(type_err("boolean")),
        },
        Field::Flags => match value {
            Value::Int(n) => u32::try_from(*n)
                .map(lit)
                .map_err(|_| type_err("u32 integer")),
            _ => Err(type_err("integer")),
        },
        // `body` / `scope` and the attribute fields compare against text.
        _ => string_literal(field, value),
    }
}

fn string_literal(field: &Field, value: &Value) -> Result<Expr, QueryError> {
    match value {
        Value::Str(s) => Ok(lit(s.clone())),
        _ => Err(QueryError::InvalidQuery {
            detail: format!("{} expects a string literal", field_name(field)),
        }),
    }
}

/// Hex-decode a `trace_id` (16 bytes) / `span_id` (8 bytes) literal,
/// case-insensitive, to match the stored `FixedSizeBinary` column (§6.2).
fn hex_bytes(field: &Field, s: &str) -> Result<Vec<u8>, QueryError> {
    let want = match field {
        Field::TraceId => 16,
        _ => 8,
    };
    let err = || QueryError::InvalidQuery {
        detail: format!("{} expects a {}-hex-digit id", field_name(field), want * 2),
    };
    if s.len() != want * 2 {
        return Err(err());
    }
    let mut bytes = Vec::with_capacity(want);
    let raw = s.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        let hi = (raw[i] as char).to_digit(16).ok_or_else(err)?;
        let lo = (raw[i + 1] as char).to_digit(16).ok_or_else(err)?;
        #[allow(clippy::cast_possible_truncation)]
        bytes.push(((hi << 4) | lo) as u8);
        i += 2;
    }
    Ok(bytes)
}

/// Resolve a first-class field to its `(column, is_optional)` per RFC 0005.
fn column_of(field: &Field) -> (&'static str, bool) {
    match field {
        Field::Body => (columns::BODY, true),
        Field::Severity => (columns::SEVERITY_NUMBER, false),
        Field::Ts => (columns::TIME_UNIX_NANO, false),
        Field::ObservedTs => (columns::OBSERVED_TIME_UNIX_NANO, true),
        Field::TraceId => (columns::TRACE_ID, true),
        Field::SpanId => (columns::SPAN_ID, true),
        Field::Scope => (columns::SCOPE_NAME, true),
        Field::EventName => (columns::EVENT_NAME, true),
        Field::Flags => (columns::FLAGS, false),
        Field::TemplateId => (columns::TEMPLATE_ID, false),
        Field::Confidence => (columns::CONFIDENCE, false),
        Field::Lossy => (columns::LOSSY_FLAG, false),
        // Attribute fields are handled before this is reached.
        Field::Service | Field::Resource(_) | Field::Attr(_) => {
            (columns::RESOURCE_ATTRIBUTES, false)
        }
    }
}

/// Whether a field maps to a text-typed column the regex operators
/// (`=~`/`!~`) and `DataFusion` string functions can apply to. The attribute
/// fields (`service`/`resource`/`attr`) are JSON-text-backed but are routed
/// through [`attr_match`] before this is consulted (the only caller is on the
/// dedicated-column path in [`column_comparison`]), so the only text columns
/// reaching here are `body`, `scope`, and `event_name`.
fn is_text_field(field: &Field) -> bool {
    matches!(field, Field::Body | Field::Scope | Field::EventName)
}

fn field_name(field: &Field) -> String {
    match field {
        Field::Body => "body".into(),
        Field::Severity => "severity".into(),
        Field::Ts => "ts".into(),
        Field::ObservedTs => "observed_ts".into(),
        Field::TraceId => "trace_id".into(),
        Field::SpanId => "span_id".into(),
        Field::Scope => "scope".into(),
        Field::EventName => "event_name".into(),
        Field::Flags => "flags".into(),
        Field::Service => "service".into(),
        Field::TemplateId => "template_id".into(),
        Field::Confidence => "confidence".into(),
        Field::Lossy => "lossy".into(),
        Field::Resource(k) => format!("resource.{k}"),
        Field::Attr(k) => format!("attr.{k}"),
    }
}

/// Compile an attribute comparison (`service`/`resource.k`/`attr.k`).
///
/// When the scanned union schema carries the key's RFC 0022 promoted column
/// (`resource.<k>` / `attr.<k>` — §3.4's compile rule), the operator set is
/// the full `cmp_op` (§3.3):
///
/// - `==`/`!=` compile to the two-arm form — the typed column arm (prunable)
///   `OR` a `P IS NULL AND <JSON arm>` fallback covering pre-amendment files
///   and non-string values.
/// - Ordering and regex compile against the typed arm only; the JSON arm
///   cannot express them, so rows whose promoted cell is `NULL`
///   (pre-amendment files, non-string values) never match — §3.3's
///   documented silent non-match, consistent with the DSL's missing-field
///   rule.
///
/// Without the promoted column, `==`/`!=` on a string value keep the #146
/// substring `LIKE` over the Ourios-canonical-JSON column (the canonical
/// encoding stores string values as
/// `{"key":"<k>","value":{"stringValue":"<v>"}}`, so an exact
/// key+string-value pair is matched by that JSON fragment), and every other
/// operator is rejected — unchanged pre-RFC 0022 behaviour.
fn attr_match(
    column: &str,
    key: &str,
    op: CmpOp,
    value: &Value,
    df: &DataFrame,
) -> Result<PredExpr, QueryError> {
    // Both attribute columns are REQUIRED, but guard for the union schema
    // anyway (a future writer could make them OPTIONAL).
    if !has_column(df, column) {
        return Ok(PredExpr::None);
    }
    let Value::Str(v) = value else {
        return Err(QueryError::InvalidQuery {
            detail: "attribute comparisons take a string value in this query surface".to_string(),
        });
    };
    let promoted_name = promoted_column_name(column, key);
    // `col()` parses dotted names as qualified references, so the promoted
    // column (literally named `resource.<k>` / `attr.<k>`) must be addressed
    // as an unqualified `Column` built directly.
    let promoted = has_column(df, &promoted_name)
        .then(|| Expr::Column(Column::new_unqualified(promoted_name)));
    let eq = match op {
        CmpOp::Ord(OrdOp::Eq) => true,
        CmpOp::Ord(OrdOp::Ne) => false,
        op => {
            let Some(p) = promoted else {
                return Err(QueryError::InvalidQuery {
                    detail: "non-promoted attributes support only == / != in this query surface"
                        .to_string(),
                });
            };
            let expr = match op {
                CmpOp::Ord(ord) => ord_expr(p, ord, lit(v.clone())),
                CmpOp::Match => regexp_like(p, lit(v.clone()), None),
                CmpOp::NotMatch => not(regexp_like(p, lit(v.clone()), None)),
            };
            return Ok(PredExpr::Filter(expr));
        }
    };
    // The canonical JSON fragment for this key/value pair. `serde_json`'s
    // string escaping is deterministic, so building the needle with the same
    // serializer the writer uses keeps it byte-aligned with stored rows.
    let needle_value = serde_json::to_string(v).map_err(|e| QueryError::InvalidQuery {
        detail: format!("attribute value is not encodable: {e}"),
    })?;
    let needle_key = serde_json::to_string(key).map_err(|e| QueryError::InvalidQuery {
        detail: format!("attribute key is not encodable: {e}"),
    })?;
    let fragment = format!("{{\"key\":{needle_key},\"value\":{{\"stringValue\":{needle_value}}}}}");
    let value_match = col(column).like(lit(format!("%{}%", like_escape(&fragment))));
    let json = if eq {
        // `==` matches when the key is present with this exact string value.
        value_match
    } else {
        // `!=` must require the key PRESENT with a *different* value: a row
        // missing the key does not match. The presence guard matches the key
        // with any string value, then we exclude the exact value above.
        // Without the guard, `NOT LIKE` is also true for absent keys, which
        // diverges from the missing-field "no match" semantics used
        // everywhere else.
        let key_present = format!("{{\"key\":{needle_key},\"value\":{{\"stringValue\":");
        let presence = col(column).like(lit(format!("%{}%", like_escape(&key_present))));
        presence.and(not(value_match))
    };
    let Some(p) = promoted else {
        return Ok(PredExpr::Filter(json));
    };
    // §3.3's two-arm form. The `!=` typed arm keeps the presence check
    // explicit (`P IS NOT NULL AND P != v`) rather than leaning on 3-valued
    // logic, mirroring the JSON arm's presence guard.
    let expr = if eq {
        p.clone().eq(lit(v.clone())).or(p.is_null().and(json))
    } else {
        p.clone()
            .is_not_null()
            .and(p.clone().not_eq(lit(v.clone())))
            .or(p.is_null().and(json))
    };
    Ok(PredExpr::Filter(expr))
}

/// The RFC 0022 promoted column name for an attribute key: the literal DSL
/// path (`resource.<k>` / `attr.<k>`, §3.1), derived from the same prefixes
/// the writer's [`ourios_parquet::promoted`] module declares.
fn promoted_column_name(column: &str, key: &str) -> String {
    match column {
        columns::RESOURCE_ATTRIBUTES => format!("{}{key}", promoted::RESOURCE_PREFIX),
        _ => format!("{}{key}", promoted::ATTR_PREFIX),
    }
}

/// Escape the `%` / `_` / `\` wildcards in a `LIKE` pattern literal so the
/// JSON fragment matches as plain text.
fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn compile_call(
    call: &Call,
    df: &DataFrame,
    alias_classes: &BTreeMap<u64, BTreeSet<u64>>,
) -> Result<PredExpr, QueryError> {
    match call {
        Call::Matches { field, arg } => {
            string_call(field, df, |lhs| regexp_like(lhs, lit(arg.clone()), None))
        }
        Call::Contains { field, arg } => like_call(field, df, &format!("%{}%", like_escape(arg))),
        Call::StartsWith { field, arg } => {
            string_call(field, df, |lhs| starts_with(lhs, lit(arg.clone())))
        }
        Call::EndsWith { field, arg } => like_call(field, df, &format!("%{}", like_escape(arg))),
        // RFC0002.9 — `resolves_to(n)` matches the whole RFC 0001 §6.7 alias
        // equivalence class of `n` (resolved per-tenant at compile time, see
        // `collect_alias_classes`). It compiles to `template_id IN (class)`. A
        // singleton class (no alias on `n`) is `template_id IN (n)`, i.e.
        // behaviourally identical to a bare `template_id == n`.
        Call::ResolvesTo(n) => Ok(PredExpr::Filter(resolves_to_expr(*n, alias_classes))),
    }
}

/// Compile `resolves_to(n)` to a `template_id IN (class)` filter over the
/// pre-resolved alias class. `alias_classes` carries an entry for every `n` in
/// the predicate (populated by `collect_alias_classes`); a missing entry
/// degrades defensively to the singleton `{n}` so it can never compile to an
/// empty `IN ()` (which would match nothing).
fn resolves_to_expr(n: u64, alias_classes: &BTreeMap<u64, BTreeSet<u64>>) -> Expr {
    let list: Vec<Expr> = match alias_classes.get(&n) {
        Some(class) => class.iter().map(|id| lit(*id)).collect(),
        None => vec![lit(n)],
    };
    col(columns::TEMPLATE_ID).in_list(list, false)
}

/// A string function over a field's column, guarded for an absent OPTIONAL
/// column. Attribute fields aren't column-backed, so a string call on one is
/// rejected in this slice.
fn string_call(
    field: &Field,
    df: &DataFrame,
    build: impl FnOnce(Expr) -> Expr,
) -> Result<PredExpr, QueryError> {
    let column = string_call_column(field)?;
    if column_of(field).1 && !has_column(df, column) {
        return Ok(PredExpr::None);
    }
    Ok(PredExpr::Filter(build(col(column))))
}

fn like_call(field: &Field, df: &DataFrame, pattern: &str) -> Result<PredExpr, QueryError> {
    string_call(field, df, |lhs| lhs.like(lit(pattern.to_string())))
}

fn string_call_column(field: &Field) -> Result<&'static str, QueryError> {
    match field {
        Field::Body => Ok(columns::BODY),
        Field::Scope => Ok(columns::SCOPE_NAME),
        Field::EventName => Ok(columns::EVENT_NAME),
        // `trace_id`/`span_id` are binary id columns. The DSL accepts them as
        // string operands at parse time (a hex-string equality is meaningful),
        // but the DataFusion string functions are not defined over binary, so
        // a string call on one is rejected here until a bytes→hex projection
        // exists.
        Field::TraceId | Field::SpanId => Err(QueryError::InvalidQuery {
            detail: format!(
                "string functions are not defined on the binary id field {}",
                field_name(field)
            ),
        }),
        // Attribute-backed string fields are JSON-encoded; a string call on
        // them is deferred until attributes are individually columned.
        Field::Service | Field::Resource(_) | Field::Attr(_) => Err(QueryError::InvalidQuery {
            detail: "string functions on attribute fields are not supported in this query surface"
                .to_string(),
        }),
        other => Err(QueryError::InvalidQuery {
            detail: format!("{} is not a string field", field_name(other)),
        }),
    }
}

/// Build an ordering comparison `Expr` from an [`OrdOp`].
fn ord_expr(lhs: Expr, op: OrdOp, rhs: Expr) -> Expr {
    match op {
        OrdOp::Eq => lhs.eq(rhs),
        OrdOp::Ne => lhs.not_eq(rhs),
        OrdOp::Lt => lhs.lt(rhs),
        OrdOp::Le => lhs.lt_eq(rhs),
        OrdOp::Gt => lhs.gt(rhs),
        OrdOp::Ge => lhs.gt_eq(rhs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_nanos_covers_all_units() {
        // Arrange / Act / Assert
        assert_eq!(duration_nanos("30s").unwrap(), 30 * NS_PER_SECOND);
        assert_eq!(duration_nanos("2m").unwrap(), 120 * NS_PER_SECOND);
        assert_eq!(duration_nanos("1h").unwrap(), 3_600 * NS_PER_SECOND);
        assert_eq!(duration_nanos("1d").unwrap(), 86_400 * NS_PER_SECOND);
        assert_eq!(duration_nanos("1w").unwrap(), 7 * 86_400 * NS_PER_SECOND);
    }

    #[test]
    fn bucket_width_beyond_i64_nanoseconds_is_rejected_at_validation() {
        // Arrange — a width that fits `u64` (`duration_nanos` succeeds) but
        // exceeds `i64::MAX` nanoseconds, the type `bucket_expr`'s execution
        // lowering casts into (§6.5 floor-division): 20,000 weeks ≈
        // 12.096e18 ns, past i64::MAX ≈ 9.223e18 ns.
        let width = "20000w";
        let i64_max_ns = u64::try_from(i64::MAX).expect("i64::MAX is non-negative");
        assert!(
            duration_nanos(width).unwrap() > i64_max_ns,
            "the fixture width must actually exceed i64::MAX ns",
        );

        // Act
        let err = validate_group_terms(
            &[GroupTerm::Bucket(width.to_string())],
            &Predicate::Bool(true),
        )
        .expect_err("an i64-overflowing bucket width must fail validation");

        // Assert — rejected here, at the same compile-time gate as every
        // other `by`-list rule, not later during `bucket_expr`'s own cast.
        let QueryError::InvalidQuery { detail } = err else {
            panic!("expected InvalidQuery, got {err:?}");
        };
        assert!(
            detail.contains("i64"),
            "the error names the i64 bound: {detail}",
        );
    }

    #[test]
    fn resolve_window_defaults_to_lookback_when_no_range() {
        // Arrange — no range stage.
        let now = 1_000 * NS_PER_SECOND;
        let w = 60 * NS_PER_SECOND;
        // Act
        let (start, end) = resolve_window(&[], now, w).unwrap();
        // Assert — `[now - W, now]`, never unbounded.
        assert_eq!(end, now);
        assert_eq!(start, now - w);
    }

    #[test]
    fn resolve_window_uses_explicit_range() {
        // Arrange — range(-1h, now).
        let now = 10_000 * NS_PER_SECOND;
        let stages = [Stage::Range(
            Time::Duration {
                neg: true,
                literal: "1h".into(),
            },
            Time::Now,
        )];
        // Act
        let (start, end) = resolve_window(&stages, now, 1).unwrap();
        // Assert
        assert_eq!(end, now);
        assert_eq!(start, now - 3_600 * NS_PER_SECOND);
    }

    #[test]
    fn hex_bytes_decodes_case_insensitively() {
        // Arrange / Act
        let b = hex_bytes(&Field::SpanId, "00Ff10aB00112233").unwrap();
        // Assert
        assert_eq!(b, vec![0x00, 0xff, 0x10, 0xab, 0x00, 0x11, 0x22, 0x33]);
        // Wrong length is rejected.
        assert!(hex_bytes(&Field::SpanId, "00ff").is_err());
        assert!(hex_bytes(&Field::TraceId, "00ff").is_err());
    }

    #[test]
    fn timestamp_nanos_parses_rfc3339() {
        // Arrange / Act
        let ns = timestamp_nanos("1970-01-01T00:00:01Z").unwrap();
        // Assert
        assert_eq!(ns, NS_PER_SECOND);
        assert!(timestamp_nanos("not-a-time").is_err());
    }

    #[test]
    fn pinned_template_id_follows_the_amendment_rule() {
        let pin = |q: &str| pinned_template_id(&crate::dsl::parse(q).unwrap().predicate);
        assert_eq!(pin("template_id == 4"), Some(4));
        assert_eq!(pin("template_id == 4 and service == \"api\""), Some(4));
        assert_eq!(pin("template_id == 4 and template_id == 4"), Some(4));
        // A disjunction / negation pins nothing; conflicting top-level ids
        // pin nothing; `resolves_to` is an alias *set*, not a pin.
        assert_eq!(pin("template_id == 4 or template_id == 7"), None);
        assert_eq!(pin("not template_id == 4"), None);
        assert_eq!(pin("template_id == 4 and template_id == 7"), None);
        assert_eq!(pin("resolves_to(4)"), None);
        assert_eq!(pin("service == \"api\""), None);
    }

    #[test]
    fn validate_enforces_group_term_rules() {
        let v = |q: &str| {
            validate(
                &crate::dsl::parse(q).unwrap(),
                1_000 * NS_PER_SECOND,
                NS_PER_SECOND,
            )
        };
        assert!(v("template_id == 4 | count by param(0), bucket(5m)").is_ok());
        // `bucket` alone has no pinning requirement of its own.
        assert!(v("true | count by bucket(5m)").is_ok());
        assert!(v("service == \"api\" | count by param(0)").is_err());
        assert!(v("template_id == 4 | count by bucket(0s)").is_err());
        assert!(v("template_id == 4 | count by bucket(5m), bucket(1h)").is_err());
        assert!(v("template_id == 4 | count by param(0), param(0)").is_err());
        assert!(v("true | count | count").is_err());
    }

    #[test]
    fn attr_fragment_matches_canonical_json() {
        // The needle the compiler builds for `service == "api"` must be a
        // substring of the canonical JSON the writer stores — this is the
        // contract that keeps the JSON-substring match correct. Built here
        // independently of `attr_match` (which needs a DataFrame) so the
        // fragment shape is pinned without the engine.
        let fragment = "{\"key\":\"service.name\",\"value\":{\"stringValue\":\"api\"}}";
        let stored = "[{\"key\":\"service.name\",\"value\":{\"stringValue\":\"api\"}}]";
        assert!(stored.contains(fragment));
    }

    #[test]
    fn validate_rejects_limit_alongside_count() {
        // A `count [by …] | limit n` pipeline would silently drop the
        // `limit` — `Terminal::Aggregate` never consults `plan.limit`
        // (group-limiting semantics are not implemented) — so `validate`
        // must reject the combination rather than execute the wrong query.
        let v = |q: &str| {
            validate(
                &crate::dsl::parse(q).unwrap(),
                1_000 * NS_PER_SECOND,
                NS_PER_SECOND,
            )
        };
        assert!(v("true | count by service | limit 10").is_err());
        assert!(v("true | count | limit 10").is_err());
        // `limit` alone, or `count` alone, are each still fine.
        assert!(v("true | limit 10").is_ok());
        assert!(v("true | count by service").is_ok());
    }

    /// Property tests (CLAUDE.md §6.2) for the §6.3 amendment's planner
    /// invariants: pin detection ([`pinned_template_id`]) and the `by`-list
    /// rules ([`validate_group_terms`], reached here through the real
    /// [`validate`] entry point). Each generated case is checked against an
    /// independently-computed reference decision — ground truth tracked
    /// alongside generation, not derived by calling the code under test —
    /// so the properties supplement (not replace) the hand-picked examples
    /// above.
    mod planner_invariants {
        use std::collections::BTreeSet;

        use proptest::prelude::*;

        use super::*;

        /// A top-level `and`-term together with the pin candidate it
        /// contributes, if any: a bare `template_id == n` comparison pins;
        /// wrapping it in `or`, `not`, or `resolves_to` does not (§6.3
        /// amendment).
        fn pin_term() -> impl Strategy<Value = (Predicate, Option<i64>)> {
            let id = 0i64..4;
            prop_oneof![
                id.clone().prop_map(|n| (
                    Predicate::Comparison {
                        field: Field::TemplateId,
                        op: CmpOp::Ord(OrdOp::Eq),
                        value: Value::Int(n),
                    },
                    Some(n),
                )),
                Just((
                    Predicate::Comparison {
                        field: Field::Service,
                        op: CmpOp::Ord(OrdOp::Eq),
                        value: Value::Str("svc".to_string()),
                    },
                    None,
                )),
                id.clone().prop_map(|n| (
                    Predicate::Or(vec![
                        Predicate::Comparison {
                            field: Field::TemplateId,
                            op: CmpOp::Ord(OrdOp::Eq),
                            value: Value::Int(n),
                        },
                        Predicate::Bool(true),
                    ]),
                    None,
                )),
                id.clone().prop_map(|n| (
                    Predicate::Not(Box::new(Predicate::Comparison {
                        field: Field::TemplateId,
                        op: CmpOp::Ord(OrdOp::Eq),
                        value: Value::Int(n),
                    })),
                    None,
                )),
                id.prop_map(|n| (
                    #[allow(clippy::cast_sign_loss)] // `id` is 0..4, always non-negative
                    Predicate::Call(Call::ResolvesTo(n as u64)),
                    None,
                )),
            ]
        }

        /// A `by`-list element: a bare field (never a pin/param concern), a
        /// `param(n)` from a small pool (biases toward duplicate `n`), or a
        /// `bucket(...)` from a pool that includes a zero-width lexeme (the
        /// only non-positive width the grammar can represent — a signed
        /// literal is not a valid `bucket(...)` argument).
        fn group_term() -> impl Strategy<Value = GroupTerm> {
            prop_oneof![
                Just(GroupTerm::Field(Field::Service)),
                Just(GroupTerm::Field(Field::Body)),
                (0u32..3).prop_map(GroupTerm::Param),
                prop_oneof![
                    Just("0s".to_string()),
                    Just("5m".to_string()),
                    Just("1h".to_string()),
                ]
                .prop_map(GroupTerm::Bucket),
            ]
        }

        proptest! {
            #[test]
            fn validate_matches_the_naive_oracle(
                terms in prop::collection::vec(pin_term(), 1..4),
                by in prop::collection::vec(group_term(), 0..5),
            ) {
                // Ground truth for the pin: all pin-candidate terms present
                // must name the same id (empty ⇒ no pin), independent of
                // `pinned_template_id`'s own traversal.
                let pins: Vec<i64> = terms.iter().filter_map(|(_, p)| *p).collect();
                let expected_pin = match pins.split_first() {
                    Some((first, rest)) if rest.iter().all(|n| n == first) => Some(*first),
                    _ => None,
                };
                let predicate = if terms.len() == 1 {
                    terms[0].0.clone()
                } else {
                    Predicate::And(terms.iter().map(|(p, _)| p.clone()).collect())
                };
                prop_assert_eq!(
                    pinned_template_id(&predicate),
                    expected_pin.map(|n| u64::try_from(n).expect("pin pool is non-negative")),
                    "pinned_template_id disagreed with the naive oracle on {:?}",
                    predicate,
                );

                // Ground truth for the `by`-list: at most one `param(n)` per
                // `n` and only under a pin, at most one `bucket(...)`, and
                // every present bucket width positive.
                let mut seen_params = BTreeSet::new();
                let mut seen_bucket = false;
                let mut expected_ok = true;
                for term in &by {
                    match term {
                        GroupTerm::Param(n) => {
                            if expected_pin.is_none() || !seen_params.insert(*n) {
                                expected_ok = false;
                            }
                        }
                        GroupTerm::Bucket(width) => {
                            if seen_bucket || duration_nanos(width).unwrap_or(0) == 0 {
                                expected_ok = false;
                            }
                            seen_bucket = true;
                        }
                        GroupTerm::Field(_) => {}
                    }
                }

                let query = Query {
                    predicate,
                    stages: vec![Stage::Count { by: by.clone() }],
                };
                let got_ok = validate(&query, 1_000 * NS_PER_SECOND, NS_PER_SECOND).is_ok();
                prop_assert_eq!(
                    got_ok,
                    expected_ok,
                    "validate() disagreed with the naive oracle: pin={:?} by={:?}",
                    expected_pin,
                    by,
                );
            }
        }
    }
}
