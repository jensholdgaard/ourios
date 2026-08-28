//! Grouping and scalar-aggregate lowering (RFC 0002 §6.3/§6.5): the
//! `by`-list, the promoted-column group keys, `param(n)` extraction,
//! `bucket(width)`, and the scalar aggregate expressions. Split from
//! the flat compile module (epic #745 wave 3); everything lowers
//! against the scanned union schema alone.

#[allow(clippy::wildcard_imports)] // parent glue after the file split
use super::*;

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
pub(crate) fn group_exprs(by: &[GroupTerm], schema: &DFSchema) -> Result<Vec<Expr>, QueryError> {
    by.iter()
        .enumerate()
        .map(|(i, term)| {
            let expr = match term {
                GroupTerm::Field(field) => field_group_expr(field, schema)?,
                GroupTerm::Param(n) => get_field(
                    array_element(col(columns::PARAMS), lit(i64::from(*n) + 1)),
                    "value",
                ),
                GroupTerm::Bucket(width) => bucket_expr(width, schema)?,
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
pub(super) fn group_by_promoted(
    column: &str,
    key: &str,
    schema: &DFSchema,
) -> Result<Expr, QueryError> {
    let name = promoted_column_name(column, key);
    if has_column(schema, &name) {
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

pub(super) fn field_group_expr(field: &Field, schema: &DFSchema) -> Result<Expr, QueryError> {
    match field {
        Field::Service => {
            let name =
                promoted_column_name(columns::RESOURCE_ATTRIBUTES, promoted::SERVICE_NAME_KEY);
            if has_column(schema, &name) {
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
        Field::Resource(key) => group_by_promoted(columns::RESOURCE_ATTRIBUTES, key, schema),
        Field::Attr(key) => group_by_promoted(columns::ATTRIBUTES, key, schema),
        _ => {
            let (column, optional) = column_of(field);
            if optional && !has_column(schema, column) {
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
    schema: &DFSchema,
) -> Result<Expr, QueryError> {
    let (column, name) = match path {
        Field::Attr(key) => (
            group_by_promoted(columns::ATTRIBUTES, key, schema)?,
            promoted_column_name(columns::ATTRIBUTES, key),
        ),
        Field::Resource(key) => (
            group_by_promoted(columns::RESOURCE_ATTRIBUTES, key, schema)?,
            promoted_column_name(columns::RESOURCE_ATTRIBUTES, key),
        ),
        _ => return Err(agg_path_error()),
    };
    // RFC 0042 §3.5 / RFC0042.3: a numeric-class column aggregates
    // directly — no parse-shaped `try_cast`. `Int64` takes a plain
    // numeric `cast` to the Float64 output type (exact for |v| ≤ 2^53
    // — RFC 0042 §3.1's stated bound; a key expected to exceed it
    // belongs in i64 and loses precision here if summed anyway — the
    // same rule as the write-side projection); a `Utf8` promoted column
    // keeps the RFC0002.17 `try_cast` (unparseable → NULL → excluded).
    let numeric = match column_type(schema, &name) {
        Some(DataType::Float64) => column,
        Some(DataType::Int64) => cast(column, DataType::Float64),
        _ => try_cast(column, DataType::Float64),
    };
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
pub(super) fn validate_agg_path(path: &Field) -> Result<(), QueryError> {
    match path {
        Field::Attr(_) | Field::Resource(_) => Ok(()),
        _ => Err(agg_path_error()),
    }
}

pub(super) fn agg_path_error() -> QueryError {
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
pub(super) fn null_scalar_for(field: &Field) -> ScalarValue {
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

pub(super) fn bucket_expr(width: &str, schema: &DFSchema) -> Result<Expr, QueryError> {
    let w = i64::try_from(duration_nanos(width)?).map_err(|_| QueryError::InvalidQuery {
        detail: format!("bucket({width}) width exceeds i64 nanoseconds"),
    })?;
    let ts = col(columns::TIME_UNIX_NANO);
    let effective = if has_column(schema, columns::EFFECTIVE_TIME_UNIX_NANO) {
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
