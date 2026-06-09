//! The structured JSON front-end (RFC 0002 §6.4): serde-deserialised types
//! for the machine surface that MCP agents + programmatic clients target,
//! converting to the *same* [`Query`] IR as the string parser (RFC0002.2).
//! No path syntax for agents to build or escape — fields are a bare name or
//! a `{resource|attr: key}` object.
//!
//! Surface ambiguity (flagged, not silently invented): a comparison `value`
//! arrives as a JSON primitive, so a JSON string maps to [`Value::Str`].
//! Durations / timestamps in a *comparison* therefore cannot be expressed
//! distinctly here — they only have a distinct lexical form in the string
//! DSL. They remain expressible where the grammar actually needs them: the
//! `range(...)` stage carries its bounds as lexical strings parsed into
//! [`Time`]. The RFC0002.2 equivalence is over the queries both surfaces can
//! express; a duration-valued *comparison* is a string-DSL-only construct.

use serde::Deserialize;

use super::DslError;
use super::ir::{
    AggFn, Call, CmpOp, DriftQuery, Field, OrdOp, Predicate, Query, SeverityValue, Stage,
    Statement, Time, Value,
};
use super::{parse_severity_name_pub, require_string_operand, validate_sort_key};

/// Parse a structured (JSON) statement — a log query (`{"predicate":…}`) or a
/// RFC 0010 `drift` query (`{"drift":{"from":…,"to":…}}`) — into a
/// [`Statement`]. The two are distinct top-level objects: a `{"drift":…}`
/// carries no `predicate`/`stages` siblings, and a `{"predicate":…}` carries
/// no `drift` key (each denies the other's keys), so the shape selects the
/// variant unambiguously (RFC 0010 §6.1).
///
/// # Errors
///
/// Returns [`DslError`] for malformed JSON or any structure that violates the
/// §6.4 surface, the §7 grammar it mirrors, or the RFC 0010 §6.1 drift object.
pub fn parse_structured_statement(json: &str) -> Result<Statement, DslError> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| DslError::new(format!("malformed structured query: {e}")))?;
    // The `drift` key selects the audit-stream variant; otherwise it is a log
    // query. Dispatching on the key (rather than an untagged enum) keeps the
    // log-query path's precise per-field deserialisation errors intact.
    if value.get("drift").is_some() {
        let envelope: RawDriftEnvelope = serde_json::from_value(value)
            .map_err(|e| DslError::new(format!("malformed drift query: {e}")))?;
        return Ok(Statement::Drift(DriftQuery {
            from: parse_time(&envelope.drift.from)?,
            to: parse_time(&envelope.drift.to)?,
        }));
    }
    let raw: RawQuery = serde_json::from_value(value)
        .map_err(|e| DslError::new(format!("malformed structured query: {e}")))?;
    Ok(Statement::Logs(raw.into_ir()?))
}

/// Parse a structured (JSON) query into the shared [`Query`] IR.
///
/// # Errors
///
/// Returns [`DslError`] for malformed JSON or any structure that violates the
/// §6.4 surface or the §7 grammar it mirrors. A RFC 0010 `drift` object is
/// rejected here — use [`parse_structured_statement`] to accept it.
pub fn parse_structured(json: &str) -> Result<Query, DslError> {
    match parse_structured_statement(json)? {
        Statement::Logs(query) => Ok(query),
        Statement::Drift(_) => Err(DslError::new(
            "`drift` is an audit-stream query, not a log query".to_string(),
        )),
    }
}

/// The RFC 0010 §6.1 drift envelope `{ "drift": { "from", "to" } }`. Denies
/// unknown keys so a `{"drift":…,"predicate":…}` mix (or a typo'd sibling) is
/// rejected rather than silently accepted.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDriftEnvelope {
    drift: RawDrift,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDrift {
    from: String,
    to: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQuery {
    predicate: RawNode,
    #[serde(default)]
    stages: Vec<RawStage>,
}

impl RawQuery {
    fn into_ir(self) -> Result<Query, DslError> {
        let predicate = self.predicate.into_ir()?;
        let stages = self
            .stages
            .into_iter()
            .map(RawStage::into_ir)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Query { predicate, stages })
    }
}

