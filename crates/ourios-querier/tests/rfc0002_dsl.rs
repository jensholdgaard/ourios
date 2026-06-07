//! RFC 0002 — Query DSL acceptance criteria (RFC0002.1–.11).
//!
//! Red gate (`specified → red`): `#[ignore]`'d `unimplemented!()` stubs
//! until the DSL parser + compiler land in front of the (already
//! implemented) RFC 0007 execution layer. Per `docs/verification.md` §3
//! the scenarios become ignored stubs first, implementations second; each
//! carries the §2.2 doc-comment form so the spec↔test mapping is
//! greppable.

/// Proptest strategies that generate **well-formed** [`Query`] IR — shapes the
/// §7 grammar admits and the canonical serialiser renders losslessly — for the
/// RFC0002.7 round-trip property. Constraints baked in (so generated IR always
/// round-trips): `severity` never appears as a scalar comparison field (it has
/// its own predicate form); `and`/`or` are never nested in a same-kind parent
/// (the serialiser flattens those); severity numbers and the int/float/duration
/// literals stay in a re-lexable range; `field_list`/`by`/`project` use only
/// bare fields (resource./attr. are not §7 `field`s).
#[cfg(test)]
mod wellformed {
    use ourios_querier::dsl::ir::{
        AggFn, Call, CmpOp, Field, OrdOp, Predicate, Query, SeverityName, SeverityValue, Stage,
        Time, Value,
    };
    use proptest::prelude::*;

    fn ord_op() -> impl Strategy<Value = OrdOp> {
        prop_oneof![
            Just(OrdOp::Eq),
            Just(OrdOp::Ne),
            Just(OrdOp::Lt),
            Just(OrdOp::Le),
            Just(OrdOp::Gt),
            Just(OrdOp::Ge),
        ]
    }

    fn cmp_op() -> impl Strategy<Value = CmpOp> {
        prop_oneof![
            ord_op().prop_map(CmpOp::Ord),
            Just(CmpOp::Match),
            Just(CmpOp::NotMatch),
        ]
    }

    /// A bare top-level field (the §7 `field` set incl. `severity`) — valid in
    /// `field_list` / `by` / `project`.
    fn bare_field() -> impl Strategy<Value = Field> {
        prop_oneof![
            Just(Field::Body),
            Just(Field::Severity),
            Just(Field::Ts),
            Just(Field::ObservedTs),
            Just(Field::TraceId),
            Just(Field::SpanId),
            Just(Field::Scope),
            Just(Field::Flags),
            Just(Field::Service),
            Just(Field::TemplateId),
            Just(Field::Confidence),
            Just(Field::Lossy),
        ]
    }

    /// A raw `OTel` attribute key: either a dotted bare-ident key or an arbitrary
    /// string (the serialiser picks dotted vs bracketed; both re-parse).
    fn attr_key() -> impl Strategy<Value = String> {
        prop_oneof![
            prop::collection::vec("[a-z][a-z0-9_]{0,5}", 1..=3).prop_map(|segs| segs.join(".")),
            ".*".prop_map(|s: String| s),
        ]
    }

    /// A `scalar_path` field: any path **except** bare `severity` (which only
    /// appears in a `severity_cmp`).
    fn scalar_field() -> impl Strategy<Value = Field> {
        prop_oneof![
            prop_oneof![
                Just(Field::Body),
                Just(Field::Ts),
                Just(Field::ObservedTs),
                Just(Field::TraceId),
                Just(Field::SpanId),
                Just(Field::Scope),
                Just(Field::Flags),
                Just(Field::Service),
                Just(Field::TemplateId),
                Just(Field::Confidence),
                Just(Field::Lossy),
            ],
            attr_key().prop_map(Field::Resource),
            attr_key().prop_map(Field::Attr),
        ]
    }

    /// A `path` field (calls / aggregates): any path, severity included.
    fn path_field() -> impl Strategy<Value = Field> {
        prop_oneof![bare_field(), scalar_field()]
    }

    fn duration_lexeme() -> impl Strategy<Value = String> {
        (
            1u32..=9999,
            prop_oneof![Just('s'), Just('m'), Just('h'), Just('d'), Just('w')],
        )
            .prop_map(|(n, u)| format!("{n}{u}"))
    }

