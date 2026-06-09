//! Canonical serialiser: [`Query`] → a single-line β string such that
//! `parse(serialize(q)) == q` (RFC0002.7 round-trip) and the result is a
//! YAML-safe scalar (RFC0002.10). Canonical choices: keyword operators
//! (`and`/`or`/`not`) over the terse aliases, `==`/`!=`, lowercase severity
//! names, dotted attribute keys when expressible (else bracketed), and the
//! §7 string escapes (`\"` `\\` `\n` `\t` `\r`, and `\uXXXX` for other
//! control characters).

use std::fmt::Write as _;

use super::ir::{
    AggFn, Call, CmpOp, DriftQuery, Field, OrdOp, Predicate, Query, SeverityName, SeverityValue,
    Stage, Statement, Time, Value,
};

/// Serialise a [`Statement`] (a log query or a RFC 0010 `drift` query) to its
/// canonical single-line β form. Round-trips through [`super::parse_statement`].
#[must_use]
pub fn serialize_statement(statement: &Statement) -> String {
    match statement {
        Statement::Logs(query) => serialize(query),
        Statement::Drift(drift) => serialize_drift(drift),
    }
}

/// Serialise a [`DriftQuery`] to `drift from <t1> to <t2>` (RFC 0010 §6.1).
fn serialize_drift(drift: &DriftQuery) -> String {
    let mut out = String::from("drift from ");
    write_time(&mut out, &drift.from);
    out.push_str(" to ");
    write_time(&mut out, &drift.to);
    out
}

/// Serialise a [`Query`] to its canonical single-line β form.
#[must_use]
pub fn serialize(query: &Query) -> String {
    let mut out = String::new();
    write_predicate(&mut out, &query.predicate, false);
    for stage in &query.stages {
        out.push_str(" | ");
        write_stage(&mut out, stage);
    }
    out
}

/// Write a predicate. `parenthesize` wraps `and`/`or` nodes so a parent of
/// higher binding (a `not`, or an `or` over an `and`) keeps the tree shape.
fn write_predicate(out: &mut String, pred: &Predicate, parenthesize: bool) {
    match pred {
        Predicate::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Predicate::Comparison { field, op, value } => {
            write_field(out, field);
            out.push(' ');
            out.push_str(cmp_op_str(*op));
            out.push(' ');
            write_value(out, value);
        }
        Predicate::Severity { op, value } => {
            out.push_str("severity ");
            out.push_str(ord_op_str(*op));
            out.push(' ');
            write_severity_value(out, value);
        }
        Predicate::Call(call) => write_call(out, call),
        Predicate::Not(inner) => {
            out.push_str("not ");
            // A `not` binds tighter than `and`/`or`, so wrap those.
            let wrap = matches!(**inner, Predicate::And(_) | Predicate::Or(_));
            write_predicate(out, inner, wrap);
        }
        Predicate::And(terms) => write_join(out, terms, " and ", parenthesize, BindLevel::And),
        Predicate::Or(terms) => write_join(out, terms, " or ", parenthesize, BindLevel::Or),
    }
}

#[derive(Clone, Copy)]
enum BindLevel {
    And,
    Or,
}

fn write_join(
    out: &mut String,
    terms: &[Predicate],
    sep: &str,
    parenthesize: bool,
    level: BindLevel,
) {
    if parenthesize {
        out.push('(');
    }
    for (i, term) in terms.iter().enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        // Inside an `and`, wrap a child `or` (lower binding). Canonical IR is
        // flattened, so a same-kind child never appears here; wrap it anyway
        // so a hand-built non-canonical tree still serialises to a string
        // that re-parses to the same shape.
        let wrap = match level {
            BindLevel::And => matches!(term, Predicate::And(_) | Predicate::Or(_)),
            BindLevel::Or => matches!(term, Predicate::Or(_)),
        };
        write_predicate(out, term, wrap);
    }
    if parenthesize {
        out.push(')');
    }
}

fn write_call(out: &mut String, call: &Call) {
    match call {
        Call::Matches { field, arg } => write_str_call(out, "matches", field, arg),
        Call::Contains { field, arg } => write_str_call(out, "contains", field, arg),
        Call::StartsWith { field, arg } => write_str_call(out, "starts_with", field, arg),
        Call::EndsWith { field, arg } => write_str_call(out, "ends_with", field, arg),
        Call::ResolvesTo(id) => {
            let _ = write!(out, "resolves_to({id})");
        }
    }
}

fn write_str_call(out: &mut String, name: &str, field: &Field, arg: &str) {
    out.push_str(name);
    out.push('(');
    write_field(out, field);
    out.push_str(", ");
    write_quoted(out, arg);
    out.push(')');
}

