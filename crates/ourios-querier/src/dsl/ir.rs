//! The query IR — the single core both front-ends (the string DSL and the
//! structured JSON surface) parse into (RFC 0002 §6.4). Typed enums mirror
//! the §7 grammar exactly; no `datafusion`/`arrow`/SQL type appears here
//! (hazard `CLAUDE.md` §4.6).

/// A whole query: a predicate followed by ordered pipe stages (§7 `query`).
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub predicate: Predicate,
    pub stages: Vec<Stage>,
}

/// A boolean expression over the `OTel` log data model (§7 `predicate`).
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    /// `true` (match-all / "no filter") or `false` (match-none) — §7 `bool_lit`.
    Bool(bool),
    /// A scalar comparison `scalar_path cmp_op literal` (§7 `scalar_cmp`).
    Comparison {
        field: Field,
        op: CmpOp,
        value: Value,
    },
    /// A severity comparison `severity ord_op (name|number)` (§7 `severity_cmp`).
    /// Always defined on the `OTel` `SeverityNumber`, never `severity_text`.
    Severity { op: OrdOp, value: SeverityValue },
    /// A boolean-returning function term (§7 `call`).
    Call(Call),
    /// `not <unary>` / `!<unary>`.
    Not(Box<Predicate>),
    /// `a and b and …` (§7 `and_expr`).
    And(Vec<Predicate>),
    /// `a or b or …` (§7 `or_expr`).
    Or(Vec<Predicate>),
}

/// A path into the `OTel` log data model (§7 `path` / `field`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    Body,
    Severity,
    Ts,
    ObservedTs,
    TraceId,
    SpanId,
    Scope,
    Flags,
    Service,
    TemplateId,
    Confidence,
    Lossy,
    /// `resource.<key>` / `resource["<key>"]` — the raw `OTel` attribute key.
    Resource(String),
    /// `attr.<key>` / `attr["<key>"]` — the raw `OTel` attribute key.
    Attr(String),
}

/// Ordering operators valid on any scalar and on severity (§7 `ord_op`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrdOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// Comparison operators for scalar paths — the ordering set plus the two
/// regex operators (§7 `cmp_op`). Severity may use only the [`OrdOp`] subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Ord(OrdOp),
    /// `=~` — regex match.
    Match,
    /// `!~` — regex non-match.
    NotMatch,
}

/// A literal value (§7 `literal`). Duration and timestamp are kept as
/// validated lexical strings for this slice (the compiler resolves them).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    /// A duration literal, e.g. `30s`, `1h`, `1d`, `1w` — its lexical form.
    Duration(String),
    /// An RFC 3339 timestamp — its lexical form.
    Timestamp(String),
}

/// The right-hand side of a severity comparison (§7 `severity_cmp`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeverityValue {
    /// A bare severity name, e.g. `error` (§7 `severity_name`).
    Name(SeverityName),
    /// A numeric `SeverityNumber`, e.g. `17`.
    Number(i64),
}

/// The six `OTel` severity levels usable as a `severity` RHS (§7 `severity_name`).
/// Parsed case-insensitively; the canonical serialised form is lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeverityName {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

/// A boolean-returning function call (§7 `call`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// `matches(path, regex)`.
    Matches { field: Field, arg: String },
    /// `contains(path, s)`.
    Contains { field: Field, arg: String },
    /// `starts_with(path, s)`.
    StartsWith { field: Field, arg: String },
    /// `ends_with(path, s)`.
    EndsWith { field: Field, arg: String },
    /// `resolves_to(template_id)` — template + its drift aliases (§6.3).
    ResolvesTo(u64),
}

/// A pipe stage (§7 `stage`).
#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    /// `range(from, to)`.
    Range(Time, Time),
    /// `count [by field, …]`.
    Count { by: Vec<Field> },
    /// `sum|min|max|avg(path) [by field, …]`.
    Agg {
        func: AggFn,
        path: Field,
        by: Vec<Field>,
    },
    /// `sort <field-or-aggregate> [asc|desc]`.
    Sort { key: String, desc: bool },
    /// `limit <n>`.
    Limit(u64),
    /// `project field, …`.
    Project(Vec<Field>),
    /// `render`.
    Render,
}

/// A scalar aggregation function (§7 `agg_fn`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFn {
    Sum,
    Min,
    Max,
    Avg,
}

/// A `range(...)` bound (§7 `time`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Time {
    /// The `now` keyword.
    Now,
    /// A relative duration, optionally negated (e.g. `-1h`); `literal` is
    /// the bare duration lexeme (`1h`), `neg` carries the leading `-`.
    Duration { neg: bool, literal: String },
    /// An RFC 3339 timestamp — its lexical form.
    Timestamp(String),
}