    /// A handful of valid RFC 3339 timestamps (canonical, re-lexable).
    fn timestamp_lexeme() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("2026-01-02T03:04:05Z".to_string()),
            Just("2026-06-07T23:59:60Z".to_string()),
            Just("2020-12-31T00:00:00+02:00".to_string()),
            Just("2026-01-02T03:04:05.250Z".to_string()),
        ]
    }

    fn value() -> impl Strategy<Value = Value> {
        prop_oneof![
            ".*".prop_map(Value::Str),
            (-1_000_000i64..1_000_000).prop_map(Value::Int),
            // Simple decimals only: format!("{f}") must re-lex without an
            // exponent (the lexer reads `digits . digits`).
            (-100_000i32..100_000).prop_map(|n| Value::Float(f64::from(n) / 100.0)),
            any::<bool>().prop_map(Value::Bool),
            Just(Value::Null),
            duration_lexeme().prop_map(Value::Duration),
            timestamp_lexeme().prop_map(Value::Timestamp),
        ]
    }

    fn severity_value() -> impl Strategy<Value = SeverityValue> {
        prop_oneof![
            prop_oneof![
                Just(SeverityName::Trace),
                Just(SeverityName::Debug),
                Just(SeverityName::Info),
                Just(SeverityName::Warn),
                Just(SeverityName::Error),
                Just(SeverityName::Fatal),
            ]
            .prop_map(SeverityValue::Name),
            // Non-negative: a `severity` RHS is a bare number token, never `-n`.
            (0i64..1000).prop_map(SeverityValue::Number),
        ]
    }

    fn call() -> impl Strategy<Value = Call> {
        prop_oneof![
            (path_field(), ".*").prop_map(|(field, arg)| Call::Matches { field, arg }),
            (path_field(), ".*").prop_map(|(field, arg)| Call::Contains { field, arg }),
            (path_field(), ".*").prop_map(|(field, arg)| Call::StartsWith { field, arg }),
            (path_field(), ".*").prop_map(|(field, arg)| Call::EndsWith { field, arg }),
            any::<u64>().prop_map(Call::ResolvesTo),
        ]
    }

    /// A non-combinator predicate term (no bare `and`/`or`).
    fn term() -> impl Strategy<Value = Predicate> {
        prop_oneof![
            any::<bool>().prop_map(Predicate::Bool),
            (scalar_field(), cmp_op(), value())
                .prop_map(|(field, op, value)| Predicate::Comparison { field, op, value }),
            (ord_op(), severity_value()).prop_map(|(op, value)| Predicate::Severity { op, value }),
            call().prop_map(Predicate::Call),
        ]
    }

    /// A `unary` node at the given depth: a term, or `not <expr>`.
    fn unary(depth: u32) -> BoxedStrategy<Predicate> {
        if depth == 0 {
            return term().boxed();
        }
        prop_oneof![
            term(),
            expr(depth - 1).prop_map(|p| Predicate::Not(Box::new(p))),
        ]
        .boxed()
    }

    /// A whole predicate at the given depth. The serialiser flattens a
    /// same-kind nested `and`/`or` and drops redundant parens, so this
    /// generator never puts an `and` directly inside an `and` (nor `or` in
    /// `or`): an `and`'s children are unary or `or` nodes, and symmetrically.
    /// That keeps `parse(serialize(q)) == q` exact (RFC0002.7).
    fn expr(depth: u32) -> BoxedStrategy<Predicate> {
        if depth == 0 {
            return unary(0);
        }
        let and_child = prop_oneof![
            unary(depth - 1),
            prop::collection::vec(unary(depth - 1), 2..=3).prop_map(Predicate::Or),
        ];
        let or_child = prop_oneof![
            unary(depth - 1),
            prop::collection::vec(unary(depth - 1), 2..=3).prop_map(Predicate::And),
        ];
        prop_oneof![
            unary(depth),
            prop::collection::vec(and_child, 2..=3).prop_map(Predicate::And),
            prop::collection::vec(or_child, 2..=3).prop_map(Predicate::Or),
        ]
        .boxed()
    }

    fn predicate() -> impl Strategy<Value = Predicate> {
        expr(3)
    }

    fn time() -> impl Strategy<Value = Time> {
        prop_oneof![
            Just(Time::Now),
            (any::<bool>(), duration_lexeme())
                .prop_map(|(neg, literal)| Time::Duration { neg, literal }),
            timestamp_lexeme().prop_map(Time::Timestamp),
        ]
    }

    /// A sort key: a curated set of re-lexable bare idents (a field name or an
    /// aggregate output like `count`).
    fn sort_key() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("count".to_string()),
            Just("template_id".to_string()),
            Just("service".to_string()),
            Just("ts".to_string()),
            Just("confidence".to_string()),
        ]
    }

    fn stage() -> impl Strategy<Value = Stage> {
        let field_list = prop::collection::vec(bare_field(), 1..=3);
        prop_oneof![
            (time(), time()).prop_map(|(a, b)| Stage::Range(a, b)),
            prop::collection::vec(bare_field(), 0..=3).prop_map(|by| Stage::Count { by }),
            (
                prop_oneof![
                    Just(AggFn::Sum),
                    Just(AggFn::Min),
                    Just(AggFn::Max),
                    Just(AggFn::Avg)
                ],
                path_field(),
                prop::collection::vec(bare_field(), 0..=3),
            )
                .prop_map(|(func, path, by)| Stage::Agg { func, path, by }),
            (sort_key(), any::<bool>()).prop_map(|(key, desc)| Stage::Sort { key, desc }),
            any::<u64>().prop_map(Stage::Limit),
            field_list.prop_map(Stage::Project),
            Just(Stage::Render),
        ]
    }

    pub fn query() -> impl Strategy<Value = Query> {
        (predicate(), prop::collection::vec(stage(), 0..=5))
            .prop_map(|(predicate, stages)| Query { predicate, stages })
    }
}