fn write_field(out: &mut String, field: &Field) {
    match field {
        Field::Body => out.push_str("body"),
        Field::Severity => out.push_str("severity"),
        Field::Ts => out.push_str("ts"),
        Field::ObservedTs => out.push_str("observed_ts"),
        Field::TraceId => out.push_str("trace_id"),
        Field::SpanId => out.push_str("span_id"),
        Field::Scope => out.push_str("scope"),
        Field::Flags => out.push_str("flags"),
        Field::Service => out.push_str("service"),
        Field::TemplateId => out.push_str("template_id"),
        Field::Confidence => out.push_str("confidence"),
        Field::Lossy => out.push_str("lossy"),
        Field::Resource(key) => write_attr_path(out, "resource", key),
        Field::Attr(key) => write_attr_path(out, "attr", key),
    }
}

/// Write `resource`/`attr` with the dotted form when every segment is a bare
/// identifier, else the bracketed form (§6.1 — the only forms that re-parse
/// to the same key).
fn write_attr_path(out: &mut String, prefix: &str, key: &str) {
    out.push_str(prefix);
    if is_dotted_key(key) {
        out.push('.');
        out.push_str(key);
    } else {
        out.push('[');
        write_quoted(out, key);
        out.push(']');
    }
}

/// True if `key` is a non-empty `ident ("." ident)*` of bare identifiers.
fn is_dotted_key(key: &str) -> bool {
    !key.is_empty()
        && key.split('.').all(|seg| {
            let mut chars = seg.chars();
            matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Str(s) => write_quoted(out, s),
        Value::Int(n) => {
            let _ = write!(out, "{n}");
        }
        Value::Float(f) => write_float(out, *f),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Null => out.push_str("null"),
        Value::Duration(d) => out.push_str(d),
        Value::Timestamp(t) => out.push_str(t),
    }
}

/// Render a float so it re-lexes as a §7 `float` (`digits "." digits`): always
/// a `.`, never exponent notation. Rust's `{}` for `f64` is already fixed-point
/// (no `e`), but we defend the contract explicitly so a future formatter change
/// can't silently emit an exponent the lexer would reject. A `-` goes through
/// the `number` literal path.
fn write_float(out: &mut String, f: f64) {
    let s = format!("{f}");
    debug_assert!(
        !s.contains(['e', 'E']),
        "float Display produced an exponent the lexer rejects: {s}"
    );
    if s.contains('.') {
        out.push_str(&s);
    } else {
        // Integer-valued float (e.g. 1.0 → "1"): force a fractional part so it
        // re-parses as Float, not Int.
        let _ = write!(out, "{s}.0");
    }
}

fn write_severity_value(out: &mut String, value: &SeverityValue) {
    match value {
        SeverityValue::Name(name) => out.push_str(severity_name_str(*name)),
        SeverityValue::Number(n) => {
            let _ = write!(out, "{n}");
        }
    }
}

fn write_stage(out: &mut String, stage: &Stage) {
    match stage {
        Stage::Range(from, to) => {
            out.push_str("range(");
            write_time(out, from);
            out.push_str(", ");
            write_time(out, to);
            out.push(')');
        }
        Stage::Count { by } => {
            out.push_str("count");
            write_by(out, by);
        }
        Stage::Agg { func, path, by } => {
            out.push_str(agg_fn_str(*func));
            out.push('(');
            write_field(out, path);
            out.push(')');
            write_by(out, by);
        }
        Stage::Sort { key, desc } => {
            out.push_str("sort ");
            out.push_str(key);
            out.push_str(if *desc { " desc" } else { " asc" });
        }
        Stage::Limit(n) => {
            let _ = write!(out, "limit {n}");
        }
        Stage::Project(fields) => {
            out.push_str("project ");
            write_field_list(out, fields);
        }
        Stage::Render => out.push_str("render"),
    }
}

fn write_by(out: &mut String, by: &[Field]) {
    if !by.is_empty() {
        out.push_str(" by ");
        write_field_list(out, by);
    }
}

fn write_field_list(out: &mut String, fields: &[Field]) {
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_field(out, f);
    }
}

fn write_time(out: &mut String, time: &Time) {
    match time {
        Time::Now => out.push_str("now"),
        Time::Duration { neg, literal } => {
            if *neg {
                out.push('-');
            }
            out.push_str(literal);
        }
        Time::Timestamp(t) => out.push_str(t),
    }
}

/// Write a string literal with §7 escaping. We escape only the §7 set so the
/// result re-lexes; everything else (including non-ASCII) is passed through.
fn write_quoted(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                // Any other control char → \uXXXX (the §7 catch-all escape).
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn ord_op_str(op: OrdOp) -> &'static str {
    match op {
        OrdOp::Eq => "==",
        OrdOp::Ne => "!=",
        OrdOp::Lt => "<",
        OrdOp::Le => "<=",
        OrdOp::Gt => ">",
        OrdOp::Ge => ">=",
    }
}

