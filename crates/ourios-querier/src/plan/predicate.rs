//! Predicate lowering (RFC 0002 §6.2, §6.4): the `PredExpr`
//! tri-state, comparisons, severity bands, attribute matching against
//! promoted columns or canonical JSON, body equality (RFC 0044), and
//! the call terms. Split from the flat compile module (epic #745
//! wave 3); every function here takes `&DFSchema` — no execution
//! object crosses in.

#[allow(clippy::wildcard_imports)] // parent glue after the file split
use super::*;

/// The result of compiling a predicate against a known schema.
pub(super) enum PredExpr {
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
    pub(super) fn into_expr(self) -> Expr {
        match self {
            PredExpr::All => lit(true),
            PredExpr::None => lit(false),
            PredExpr::Filter(e) => e,
        }
    }
}

pub(super) fn compile_predicate(
    p: &Predicate,
    schema: &DFSchema,
    alias_classes: &BTreeMap<u64, BTreeSet<u64>>,
    body_eqs: &BTreeMap<String, BodyEqualityPlan>,
) -> Result<PredExpr, QueryError> {
    match p {
        Predicate::Bool(true) => Ok(PredExpr::All),
        Predicate::Bool(false) => Ok(PredExpr::None),
        Predicate::Not(inner) => match compile_predicate(inner, schema, alias_classes, body_eqs)? {
            PredExpr::All => Ok(PredExpr::None),
            PredExpr::None => Ok(PredExpr::All),
            PredExpr::Filter(e) => Ok(PredExpr::Filter(not(e))),
        },
        Predicate::And(terms) => combine(terms, schema, alias_classes, body_eqs, true),
        Predicate::Or(terms) => combine(terms, schema, alias_classes, body_eqs, false),
        Predicate::Comparison { field, op, value } => {
            compile_comparison(field, *op, value, schema, body_eqs)
        }
        Predicate::Severity { op, value } => Ok(compile_severity(*op, value)),
        Predicate::Call(call) => compile_call(call, schema, alias_classes),
    }
}

