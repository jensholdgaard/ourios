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
//! `service`, `resource.<k>`, and `attr.<k>` have **no dedicated column** in
//! the RFC 0005 schema: resource/log attributes are stored as a single
//! OTLP-canonical-JSON `Utf8` column (`resource_attributes` / `attributes`).
//! They compile to a substring/`LIKE` match against that JSON column using a
//! needle built from the canonical `{"key":…,"value":{"stringValue":…}}`
//! shape — honest about the storage, not a column that doesn't exist. This is
//! a `Filter` with no row-group-pruning claim (RFC 0002 §5 RFC0002.6), and is
//! limited to string equality / string calls; ordering comparisons on a
//! JSON-encoded attribute are out of scope for this slice and rejected.
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

use datafusion::dataframe::DataFrame;
use datafusion::functions::expr_fn::{regexp_like, starts_with};
use datafusion::logical_expr::{Expr, not};
use datafusion::prelude::{col, lit};

use ourios_core::alias::AliasMap;
use ourios_core::tenant::TenantId;

use crate::dsl::ir::{
    Call, CmpOp, Field, OrdOp, Predicate, Query, SeverityValue, Stage, Time, Value,
};
use crate::{QueryError, has_column, time_bound_scalar};
use ourios_parquet::columns;

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
    limit: Option<usize>,
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
) -> Result<((u64, u64), Option<usize>), QueryError> {
    // This slice executes only the `range` (time window) and `limit` stages.
    // The aggregation / sort / projection / render stages parse into a valid
    // IR but are not yet wired to execution; reject them explicitly so a
    // query asking for one fails fast rather than silently returning a plain
    // filtered row set (RFC0002 — full pipeline execution is a later slice).
    for stage in &query.stages {
        let unsupported = match stage {
            Stage::Range(..) | Stage::Limit(_) => None,
            Stage::Count { .. } => Some("count"),
            Stage::Agg { .. } => Some("aggregation"),
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
    Ok((window, limit))
}

pub(crate) fn compile(
    query: &Query,
    tenant: &TenantId,
    now_unix_nano: u64,
    default_window_nanos: u64,
    alias_map: &AliasMap,
) -> Result<Plan, QueryError> {
    let (window, limit) = validate(query, now_unix_nano, default_window_nanos)?;
    // Eagerly resolve every `resolves_to(n)` against the tenant's alias map
    // so the deferred predicate compilation in `apply` is tenant-agnostic.
    let mut alias_classes = BTreeMap::new();
    collect_alias_classes(&query.predicate, tenant, alias_map, &mut alias_classes);

    Ok(Plan {
        window,
        predicate: query.predicate.clone(),
        alias_classes,
        limit,
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

/// Apply a compiled [`Plan`] to the base `DataFrame`: the time-window filter,
/// the compiled predicate (using the now-known union schema for the
/// absent-column guard), and the `limit`. Returns `Ok(None)` when the whole
/// query is provably empty.
pub(crate) fn apply(df: DataFrame, plan: Plan) -> Result<Option<DataFrame>, QueryError> {
    let Plan {
        window: (start, end),
        predicate,
        alias_classes,
        limit,
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

    if let Some(n) = limit {
        df = df.limit(0, Some(n)).map_err(crate::storage_err)?;
    }
    Ok(Some(df))
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
/// through [`attr_match`] before this is consulted, so the only text columns
/// reaching the column path are `body` and `scope`.
fn is_text_field(field: &Field) -> bool {
    matches!(
        field,
        Field::Body | Field::Scope | Field::Service | Field::Resource(_) | Field::Attr(_)
    )
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
        Field::Flags => "flags".into(),
        Field::Service => "service".into(),
        Field::TemplateId => "template_id".into(),
        Field::Confidence => "confidence".into(),
        Field::Lossy => "lossy".into(),
        Field::Resource(k) => format!("resource.{k}"),
        Field::Attr(k) => format!("attr.{k}"),
    }
}

/// Compile an attribute equality (`service`/`resource.k`/`attr.k`) to a
/// substring `LIKE` over the OTLP-canonical-JSON column. Only `==`/`!=` on a
/// string value is supported in this slice; the canonical encoding stores
/// string values as `{"key":"<k>","value":{"stringValue":"<v>"}}`, so an
/// exact key+string-value pair is matched by that JSON fragment as a `LIKE`
/// substring. Ordering / regex / non-string comparisons over a JSON-encoded
/// attribute are rejected (out of scope until attributes are columned).
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
    let ord = match op {
        CmpOp::Ord(OrdOp::Eq) => true,
        CmpOp::Ord(OrdOp::Ne) => false,
        _ => {
            return Err(QueryError::InvalidQuery {
                detail: "attributes support only == / != in this query surface".to_string(),
            });
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
    if ord {
        // `==` matches when the key is present with this exact string value.
        return Ok(PredExpr::Filter(value_match));
    }
    // `!=` must require the key PRESENT with a *different* value: a row
    // missing the key does not match. The presence guard matches the key with
    // any string value, then we exclude the exact value above. Without the
    // guard, `NOT LIKE` is also true for absent keys, which diverges from the
    // missing-field "no match" semantics used everywhere else.
    let key_present = format!("{{\"key\":{needle_key},\"value\":{{\"stringValue\":");
    let presence = col(column).like(lit(format!("%{}%", like_escape(&key_present))));
    Ok(PredExpr::Filter(presence.and(not(value_match))))
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
}