/// Scenario RFC0002.1 — A Branch-B predicate parses and compiles to a filter.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.1)"]
#[test]
fn rfc0002_1_predicate_compiles_to_a_filter() {
    unimplemented!(
        "RFC0002.1 — a Branch-B predicate parses to the query IR and \
         compiles to an internal DataFusion Filter; predicates over the \
         RFC 0007 §4.3 pushdown keys (template_id, time_unix_nano) prune \
         the scan identically to the equivalent ourios_querier request."
    );
}

/// Scenario RFC0002.2 — String DSL and structured surface compile to the same plan.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[test]
fn rfc0002_2_string_and_structured_surfaces_agree() {
    // Arrange — one representative query that exercises the predicate (a
    // bare field, first-class `service`, bare-name severity, an `attr.`
    // path) and the pipe stages (range / count-by / sort / limit),
    // expressed in both front-ends.
    let beta = "service == \"api\" and severity >= error and attr.http.status_code == 500 \
                | range(-1h, now) | count by template_id | sort count desc | limit 10";
    let structured = r#"{
        "predicate": {
            "and": [
                { "field": "service", "op": "==", "value": "api" },
                { "field": "severity", "op": ">=", "value": "error" },
                { "field": { "attr": "http.status_code" }, "op": "==", "value": 500 }
            ]
        },
        "stages": [
            { "range": { "from": "-1h", "to": "now" } },
            { "count": { "by": ["template_id"] } },
            { "sort": { "key": "count", "desc": true } },
            { "limit": 10 }
        ]
    }"#;

    // Act
    let from_string = ourios_querier::dsl::parse(beta).expect("β string parses");
    let from_json =
        ourios_querier::dsl::parse_structured(structured).expect("structured query parses");

    // Assert — the one-core/two-surfaces invariant (§6.4): identical IR.
    assert_eq!(
        from_string, from_json,
        "the string DSL and structured surface must produce the same IR",
    );
}

/// Scenario RFC0002.3 — No `DataFusion`/arrow/SQL leakage.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.3)"]
#[test]
fn rfc0002_3_no_datafusion_leakage() {
    unimplemented!(
        "RFC0002.3 — no datafusion/arrow/SQL type or message appears in \
         any public DSL signature or error string (compile + string-level \
         boundary test, mirroring RFC0007.3)."
    );
}

/// Scenario RFC0002.4 — A query without an explicit range gets the tenant default window.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.4)"]
#[test]
fn rfc0002_4_default_time_window() {
    unimplemented!(
        "RFC0002.4 — a query with no range(...) stage compiles with a \
         time-column filter equal to the tenant default window W, never an \
         unbounded scan."
    );
}

/// Scenario RFC0002.5 — Bare-identifier severity maps to its `SeverityNumber`.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.5)"]
#[test]
fn rfc0002_5_severity_name_maps_to_severity_number() {
    unimplemented!(
        "RFC0002.5 — severity >= error (and warn/info/debug/trace/fatal) \
         map case-insensitively to the §6.1 SeverityNumber floors \
         (trace 1, debug 5, info 9, warn 13, error 17, fatal 21), \
         compiling identically to the numeric form (severity >= 17)."
    );
}

/// Scenario RFC0002.6 — First-class OTel-canonical fields resolve correctly.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.6)"]
#[test]
fn rfc0002_6_first_class_fields_resolve() {
    unimplemented!(
        "RFC0002.6 — service, trace_id, span_id, scope, ts, observed_ts \
         resolve to the RFC 0001 §6.1 columns / resource attributes \
         (service → resource[\"service.name\"], ts → time_unix_nano, …)."
    );
}