/// A structured field: a bare top-level name string or a `{resource|attr}`
/// object (§6.4).
#[derive(Deserialize)]
#[serde(untagged)]
enum RawField {
    Name(String),
    Object(RawFieldObject),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFieldObject {
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    attr: Option<String>,
}

impl RawField {
    fn into_ir(self) -> Result<Field, DslError> {
        match self {
            Self::Name(name) => name_to_field(&name),
            Self::Object(o) => match (o.resource, o.attr) {
                (Some(key), None) => Ok(Field::Resource(key)),
                (None, Some(key)) => Ok(Field::Attr(key)),
                (Some(_), Some(_)) => Err(DslError::new(
                    "field object has both \"resource\" and \"attr\"; use exactly one".to_string(),
                )),
                (None, None) => Err(DslError::new(
                    "field object must have a \"resource\" or \"attr\" key".to_string(),
                )),
            },
        }
    }
}

/// Resolve a bare structured field name (including `severity`).
fn name_to_field(name: &str) -> Result<Field, DslError> {
    Some(match name {
        "body" => Field::Body,
        "severity" => Field::Severity,
        "ts" => Field::Ts,
        "observed_ts" => Field::ObservedTs,
        "trace_id" => Field::TraceId,
        "span_id" => Field::SpanId,
        "scope" => Field::Scope,
        "flags" => Field::Flags,
        "service" => Field::Service,
        "template_id" => Field::TemplateId,
        "confidence" => Field::Confidence,
        "lossy" => Field::Lossy,
        _ => return Err(DslError::new(format!("unknown field name {name:?}"))),
    })
    .ok_or_else(|| DslError::new(format!("unknown field name {name:?}")))
}

/// A predicate node (§6.4 `<node>`). `untagged` so the present keys select the
/// variant — comparison, call, const, or boolean. `serde` forbids
/// `deny_unknown_fields` on an `untagged` enum (and on its variants), so each
/// variant carries a dedicated struct that denies unknown keys itself; a node
/// with a stray extra key matches no variant and is rejected rather than
/// silently coerced (RFC0002.2 / §6.4 surface contract).
#[derive(Deserialize)]
#[serde(untagged)]
enum RawNode {
    Comparison(RawComparison),
    Call(RawCall),
    Const(RawConst),
    And(RawAnd),
    Or(RawOr),
    Not(RawNot),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawComparison {
    field: RawField,
    op: String,
    value: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCall {
    call: String,
    args: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConst {
    #[serde(rename = "const")]
    konst: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAnd {
    and: Vec<RawNode>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOr {
    or: Vec<RawNode>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNot {
    not: Box<RawNode>,
}

impl RawNode {
    fn into_ir(self) -> Result<Predicate, DslError> {
        match self {
            Self::Const(RawConst { konst }) => Ok(Predicate::Bool(konst)),
            Self::And(RawAnd { and }) => Ok(Predicate::and(combinator_terms("and", and)?)),
            Self::Or(RawOr { or }) => Ok(Predicate::or(combinator_terms("or", or)?)),
            Self::Not(RawNot { not }) => Ok(Predicate::Not(Box::new(not.into_ir()?))),
            Self::Call(RawCall { call, args }) => Ok(Predicate::Call(call_into_ir(&call, args)?)),
            Self::Comparison(RawComparison { field, op, value }) => {
                comparison_into_ir(field, &op, value)
            }
        }
    }
}

/// Convert the children of an `and`/`or` combinator, rejecting an empty list
/// (the §7 grammar has no nullary combinator). Flattening of same-kind nesting
/// and the single-element collapse happen in [`Predicate::and`]/[`Predicate::or`]
/// so the structured IR matches the string IR (RFC0002.2).
fn combinator_terms(kind: &str, nodes: Vec<RawNode>) -> Result<Vec<Predicate>, DslError> {
    if nodes.is_empty() {
        return Err(DslError::new(format!(
            "an \"{kind}\" needs at least one term"
        )));
    }
    nodes.into_iter().map(RawNode::into_ir).collect()
}

/// Build a comparison predicate, routing `severity` to the severity form so
/// the two surfaces share one IR (RFC0002.2).
fn comparison_into_ir(
    field: RawField,
    op: &str,
    value: serde_json::Value,
) -> Result<Predicate, DslError> {
    let field = field.into_ir()?;
    if field == Field::Severity {
        let op = parse_ord_op(op)?;
        let sval = severity_value(&value)?;
        return Ok(Predicate::Severity { op, value: sval });
    }
    let op = parse_cmp_op(op)?;
    let value = json_to_value(value)?;
    Ok(Predicate::Comparison { field, op, value })
}

/// The severity RHS in structured form: a JSON string (name) or number.
fn severity_value(value: &serde_json::Value) -> Result<SeverityValue, DslError> {
    match value {
        serde_json::Value::String(s) => parse_severity_name_pub(s)
            .map(SeverityValue::Name)
            .ok_or_else(|| {
                DslError::new(format!(
                    "{s:?} is not a severity name (trace|debug|info|warn|error|fatal)"
                ))
            }),
        // The string grammar's severity number is an unsigned token (no `-`),
        // so reject negatives here to keep the two surfaces aligned (RFC0002.2).
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) if i >= 0 => Ok(SeverityValue::Number(i)),
            Some(_) => Err(DslError::new(format!(
                "severity number {n} must be non-negative"
            ))),
            None => Err(DslError::new(format!(
                "severity number {n} is not an integer"
            ))),
        },
        other => Err(DslError::new(format!(
            "severity must compare against a name or number, found {other}"
        ))),
    }
}

/// Map a JSON primitive to an IR [`Value`]. A JSON string is a string literal
/// (see the module-level note on duration/timestamp ambiguity).
fn json_to_value(value: serde_json::Value) -> Result<Value, DslError> {
    match value {
        serde_json::Value::String(s) => Ok(Value::Str(s)),
        serde_json::Value::Bool(b) => Ok(Value::Bool(b)),
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Int(i))
            } else if n.is_u64() || n.is_i64() {
                // A JSON integer outside i64 range — reject rather than
                // silently coerce to Float (precision loss; would also diverge
                // from the string DSL, which errors on out-of-range integers).
                Err(DslError::new(format!("integer {n} is out of range")))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::Float(f))
            } else {
                Err(DslError::new(format!("number {n} is out of range")))
            }
        }
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => Err(DslError::new(
            "a comparison value must be a JSON primitive (string, number, bool, or null)"
                .to_string(),
        )),
    }
}

/// Build a call from the structured `{call, args}` shape, validating the §7
/// typed signatures.
fn call_into_ir(call: &str, mut args: Vec<serde_json::Value>) -> Result<Call, DslError> {
    if call == "resolves_to" {
        if args.len() != 1 {
            return Err(DslError::new(format!(
                "resolves_to takes exactly 1 argument (a template id), got {}",
                args.len()
            )));
        }
        let n = args.remove(0);
        let id = n
            .as_u64()
            .ok_or_else(|| DslError::new(format!("resolves_to template id {n} is not a u64")))?;
        return Ok(Call::ResolvesTo(id));
    }
    if !matches!(call, "matches" | "contains" | "starts_with" | "ends_with") {
        return Err(DslError::new(format!("unknown function {call:?}")));
    }
    if args.len() != 2 {
        return Err(DslError::new(format!(
            "{call} takes exactly 2 arguments (a field and a string), got {}",
            args.len()
        )));
    }
    let arg_str = args.remove(1);
    let field_json = args.remove(0);
    let field: RawField = serde_json::from_value(field_json)
        .map_err(|e| DslError::new(format!("{call}: first argument is not a field: {e}")))?;
    let field = field.into_ir()?;
    require_string_operand(call, &field)?;
    let arg = match arg_str {
        serde_json::Value::String(s) => s,
        other => {
            return Err(DslError::new(format!(
                "{call}: second argument must be a string, found {other}"
            )));
        }
    };
    Ok(match call {
        "matches" => Call::Matches { field, arg },
        "contains" => Call::Contains { field, arg },
        "starts_with" => Call::StartsWith { field, arg },
        "ends_with" => Call::EndsWith { field, arg },
        _ => unreachable!("call set checked above"),
    })
}

fn parse_ord_op(op: &str) -> Result<OrdOp, DslError> {
    Some(match op {
        "==" => OrdOp::Eq,
        "!=" => OrdOp::Ne,
        "<" => OrdOp::Lt,
        "<=" => OrdOp::Le,
        ">" => OrdOp::Gt,
        ">=" => OrdOp::Ge,
        _ => return Err(severity_op_error(op)),
    })
    .ok_or_else(|| severity_op_error(op))
}

fn severity_op_error(op: &str) -> DslError {
    if op == "=~" || op == "!~" {
        DslError::new(
            "severity is numeric (SeverityNumber); regex operators '=~'/'!~' are not allowed \
             on it"
                .to_string(),
        )
    } else {
        DslError::new(format!("unknown severity operator {op:?}"))
    }
}

fn parse_cmp_op(op: &str) -> Result<CmpOp, DslError> {
    Some(match op {
        "==" => CmpOp::Ord(OrdOp::Eq),
        "!=" => CmpOp::Ord(OrdOp::Ne),
        "<" => CmpOp::Ord(OrdOp::Lt),
        "<=" => CmpOp::Ord(OrdOp::Le),
        ">" => CmpOp::Ord(OrdOp::Gt),
        ">=" => CmpOp::Ord(OrdOp::Ge),
        "=~" => CmpOp::Match,
        "!~" => CmpOp::NotMatch,
        _ => return Err(DslError::new(format!("unknown comparison operator {op:?}"))),
    })
    .ok_or_else(|| DslError::new(format!("unknown comparison operator {op:?}")))
}

// ---- stages ----

/// A structured stage (§6.4): a single-key tagged object covering the full §7
/// stage set. Deserialised as a raw JSON object and dispatched on its tag key
/// — untagged enums interact badly with the aggregates' `{<fn>: path}` shape,
/// so we dispatch by hand for a precise, leak-free error on a bad tag.
#[derive(Deserialize)]
#[serde(transparent)]
struct RawStage(serde_json::Map<String, serde_json::Value>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRange {
    from: String,
    to: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCount {
    #[serde(default)]
    by: Vec<RawField>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSort {
    key: String,
    #[serde(default)]
    desc: bool,
}

impl RawStage {
    fn into_ir(self) -> Result<Stage, DslError> {
        let obj = self.0;
        // The tag is the stage kind. Aggregates carry an extra `by`, so a
        // stage is identified by its non-`by` key rather than a strict single
        // key. Reject anything that isn't `{tag: …}` (optionally `+ by`).
        let agg_tag = obj
            .keys()
            .find(|k| k.as_str() != "by")
            .cloned()
            .ok_or_else(|| DslError::new("a stage object has no stage key".to_string()))?;
        let extra = obj.keys().filter(|k| k.as_str() != "by").count();
        if extra != 1 {
            return Err(DslError::new(format!(
                "a stage names exactly one kind (range/count/sum/min/max/avg/sort/\
                 limit/project/render); got {extra}"
            )));
        }
        let has_by = obj.contains_key("by");
        let mut obj = obj;
        let by_value = obj.remove("by");
        let body = obj.remove(&agg_tag).expect("tag key present");
        // `by` is only valid on count and the aggregates.
        let reject_stray_by = |kind: &str| -> Result<(), DslError> {
            if has_by {
                Err(DslError::new(format!("the {kind} stage takes no `by`")))
            } else {
                Ok(())
            }
        };
        match agg_tag.as_str() {
            "range" => {
                reject_stray_by("range")?;
                let r: RawRange = from_stage_body(body, "range")?;
                Ok(Stage::Range(parse_time(&r.from)?, parse_time(&r.to)?))
            }
            "count" => {
                // `count` carries its grouping nested (`{"count":{"by":…}}`),
                // unlike the aggregates' sibling `by`. A top-level sibling `by`
                // here would be silently dropped, so reject it explicitly.
                if has_by {
                    return Err(DslError::new(
                        "count groups via {\"count\":{\"by\":[…]}}, not a top-level \"by\""
                            .to_string(),
                    ));
                }
                let c: RawCount = from_stage_body(body, "count")?;
                Ok(Stage::Count {
                    by: fields_to_ir(c.by)?,
                })
            }
            "sort" => {
                reject_stray_by("sort")?;
                let s: RawSort = from_stage_body(body, "sort")?;
                validate_sort_key(&s.key)?;
                Ok(Stage::Sort {
                    key: s.key,
                    desc: s.desc,
                })
            }
            "limit" => {
                reject_stray_by("limit")?;
                let n: u64 = from_stage_body(body, "limit")?;
                Ok(Stage::Limit(n))
            }
            "project" => {
                reject_stray_by("project")?;
                let fields: Vec<RawField> = from_stage_body(body, "project")?;
                Ok(Stage::Project(fields_to_ir(fields)?))
            }
            "render" => {
                reject_stray_by("render")?;
                // render is argument-less: only an empty body (`{}` or null).
                if !(body.is_null() || body.as_object().is_some_and(serde_json::Map::is_empty)) {
                    return Err(DslError::new(format!(
                        "render takes no arguments; got body `{body}`"
                    )));
                }
                Ok(Stage::Render)
            }
            "sum" | "min" | "max" | "avg" => agg_into_ir(&agg_tag, body, by_value),
            other => Err(DslError::new(format!(
                "unknown stage {other:?}; expected range, count, sum, min, max, avg, \
                 sort, limit, project, or render"
            ))),
        }
    }
}

/// Build an aggregate stage `{"<fn>": <path>}` with an optional sibling
/// `"by": [field, …]`.
fn agg_into_ir(
    tag: &str,
    path_body: serde_json::Value,
    by_value: Option<serde_json::Value>,
) -> Result<Stage, DslError> {
    let func = match tag {
        "sum" => AggFn::Sum,
        "min" => AggFn::Min,
        "max" => AggFn::Max,
        "avg" => AggFn::Avg,
        _ => unreachable!("tag set checked by caller"),
    };
    let path: RawField = serde_json::from_value(path_body)
        .map_err(|e| DslError::new(format!("aggregate path is not a field: {e}")))?;
    let by = match by_value {
        None => Vec::new(),
        Some(v) => {
            let fields: Vec<RawField> = serde_json::from_value(v)
                .map_err(|e| DslError::new(format!("aggregate `by` is not a field list: {e}")))?;
            fields_to_ir(fields)?
        }
    };
    Ok(Stage::Agg {
        func,
        path: path.into_ir()?,
        by,
    })
}

fn fields_to_ir(fields: Vec<RawField>) -> Result<Vec<Field>, DslError> {
    fields.into_iter().map(RawField::into_ir).collect()
}

/// Deserialize a stage body into `T`, mapping any serde error to a leak-free
/// [`DslError`] naming the stage kind.
fn from_stage_body<T: serde::de::DeserializeOwned>(
    body: serde_json::Value,
    what: &str,
) -> Result<T, DslError> {
    serde_json::from_value(body).map_err(|e| DslError::new(format!("malformed {what} stage: {e}")))
}

/// Parse a `range(...)` bound from its lexical string into a [`Time`], reusing
/// the string-DSL time grammar so the two surfaces agree (RFC0002.2).
fn parse_time(s: &str) -> Result<Time, DslError> {
    super::parse_time_pub(s)
}

#[cfg(test)]
mod tests {
    use super::parse_structured;
    use crate::dsl::ir::{Call, CmpOp, Field, OrdOp, Predicate, Stage, Value};

    #[test]
    fn parses_comparison_with_attr_object() {
        // Act
        let q = parse_structured(
            r#"{"predicate":{"field":{"attr":"http.status_code"},"op":"==","value":500}}"#,
        )
        .unwrap();
        // Assert
        assert_eq!(
            q.predicate,
            Predicate::Comparison {
                field: Field::Attr("http.status_code".into()),
                op: CmpOp::Ord(OrdOp::Eq),
                value: Value::Int(500),
            }
        );
    }

    #[test]
    fn routes_severity_to_the_severity_predicate() {
        // Act
        let q = parse_structured(r#"{"predicate":{"field":"severity","op":">=","value":"error"}}"#)
            .unwrap();
        // Assert
        assert!(matches!(q.predicate, Predicate::Severity { .. }));
    }

    #[test]
    fn parses_const_and_boolean_nodes() {
        let q = parse_structured(
            r#"{"predicate":{"and":[{"const":true},{"not":{"field":"lossy","op":"==","value":true}}]}}"#,
        )
        .unwrap();
        match q.predicate {
            Predicate::And(terms) => {
                assert_eq!(terms[0], Predicate::Bool(true));
                assert!(matches!(terms[1], Predicate::Not(_)));
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn parses_calls_with_typed_args() {
        let q = parse_structured(r#"{"predicate":{"call":"contains","args":["body","timeout"]}}"#)
            .unwrap();
        assert_eq!(
            q.predicate,
            Predicate::Call(Call::Contains {
                field: Field::Body,
                arg: "timeout".into(),
            })
        );
        let r = parse_structured(r#"{"predicate":{"call":"resolves_to","args":[7]}}"#).unwrap();
        assert_eq!(r.predicate, Predicate::Call(Call::ResolvesTo(7)));
    }

    #[test]
    fn rejects_bad_call_arity_and_unknown_fn() {
        assert!(parse_structured(r#"{"predicate":{"call":"contains","args":["body"]}}"#).is_err());
        assert!(parse_structured(r#"{"predicate":{"call":"nope","args":["body","x"]}}"#).is_err());
        assert!(parse_structured(r#"{"predicate":{"call":"resolves_to","args":["x"]}}"#).is_err());
    }

    #[test]
    fn parses_the_full_stage_set() {
        let q = parse_structured(
            r#"{
                "predicate":{"const":true},
                "stages":[
                    {"range":{"from":"-1h","to":"now"}},
                    {"count":{"by":["template_id","service"]}},
                    {"avg":"confidence","by":["service"]},
                    {"sort":{"key":"count","desc":true}},
                    {"project":["body","ts"]},
                    {"limit":10},
                    {"render":{}}
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(q.stages.len(), 7);
        assert!(matches!(q.stages[0], Stage::Range(_, _)));
        assert!(matches!(&q.stages[2], Stage::Agg { by, .. } if by.len() == 1));
        assert!(matches!(q.stages[6], Stage::Render));
    }

    #[test]
    fn rejects_regex_op_on_severity() {
        let err =
            parse_structured(r#"{"predicate":{"field":"severity","op":"=~","value":"error"}}"#)
                .unwrap_err();
        assert!(err.message().contains("regex"), "{}", err.message());
    }

    #[test]
    fn defaults_stages_to_empty() {
        let q = parse_structured(r#"{"predicate":{"const":true}}"#).unwrap();
        assert!(q.stages.is_empty());
    }

    #[test]
    fn rejects_unknown_field_and_malformed_json() {
        assert!(parse_structured(r#"{"predicate":{"field":"nope","op":"==","value":1}}"#).is_err());
        assert!(parse_structured("not json").is_err());
    }

    #[test]
    fn rejects_oversized_integer_instead_of_float_coercion() {
        // 18446744073709551615 > i64::MAX must be rejected, not coerced to Float.
        let err = parse_structured(
            r#"{"predicate":{"field":"template_id","op":"==","value":18446744073709551615}}"#,
        )
        .unwrap_err();
        assert!(err.message().contains("out of range"), "{}", err.message());
    }

    #[test]
    fn rejects_negative_severity_number() {
        let err = parse_structured(r#"{"predicate":{"field":"severity","op":">=","value":-1}}"#)
            .unwrap_err();
        assert!(err.message().contains("non-negative"), "{}", err.message());
    }

    #[test]
    fn rejects_non_string_operand_in_call() {
        assert!(
            parse_structured(r#"{"predicate":{"call":"contains","args":["severity","x"]}}"#)
                .is_err()
        );
        assert!(
            parse_structured(r#"{"predicate":{"call":"contains","args":["body","x"]}}"#).is_ok()
        );
    }

    #[test]
    fn rejects_empty_combinator_and_flattens_same_kind() {
        // Arrange / Act — empty list rejected; a nested same-kind `and`
        // flattens to one node (matching the string IR, RFC0002.2).
        assert!(parse_structured(r#"{"predicate":{"and":[]}}"#).is_err());
        let q = parse_structured(
            r#"{"predicate":{"and":[
                {"field":"body","op":"==","value":1},
                {"and":[
                    {"field":"service","op":"==","value":"x"},
                    {"field":"template_id","op":"==","value":2}
                ]}
            ]}}"#,
        )
        .unwrap();
        // Assert
        match q.predicate {
            Predicate::And(terms) => assert_eq!(terms.len(), 3),
            other => panic!("expected flat And, got {other:?}"),
        }
    }

    #[test]
    fn rejects_stray_top_level_by_on_count() {
        // `count` groups nested; a sibling `by` would be silently dropped.
        assert!(
            parse_structured(
                r#"{"predicate":{"const":true},"stages":[{"count":{},"by":["service"]}]}"#
            )
            .is_err()
        );
        assert!(
            parse_structured(
                r#"{"predicate":{"const":true},"stages":[{"count":{"by":["service"]}}]}"#
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_unknown_top_level_key() {
        // Arrange / Act — a stray sibling of `predicate`/`stages`.
        let err = parse_structured(r#"{"predicate":{"const":true},"bogus":1}"#).unwrap_err();
        // Assert — the malformed input errors instead of being accepted.
        assert!(err.message().contains("bogus"), "{}", err.message());
    }

    #[test]
    fn rejects_unknown_field_object_key() {
        // Arrange / Act — `{resource|attr}` plus a typo'd extra key.
        let err = parse_structured(
            r#"{"predicate":{"field":{"attr":"k","typo":"x"},"op":"==","value":1}}"#,
        )
        .unwrap_err();
        // Assert
        assert!(!err.message().is_empty());
    }

    #[test]
    fn rejects_unknown_node_key() {
        // Arrange / Act — a comparison node with an accidental extra key must
        // not deserialize as a partial Comparison (untagged variant denial).
        let err =
            parse_structured(r#"{"predicate":{"field":"body","op":"==","value":1,"extra":true}}"#)
                .unwrap_err();
        // Assert
        assert!(!err.message().is_empty());
    }

    #[test]
    fn rejects_unknown_stage_body_key() {
        // Arrange / Act — a `range` body with an unexpected key.
        let err = parse_structured(
            r#"{"predicate":{"const":true},"stages":[{"range":{"from":"-1h","to":"now","step":"5m"}}]}"#,
        )
        .unwrap_err();
        // Assert
        assert!(err.message().contains("range"), "{}", err.message());
    }

    #[test]
    fn render_rejects_a_non_empty_body() {
        // Arrange / Act — render is argument-less; a non-empty body is invalid.
        let err = parse_structured(r#"{"predicate":{"const":true},"stages":[{"render":1}]}"#)
            .unwrap_err();
        // Assert
        assert!(err.message().contains("render"), "{}", err.message());
    }

    #[test]
    fn validates_sort_key_against_the_grammar() {
        assert!(
            parse_structured(r#"{"predicate":{"const":true},"stages":[{"sort":{"key":"count"}}]}"#)
                .is_ok()
        );
        // A dotted attr path is not a §7 sort_key — the string surface could
        // not re-parse it from the unquoted serialised form.
        assert!(
            parse_structured(
                r#"{"predicate":{"const":true},"stages":[{"sort":{"key":"attr.http.status_code"}}]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn parses_structured_drift_object() {
        // Arrange / Act — the RFC 0010 §6.1 `{ "drift": { "from", "to" } }`.
        use super::parse_structured_statement;
        use crate::dsl::ir::{Statement, Time};
        let s = parse_structured_statement(r#"{"drift":{"from":"-7d","to":"now"}}"#).unwrap();
        // Assert
        match s {
            Statement::Drift(d) => {
                assert_eq!(
                    d.from,
                    Time::Duration {
                        neg: true,
                        literal: "7d".into()
                    }
                );
                assert_eq!(d.to, Time::Now);
            }
            Statement::Logs(_) => panic!("expected Drift, got Logs"),
        }
    }

    #[test]
    fn rejects_drift_object_mixed_with_predicate_or_stages() {
        use super::parse_structured_statement;
        // A drift object admits no predicate/stages sibling (RFC 0010 §6.1).
        assert!(
            parse_structured_statement(
                r#"{"drift":{"from":"-7d","to":"now"},"predicate":{"const":true}}"#
            )
            .is_err()
        );
        assert!(parse_structured_statement(r#"{"drift":{"from":"-7d"}}"#).is_err());
        assert!(
            parse_structured_statement(r#"{"drift":{"from":"-7d","to":"now","step":"5m"}}"#)
                .is_err()
        );
    }

    #[test]
    fn parse_structured_rejects_drift_as_a_log_query() {
        // The log-only `parse_structured` rejects the drift object.
        assert!(parse_structured(r#"{"drift":{"from":"-7d","to":"now"}}"#).is_err());
    }
}
