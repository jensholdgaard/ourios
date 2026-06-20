//! The query IR — the single core both front-ends (the string DSL and the
//! structured JSON surface) parse into (RFC 0002 §6.4). Typed enums mirror
//! the §7 grammar exactly; no `datafusion`/`arrow`/SQL type appears here
//! (hazard `CLAUDE.md` §4.6).

/// A top-level statement — either a log-record query (the RFC 0002
/// `predicate { | stage }` pipeline) or a RFC 0010 `drift` query over the
/// audit stream. The two are distinct shapes, not a flag on one struct: a
/// drift query has no predicate and no stages (RFC 0010 §6.1), so mixing the
/// two is unrepresentable.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// A log-record query against the `data/` series (RFC 0002).
    Logs(Query),
    /// A `drift from <t1> to <t2>` query against the `audit/` series
    /// (RFC 0010 §6.1).
    Drift(DriftQuery),
}

/// A whole log query: a predicate followed by ordered pipe stages (§7 `query`).
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub predicate: Predicate,
    pub stages: Vec<Stage>,
}

/// A RFC 0010 `drift` query: the closed-form audit-stream question "which
/// templates gained a version in the window `[from, to)`" (RFC 0010 §6.1).
/// It carries only the resolved window — the projection, grouping, and
/// ordering are fixed by §6.3, so there is nothing else to express and no
/// `|` stage to compose. `from` / `to` reuse the §7 [`Time`] production
/// verbatim (RFC 0010 §6.5); the window is half-open `[from, to)`.
#[derive(Debug, Clone, PartialEq)]
pub struct DriftQuery {
    pub from: Time,
    pub to: Time,
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

impl Predicate {
    /// Build an `and` over `terms`, flattening any nested `and` so the IR is
    /// associative-normalised (`a and (b and c)` ≡ `a and b and c`). A
    /// single term collapses to itself. Both front-ends route through this so
    /// they produce the same canonical IR (RFC0002.2) and the serialised form
    /// round-trips (RFC0002.7).
    #[must_use]
    pub fn and(terms: Vec<Predicate>) -> Predicate {
        Self::flatten(terms, true)
    }

    /// Build an `or` over `terms`, flattening any nested `or`. See [`Predicate::and`].
    #[must_use]
    pub fn or(terms: Vec<Predicate>) -> Predicate {
        Self::flatten(terms, false)
    }

    fn flatten(terms: Vec<Predicate>, is_and: bool) -> Predicate {
        let mut flat = Vec::with_capacity(terms.len());
        for term in terms {
            match term {
                Predicate::And(inner) if is_and => flat.extend(inner),
                Predicate::Or(inner) if !is_and => flat.extend(inner),
                other => flat.push(other),
            }
        }
        if flat.len() == 1 {
            flat.pop().expect("len checked")
        } else if is_and {
            Predicate::And(flat)
        } else {
            Predicate::Or(flat)
        }
    }
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
    EventName,
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

impl Field {
    /// Whether a string function (`matches`/`contains`/`starts_with`/
    /// `ends_with`) may take this field as its first operand. Those functions
    /// require a string operand (§6.1); applying one to a numeric, boolean, or
    /// timestamp field is a parse-time type error. `body` is an `OTel`
    /// `AnyValue` and `resource`/`attr` keys are dynamically typed, so all
    /// three are treated as string-compatible.
    #[must_use]
    pub fn is_string_operand(&self) -> bool {
        matches!(
            self,
            Self::Body
                | Self::TraceId
                | Self::SpanId
                | Self::Scope
                | Self::EventName
                | Self::Service
                | Self::Resource(_)
                | Self::Attr(_)
        )
    }
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

impl SeverityName {
    /// The floor of the matching `OTel` `SeverityNumber` range (RFC 0002
    /// §6.1): `trace`→1, `debug`→5, `info`→9, `warn`→13, `error`→17,
    /// `fatal`→21. A bare-name severity comparison compiles to the same
    /// `severity_number` predicate as the numeric form using this floor,
    /// so `severity >= error` is identical to `severity >= 17`. The `OTel`
    /// spec standardises the *ranges* and mandates comparing on
    /// `SeverityNumber`; this name→number choice is Ourios's, aligned with
    /// those ranges.
    #[must_use]
    pub fn floor(self) -> i64 {
        match self {
            Self::Trace => 1,
            Self::Debug => 5,
            Self::Info => 9,
            Self::Warn => 13,
            Self::Error => 17,
            Self::Fatal => 21,
        }
    }

    /// The ceiling of the matching `OTel` `SeverityNumber` band (RFC 0002
    /// §6.1): each name spans four numbers, so `ceil` is `floor + 3`
    /// (`error` → 17..=20, `fatal` → 21..=24). Equality / inequality against
    /// a bare name tests membership in this `floor..=ceil` band; ordering
    /// comparisons use [`SeverityName::floor`] alone.
    #[must_use]
    pub fn ceil(self) -> i64 {
        self.floor() + 3
    }
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

#[cfg(test)]
mod tests {
    use super::{Field, Predicate};

    fn t(b: bool) -> Predicate {
        Predicate::Bool(b)
    }

    #[test]
    fn and_flattens_nested_same_kind_and_collapses_singletons() {
        // Arrange / Act — a nested `and` flattens; a single term collapses.
        let flat = Predicate::and(vec![t(true), Predicate::And(vec![t(false), t(true)])]);
        let single = Predicate::and(vec![t(true)]);
        // Assert
        assert_eq!(flat, Predicate::And(vec![t(true), t(false), t(true)]));
        assert_eq!(single, t(true));
    }

    #[test]
    fn or_does_not_absorb_an_and_child() {
        // A different-kind child stays nested (only same-kind flattens).
        let p = Predicate::or(vec![t(true), Predicate::And(vec![t(false), t(true)])]);
        assert_eq!(
            p,
            Predicate::Or(vec![t(true), Predicate::And(vec![t(false), t(true)])])
        );
    }

    #[test]
    fn is_string_operand_classifies_fields() {
        assert!(Field::Body.is_string_operand());
        assert!(Field::Service.is_string_operand());
        assert!(Field::Attr("k".into()).is_string_operand());
        assert!(!Field::Severity.is_string_operand());
        assert!(!Field::Lossy.is_string_operand());
        assert!(!Field::Ts.is_string_operand());
    }
}