/// Scenario RFC0002.7 — Parse/serialise round-trip is idempotent.
/// See `docs/rfcs/0002-query-dsl.md` §5.
///
/// Property test: a generated well-formed [`Query`] serialised to its
/// canonical β string and re-parsed equals the original IR. The generator
/// (`wellformed::query`) emits IR shapes the §7 grammar admits, so this also
/// indirectly covers serialise → parse → serialise stability.
#[test]
fn rfc0002_7_round_trip_idempotent() {
    use proptest::prelude::*;

    let mut runner = proptest::test_runner::TestRunner::default();
    runner
        .run(&wellformed::query(), |query| {
            // Act
            let text = ourios_querier::dsl::serialize(&query);
            let reparsed = ourios_querier::dsl::parse(&text).map_err(|e| {
                TestCaseError::fail(format!(
                    "serialised query failed to re-parse: {e}\n  {text}"
                ))
            })?;

            // Assert — AST idempotence.
            prop_assert_eq!(
                reparsed,
                query,
                "round-trip changed the IR; serialised form was: {}",
                text
            );
            Ok(())
        })
        .expect("RFC0002.7 round-trip property holds for all generated queries");
}

/// Scenario RFC0002.8 — A malformed query yields a specific, leak-free error.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[test]
fn rfc0002_8_malformed_query_specific_error() {
    // Engine/SQL tokens that must never surface in a user-facing DSL error
    // (hazard `CLAUDE.md` §4.6 / RFC0002.3). Lowercase — we scan the
    // lowercased message.
    const LEAK_TOKENS: &[&str] = &[
        "datafusion",
        "arrow",
        "sql",
        "logicalplan",
        "logical plan",
        "recordbatch",
    ];

    // Arrange — a table of malformed queries, each with the offending
    // construct it must be rejected for.
    let cases: &[(&str, &str)] = &[
        ("body == ", "missing literal"),
        ("severity =~ error", "severity with a regex operator"),
        ("frobnicate == 1", "unknown field"),
        ("body == \"unterminated", "unterminated string"),
        ("body == \"line\nbreak\"", "literal newline in a string"),
        ("contains(body)", "wrong function arity"),
        (
            "contains(body, \"x\", \"y\")",
            "too many function arguments",
        ),
        ("template_id == X", "bare identifier as a value"),
        ("body =! 1", "bad operator token"),
        ("(body == 1", "unclosed group"),
        ("body == 1 |", "trailing pipe with no stage"),
        ("body == 1 | limit -3", "negative limit"),
        ("severity == louder", "non-severity name on severity"),
    ];

    for (query, what) in cases {
        // Act
        let err = ourios_querier::dsl::parse(query)
            .expect_err(&format!("expected {what} to be rejected: {query:?}"));

        // Assert — a specific, non-empty message …
        let msg = err.to_string();
        assert!(
            !err.message().is_empty(),
            "error for {what} ({query:?}) must cite the offending construct",
        );
        // … that leaks no engine/SQL token (case-insensitive).
        let lower = msg.to_ascii_lowercase();
        for token in LEAK_TOKENS {
            assert!(
                !lower.contains(token),
                "error for {what} ({query:?}) leaked engine token {token:?}: {msg:?}",
            );
        }
    }
}

/// Scenario RFC0002.9 — Template primitives compile.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.9)"]
#[test]
fn rfc0002_9_template_primitives_compile() {
    unimplemented!(
        "RFC0002.9 — template_id == 42, resolves_to(42), lossy == true, \
         confidence < 0.7 each compile to the documented plan \
         (resolves_to expands to the RFC 0001 §6.7 alias-set membership)."
    );
}

/// Scenario RFC0002.10 — A query is a YAML-safe single-line scalar.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.10)"]
#[test]
fn rfc0002_10_yaml_safe_single_line() {
    unimplemented!(
        "RFC0002.10 — the canonical serialisation of any well-formed query \
         is a single-line scalar that survives a YAML round-trip and \
         re-parses to the same query (the Perses-embedding guarantee)."
    );
}

/// Scenario RFC0002.11 — The structured surface validates against its published schema.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.11)"]
#[test]
fn rfc0002_11_structured_surface_schema_validation() {
    unimplemented!(
        "RFC0002.11 — well-formed structured (MCP) requests pass the \
         published JSON Schema and compile; malformed ones are rejected by \
         the schema before reaching the planner."
    );
}