fn cmp_op_str(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Ord(o) => ord_op_str(o),
        CmpOp::Match => "=~",
        CmpOp::NotMatch => "!~",
    }
}

fn severity_name_str(name: SeverityName) -> &'static str {
    match name {
        SeverityName::Trace => "trace",
        SeverityName::Debug => "debug",
        SeverityName::Info => "info",
        SeverityName::Warn => "warn",
        SeverityName::Error => "error",
        SeverityName::Fatal => "fatal",
    }
}

fn agg_fn_str(func: AggFn) -> &'static str {
    match func {
        AggFn::Sum => "sum",
        AggFn::Min => "min",
        AggFn::Max => "max",
        AggFn::Avg => "avg",
    }
}

#[cfg(test)]
mod tests {
    use crate::dsl::{parse, serialize};

    /// A small, diverse corpus that exercises every production; each must
    /// round-trip through serialize→parse to the same IR.
    const CORPUS: &[&str] = &[
        "true",
        "false",
        "not lossy == true",
        "template_id == 42",
        "confidence < 0.7",
        "service == \"api\"",
        "body =~ \"^GET\"",
        "body !~ \"health\"",
        "severity >= error",
        "severity == 17",
        "attr.http.status_code == 500",
        "resource[\"k8s.pod.name\"] == \"p\"",
        "contains(body, \"timeout\")",
        "starts_with(service, \"api-\")",
        "resolves_to(7)",
        "service == \"api\" and severity >= error",
        "service == \"api\" or template_id == 1",
        "lossy == true or template_id == 1 and confidence < 0.5",
        "not (service == \"api\" or service == \"web\")",
        "true | range(-1h, now) | limit 100",
        "service == \"x\" | count by template_id, service | sort count desc | limit 10",
        "true | avg(confidence) by service | project body, ts | render",
        "ts >= 2026-01-02T03:04:05Z",
        "attr.dur == 30s",
    ];

    #[test]
    fn corpus_round_trips_through_canonical_form() {
        for query in CORPUS {
            // Arrange
            let parsed = parse(query).unwrap_or_else(|e| panic!("parse {query:?}: {e}"));
            // Act
            let serialized = serialize(&parsed);
            let reparsed =
                parse(&serialized).unwrap_or_else(|e| panic!("reparse {serialized:?}: {e}"));
            // Assert
            assert_eq!(parsed, reparsed, "round-trip mismatch for {query:?}");
        }
    }

    #[test]
    fn serialization_is_single_line() {
        for query in CORPUS {
            let s = serialize(&parse(query).unwrap());
            assert!(!s.contains('\n'), "serialised form is multi-line: {s:?}");
        }
    }

    #[test]
    fn canonicalizes_terse_aliases_to_keywords() {
        // Arrange — terse aliases and an integer-valued float.
        let q = parse("service == \"a\" && !lossy == true || template_id == 1").unwrap();
        // Act
        let s = serialize(&q);
        // Assert — canonical keywords, no terse aliases.
        assert!(s.contains(" and "));
        assert!(s.contains(" or "));
        assert!(s.contains("not "));
        assert!(!s.contains("&&") && !s.contains("||") && !s.contains('!'));
    }

    #[test]
    fn integer_valued_float_keeps_a_fractional_part() {
        // 1.0 must serialise as "1.0", not "1" (which would re-parse as Int).
        let q = parse("confidence == 1.0").unwrap();
        let s = serialize(&q);
        assert!(s.ends_with("1.0"), "{s}");
        assert_eq!(parse(&s).unwrap(), q);
    }

    #[test]
    fn floats_serialise_without_an_exponent() {
        // A small-magnitude float must serialise as fixed-point `digits.digits`
        // (the lexer rejects an exponent form), and round-trip.
        use crate::dsl::ir::{CmpOp, Field, OrdOp, Predicate, Query, Value};
        for f in [0.000_000_001_f64, 0.000_25, 123_456.789] {
            let q = Query {
                predicate: Predicate::Comparison {
                    field: Field::Confidence,
                    op: CmpOp::Ord(OrdOp::Eq),
                    value: Value::Float(f),
                },
                stages: Vec::new(),
            };
            let s = serialize(&q);
            let lexeme = s.rsplit("== ").next().unwrap();
            assert!(
                !lexeme.contains(['e', 'E']),
                "exponent in float lexeme {lexeme:?}"
            );
            assert_eq!(parse(&s).unwrap(), q, "round-trip failed for {s:?}");
        }
    }
}