pub(super) fn combine(
    terms: &[Predicate],
    schema: &DFSchema,
    alias_classes: &BTreeMap<u64, BTreeSet<u64>>,
    body_eqs: &BTreeMap<String, BodyEqualityPlan>,
    is_and: bool,
) -> Result<PredExpr, QueryError> {
    let mut acc: Option<Expr> = None;
    for term in terms {
        match (
            compile_predicate(term, schema, alias_classes, body_eqs)?,
            is_and,
        ) {
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

/// The `OTel` logs data model's "no severity here" — the value its proto enum
/// spells `SEVERITY_NUMBER_UNSPECIFIED`. Real sources emit it (Claude Code's
/// `GenAI` events, ETW's `LOG_ALWAYS`, Google Cloud's `DEFAULT`).
const SEVERITY_UNSPECIFIED: i64 = 0;

pub(super) fn compile_severity(op: OrdOp, value: &SeverityValue) -> PredExpr {
    // `severity_number` is REQUIRED (always present), so no absent-column
    // guard is needed. Compare as i64; DataFusion coerces against UInt8.
    let sev = || col(columns::SEVERITY_NUMBER);
    // A bare name denotes a four-wide OTel band (`error` → 17..=20), so
    // membership (`==`/`!=`) is a range test, not a single-value compare. A
    // numeric RHS is exact, and ordering ops use the band floor either way
    // (RFC0002.5: ordering compares against the floor of the named band).
    let threshold = match value {
        SeverityValue::Name(name) => name.floor(),
        SeverityValue::Number(n) => *n,
    };
    let expr = match (value, op) {
        (SeverityValue::Name(name), OrdOp::Eq) => sev()
            .gt_eq(lit(name.floor()))
            .and(sev().lt_eq(lit(name.ceil()))),
        (SeverityValue::Name(name), OrdOp::Ne) => {
            sev().lt(lit(name.floor())).or(sev().gt(lit(name.ceil())))
        }
        // RFC0002.21 — a minimum-severity floor (`>=`/`>`) does not filter out
        // records whose severity is *unspecified*. This mirrors the OTel Logs
        // SDK, whose `minimum_severity` drops a record only when its
        // SeverityNumber "is specified (i.e. not 0)"; unspecified records
        // "bypass minimum severity filtering". The data model sanctions the
        // special case explicitly ("Special handling MAY be given to
        // SeverityNumber=0 ... in less-than / greater-than comparisons").
        //
        // Written as a disjunction rather than a post-filter on purpose: the
        // `= 0` arm keeps row-group pruning correct for free, because a group
        // whose severity range includes 0 can no longer be proven non-matching.
        // A post-filter would leave the old pruning in place and silently skip
        // whole files of unspecified rows.
        (_, OrdOp::Ge | OrdOp::Gt) if threshold > SEVERITY_UNSPECIFIED => sev()
            .eq(lit(SEVERITY_UNSPECIFIED))
            .or(ord_expr(sev(), op, lit(threshold))),
        // The ceiling side must *exclude* unspecified, or the bypass above
        // would make a row match both `>= X` and `< X` — `0 < 17` is true
        // numerically, but a record with no severity is not "below error".
        // Excluding it here keeps a predicate and its negation a partition,
        // which is the property a query language cannot give up.
        (_, OrdOp::Lt | OrdOp::Le) if threshold > SEVERITY_UNSPECIFIED => sev()
            .not_eq(lit(SEVERITY_UNSPECIFIED))
            .and(ord_expr(sev(), op, lit(threshold))),
        _ => ord_expr(sev(), op, lit(threshold)),
    };
    PredExpr::Filter(expr)
}

pub(super) fn compile_comparison(
    field: &Field,
    op: CmpOp,
    value: &Value,
    schema: &DFSchema,
    body_eqs: &BTreeMap<String, BodyEqualityPlan>,
) -> Result<PredExpr, QueryError> {
    match (field, op, value) {
        // `body ==`/`!=` compiles to the RFC 0044 two-arm form; the plan
        // resolved the literal's template candidates eagerly.
        (Field::Body, CmpOp::Ord(OrdOp::Eq), Value::Str(literal)) => {
            Ok(body_equality(&body_eqs[literal], literal, schema, false))
        }
        (Field::Body, CmpOp::Ord(OrdOp::Ne), Value::Str(literal)) => {
            Ok(body_equality(&body_eqs[literal], literal, schema, true))
        }
        _ => match field {
            // Attribute-backed fields have no dedicated column (JSON storage).
            Field::Service => attr_match(
                columns::RESOURCE_ATTRIBUTES,
                "service.name",
                op,
                value,
                schema,
            ),
            Field::Resource(key) => {
                attr_match(columns::RESOURCE_ATTRIBUTES, key, op, value, schema)
            }
            Field::Attr(key) => attr_match(columns::ATTRIBUTES, key, op, value, schema),
            _ => column_comparison(field, op, value, schema),
        },
    }
}

/// The RFC 0044 §3.1 two-arm compile for `body == literal` (`negated` for
/// `!=`, §3.2's three-valued handling made explicit).
///
/// **Physical arm** — the stored body column, gated on
/// `body_kind = String`: retained (low-confidence) and lossy bodies, where
/// the stored bytes are the truth. The gate is also what excludes
/// structured bodies from both operators (§3.4 — their canonical JSON
/// bytes must never string-match).
///
/// **Template arm** — for each plan-time candidate: the version-qualified
/// `template_id` (prunable via the existing statistics), `lossy = false`
/// (a lossy reconstruction's truth is the retained body, physical arm),
/// and element-wise equality of the stored `params`/`separators` against
/// the values the literal implies. Params are single whitespace-free
/// tokens by construction, so element equality *is* byte-identical
/// reconstruction equality (`CLAUDE.md` §3.3).
pub(super) fn body_equality(
    plan: &BodyEqualityPlan,
    literal: &str,
    schema: &DFSchema,
    negated: bool,
) -> PredExpr {
    let string_kind = col(columns::BODY_KIND).eq(lit(0_u8));
    let physical_present = has_column(schema, columns::BODY);
    let body_lit = || lit(ScalarValue::Binary(Some(literal.as_bytes().to_vec())));

    let template_arm = plan
        .candidates
        .iter()
        .map(|c| candidate_arm(c, &plan.separators))
        .reduce(Expr::or);

    if negated {
        // `!=`: a stored body that differs, OR a faithful (non-lossy) mined
        // record matching no candidate — NULL physical bodies must not
        // silently drop mined records (§3.2, the mirror of #664).
        let physical_ne = physical_present.then(|| {
            string_kind
                .clone()
                .and(col(columns::BODY).is_not_null())
                .and(col(columns::BODY).not_eq(body_lit()))
        });
        let mined_base = string_kind
            .and(not(col(columns::LOSSY_FLAG)))
            .and(if physical_present {
                col(columns::BODY).is_null()
            } else {
                lit(true)
            });
        // `IS NOT TRUE` totalises the arm: a NULL (a corrupted param slot
        // under a matching template — reconstruct's own fallback treats the
        // row as body-retained) reads as "not this candidate", so the row
        // stays admitted rather than being three-valued-dropped.
        let mined_ne = match template_arm {
            Some(arm) => mined_base.and(is_not_true(arm)),
            None => mined_base,
        };
        return PredExpr::Filter(match physical_ne {
            Some(p) => p.or(mined_ne),
            None => mined_ne,
        });
    }

    let physical_eq =
        physical_present.then(|| string_kind.clone().and(col(columns::BODY).eq(body_lit())));
    // No `IS TRUE` here, deliberately: under a filter NULL and false are
    // equivalent (both drop the row), so equality is already total — and
    // wrapping the arm would defeat the row-group pruning the template
    // ids exist to enable (the `!=` branch, where NULL genuinely differs
    // from false, uses `IS NOT TRUE`). The arm is gated to NULL physical
    // bodies: when a body is retained the stored bytes are the truth (an
    // overflow-spilled param's truncated stored value could otherwise
    // false-match a crafted literal — `reconstruct` refuses exactly that
    // case), and for non-lossy retention §3.3 makes the physical arm
    // equivalent anyway.
    let template_eq = template_arm.map(|arm| {
        string_kind
            .and(not(col(columns::LOSSY_FLAG)))
            .and(if physical_present {
                col(columns::BODY).is_null()
            } else {
                lit(true)
            })
            .and(arm)
    });
    match (physical_eq, template_eq) {
        (Some(p), Some(t)) => PredExpr::Filter(p.or(t)),
        (Some(p), None) => PredExpr::Filter(p),
        (None, Some(t)) => PredExpr::Filter(t),
        // No body column in the union schema and no candidates: nothing
        // can match — the correct, cheap empty (§5 RFC0044.8).
        (None, None) => PredExpr::None,
    }
}

/// One candidate's conjunction: version-qualified template identity plus
/// the element-wise `params`/`separators` equalities the literal implies.
pub(super) fn candidate_arm(candidate: &BodyLiteralMatch, separators: &[Vec<u8>]) -> Expr {
    let mut arm = col(columns::TEMPLATE_ID)
        .eq(lit(candidate.template_id))
        .and(col(columns::TEMPLATE_VERSION).eq(lit(candidate.template_version)));
    for (i, value) in candidate.params.iter().enumerate() {
        let idx = i64::try_from(i).unwrap_or(i64::MAX).saturating_add(1);
        arm = arm.and(
            get_field(array_element(col(columns::PARAMS), lit(idx)), "value")
                .eq(lit(ScalarValue::Binary(Some(value.as_bytes().to_vec())))),
        );
    }
    for (k, sep) in separators.iter().enumerate() {
        let idx = i64::try_from(k).unwrap_or(i64::MAX).saturating_add(1);
        arm = arm.and(
            array_element(col(columns::SEPARATORS), lit(idx))
                .eq(lit(ScalarValue::Binary(Some(sep.clone())))),
        );
    }
    arm
}

/// A comparison over a field that maps to a dedicated RFC 0005 column.
pub(super) fn column_comparison(
    field: &Field,
    op: CmpOp,
    value: &Value,
    schema: &DFSchema,
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
    if optional && !has_column(schema, column) {
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
pub(super) fn field_literal(field: &Field, value: &Value) -> Result<Expr, QueryError> {
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

pub(super) fn string_literal(field: &Field, value: &Value) -> Result<Expr, QueryError> {
    match value {
        Value::Str(s) => Ok(lit(s.clone())),
        _ => Err(QueryError::InvalidQuery {
            detail: format!("{} expects a string literal", field_name(field)),
        }),
    }
}

/// Hex-decode a `trace_id` (16 bytes) / `span_id` (8 bytes) literal,
/// case-insensitive, to match the stored `FixedSizeBinary` column (§6.2).
pub(super) fn hex_bytes(field: &Field, s: &str) -> Result<Vec<u8>, QueryError> {
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
pub(super) fn column_of(field: &Field) -> (&'static str, bool) {
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
pub(super) fn is_text_field(field: &Field) -> bool {
    matches!(field, Field::Body | Field::Scope | Field::EventName)
}

pub(super) fn field_name(field: &Field) -> String {
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
pub(super) fn attr_match(
    column: &str,
    key: &str,
    op: CmpOp,
    value: &Value,
    schema: &DFSchema,
) -> Result<PredExpr, QueryError> {
    // Both attribute columns are REQUIRED, but guard for the union schema
    // anyway (a future writer could make them OPTIONAL).
    if !has_column(schema, column) {
        return Ok(PredExpr::None);
    }
    let promoted_name = promoted_column_name(column, key);
    // RFC 0042 §3.4: the union schema's type for the promoted column is
    // the declared class's type (`schema_adapt::merge_scanned_schemas`),
    // so a numeric column type routes to the numeric-class compilation.
    match column_type(schema, &promoted_name) {
        Some(DataType::Int64) => {
            return numeric_attr_match(column, key, &promoted_name, op, value, NumericClass::I64);
        }
        Some(DataType::Float64) => {
            return numeric_attr_match(column, key, &promoted_name, op, value, NumericClass::F64);
        }
        _ => {}
    }
    let Value::Str(v) = value else {
        return Err(QueryError::InvalidQuery {
            detail: "attribute comparisons take a string value in this query surface".to_string(),
        });
    };
    // `col()` parses dotted names as qualified references, so the promoted
    // column (literally named `resource.<k>` / `attr.<k>`) must be addressed
    // as an unqualified `Column` built directly.
    let promoted = has_column(schema, &promoted_name)
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

/// The numeric promotion class of a key, as read off the union schema
/// (RFC 0042 §3.4).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NumericClass {
    I64,
    F64,
}

impl NumericClass {
    fn name(self) -> &'static str {
        match self {
            Self::I64 => "i64",
            Self::F64 => "f64",
        }
    }
}

/// RFC 0042 §3.4 — predicate compilation for a numeric-class promoted key.
///
/// - The literal must be numeric: an int for `i64`; an int or float for
///   `f64` (ints widen, matching the write-side projection). A string
///   literal or a float against `i64` is a compile error naming the
///   declared class.
/// - Ordering is typed-arm-only, prunable via numeric min/max statistics.
/// - `==`/`!=` on `i64` carry the JSON fallback arm for files where the
///   column is absent or type-mismatched — canonical integer formatting
///   is unique (the stored form is `"intValue":"<decimal>"`, RFC 0005
///   §3.3 / proto3-JSON's string-encoded i64), so the fragment is exact.
/// - `==`/`!=` on `f64` are typed-arm-only: JSON text has no canonical
///   float formatting, so a fallback arm would be wrong both ways.
/// - Regex is a compile error — a category the string class serves.
pub(super) fn numeric_attr_match(
    column: &str,
    key: &str,
    promoted_name: &str,
    op: CmpOp,
    value: &Value,
    class: NumericClass,
) -> Result<PredExpr, QueryError> {
    let class_err = |what: &str| {
        Err(QueryError::InvalidQuery {
            detail: format!(
                "'{promoted_name}' is promoted as {}: {what} (RFC 0042 §3.4)",
                class.name()
            ),
        })
    };
    let p = Expr::Column(Column::new_unqualified(promoted_name.to_string()));
    // The typed literal. i64 == keeps the exact integer for the JSON arm.
    let (typed_lit, int_for_json) = match (class, value) {
        (NumericClass::I64, Value::Int(i)) => (lit(*i), Some(*i)),
        (NumericClass::F64, Value::Int(i)) => {
            #[allow(clippy::cast_precision_loss)] // §3.1: int widening is the contract
            (lit(*i as f64), None)
        }
        (NumericClass::F64, Value::Float(f)) => (lit(*f), None),
        (NumericClass::I64, Value::Float(_)) => {
            return class_err("compare it with an integer literal, not a float");
        }
        _ => {
            return class_err("compare it with a numeric literal, not a string");
        }
    };
    match op {
        CmpOp::Match | CmpOp::NotMatch => class_err("regex applies to string-class keys only"),
        CmpOp::Ord(OrdOp::Eq | OrdOp::Ne) => {
            let eq = matches!(op, CmpOp::Ord(OrdOp::Eq));
            let typed = if eq {
                p.clone().eq(typed_lit)
            } else {
                // Presence explicit, as in the string two-arm form.
                p.clone().is_not_null().and(p.clone().not_eq(typed_lit))
            };
            let Some(i) = int_for_json else {
                // f64 equality: typed arm only (§3.4). Pre-amendment /
                // mismatched files never match — documented consequence.
                return Ok(PredExpr::Filter(typed));
            };
            // The canonical stored fragment for an integer attribute:
            // {"key":"<k>","value":{"intValue":"<decimal>"}}.
            let needle_key = serde_json::to_string(key).map_err(|e| QueryError::InvalidQuery {
                detail: format!("attribute key is not encodable: {e}"),
            })?;
            let fragment = format!("{{\"key\":{needle_key},\"value\":{{\"intValue\":\"{i}\"}}}}");
            let value_match = col(column).like(lit(format!("%{}%", like_escape(&fragment))));
            let json = if eq {
                value_match
            } else {
                let key_present = format!("{{\"key\":{needle_key},\"value\":{{\"intValue\":\"");
                let presence = col(column).like(lit(format!("%{}%", like_escape(&key_present))));
                presence.and(not(value_match))
            };
            Ok(PredExpr::Filter(typed.or(p.is_null().and(json))))
        }
        CmpOp::Ord(ord) => Ok(PredExpr::Filter(ord_expr(p, ord, typed_lit))),
    }
}

/// The RFC 0022 promoted column name for an attribute key: the literal DSL
/// path (`resource.<k>` / `attr.<k>`, §3.1), derived from the same prefixes
/// the writer's [`ourios_parquet::promoted`] module declares.
pub(super) fn promoted_column_name(column: &str, key: &str) -> String {
    match column {
        columns::RESOURCE_ATTRIBUTES => format!("{}{key}", promoted::RESOURCE_PREFIX),
        _ => format!("{}{key}", promoted::ATTR_PREFIX),
    }
}

/// Escape the `%` / `_` / `\` wildcards in a `LIKE` pattern literal so the
/// JSON fragment matches as plain text.
pub(super) fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub(super) fn compile_call(
    call: &Call,
    schema: &DFSchema,
    alias_classes: &BTreeMap<u64, BTreeSet<u64>>,
) -> Result<PredExpr, QueryError> {
    match call {
        Call::Matches { field, arg } => string_call(field, schema, |lhs| {
            regexp_like(lhs, lit(arg.clone()), None)
        }),
        Call::Contains { field, arg } => {
            like_call(field, schema, &format!("%{}%", like_escape(arg)))
        }
        Call::StartsWith { field, arg } => {
            string_call(field, schema, |lhs| starts_with(lhs, lit(arg.clone())))
        }
        Call::EndsWith { field, arg } => {
            like_call(field, schema, &format!("%{}", like_escape(arg)))
        }
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
pub(super) fn resolves_to_expr(n: u64, alias_classes: &BTreeMap<u64, BTreeSet<u64>>) -> Expr {
    let list: Vec<Expr> = match alias_classes.get(&n) {
        Some(class) => class.iter().map(|id| lit(*id)).collect(),
        None => vec![lit(n)],
    };
    col(columns::TEMPLATE_ID).in_list(list, false)
}

/// A string function over a field's column, guarded for an absent OPTIONAL
/// column. Attribute fields aren't column-backed, so a string call on one is
/// rejected in this slice.
pub(super) fn string_call(
    field: &Field,
    schema: &DFSchema,
    build: impl FnOnce(Expr) -> Expr,
) -> Result<PredExpr, QueryError> {
    let column = string_call_column(field)?;
    if column_of(field).1 && !has_column(schema, column) {
        return Ok(PredExpr::None);
    }
    Ok(PredExpr::Filter(build(col(column))))
}

pub(super) fn like_call(
    field: &Field,
    schema: &DFSchema,
    pattern: &str,
) -> Result<PredExpr, QueryError> {
    string_call(field, schema, |lhs| lhs.like(lit(pattern.to_string())))
}

pub(super) fn string_call_column(field: &Field) -> Result<&'static str, QueryError> {
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
pub(super) fn ord_expr(lhs: Expr, op: OrdOp, rhs: Expr) -> Expr {
    match op {
        OrdOp::Eq => lhs.eq(rhs),
        OrdOp::Ne => lhs.not_eq(rhs),
        OrdOp::Lt => lhs.lt(rhs),
        OrdOp::Le => lhs.lt_eq(rhs),
        OrdOp::Gt => lhs.gt(rhs),
        OrdOp::Ge => lhs.gt_eq(rhs),
    }
}
