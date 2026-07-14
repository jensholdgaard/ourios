//! RFC 0002 — Query DSL acceptance criteria (RFC0002.1–.16).
//!
//! Red gate (`specified → red`): `#[ignore]`'d `todo!()` stubs
//! until the DSL parser + compiler land in front of the (already
//! implemented) RFC 0007 execution layer. Per `docs/verification.md` §3
//! the scenarios become ignored stubs first, implementations second; each
//! carries the §2.2 doc-comment form so the spec↔test mapping is
//! greppable. RFC0002.1–.11 are green; RFC0002.12–.16 (the 2026-07-15
//! aggregation-execution amendment, RFC 0031 L4) are the current red
//! stubs at the bottom of this file.

/// Proptest strategies that generate **well-formed** [`Query`] IR — shapes the
/// §7 grammar admits and the canonical serialiser renders losslessly — for the
/// RFC0002.7 round-trip property. Constraints baked in (so generated IR always
/// round-trips): `severity` never appears as a scalar comparison field (it has
/// its own predicate form); `and`/`or` are never nested in a same-kind parent
/// (the serialiser flattens those); severity numbers and the int/float/duration
/// literals stay in a re-lexable range; `field_list`/`by`/`project` use only
/// bare fields (resource./attr. are not §7 `field`s); string functions take
/// only a string-typed operand (§6.1).
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
    /// (bounded) string (the serialiser picks dotted vs bracketed; both
    /// re-parse). The arbitrary form is length-capped: an unbounded `".*"`
    /// inflates each leaf's value tree and, multiplied across a recursive
    /// predicate, overflows the generator's construction stack.
    fn attr_key() -> impl Strategy<Value = String> {
        prop_oneof![
            prop::collection::vec("[a-z][a-z0-9_]{0,5}", 1..=3).prop_map(|segs| segs.join(".")),
            ".{0,16}".prop_map(|s: String| s),
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

    /// A string-typed `path` field — the only operands a string function
    /// (`matches`/`contains`/`starts_with`/`ends_with`) accepts (§6.1). Uses
    /// the dotted-ident key form for `resource`/`attr`.
    fn string_field() -> impl Strategy<Value = Field> {
        let dotted_key = prop::collection::vec("[a-z][a-z0-9_]{0,5}", 1..=3)
            .prop_map(|segs: Vec<String>| segs.join("."));
        prop_oneof![
            Just(Field::Body),
            Just(Field::TraceId),
            Just(Field::SpanId),
            Just(Field::Scope),
            Just(Field::Service),
            dotted_key.clone().prop_map(Field::Resource),
            dotted_key.prop_map(Field::Attr),
        ]
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
            ".{0,16}".prop_map(Value::Str),
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
            (string_field(), ".{0,16}").prop_map(|(field, arg)| Call::Matches { field, arg }),
            (string_field(), ".{0,16}").prop_map(|(field, arg)| Call::Contains { field, arg }),
            (string_field(), ".{0,16}").prop_map(|(field, arg)| Call::StartsWith { field, arg }),
            (string_field(), ".{0,16}").prop_map(|(field, arg)| Call::EndsWith { field, arg }),
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

    /// Canonicalise a freely-generated predicate into the form the parser and
    /// serialiser both produce: associative `and`/`or` are flattened (no
    /// same-kind direct nesting) and single-element combinators collapse to
    /// their child. The round-trip property (RFC0002.7) holds for this
    /// canonical shape, so the generator emits arbitrary trees and we compare
    /// against their canonical form.
    fn canonicalize(p: Predicate) -> Predicate {
        match p {
            Predicate::Not(inner) => Predicate::Not(Box::new(canonicalize(*inner))),
            Predicate::And(terms) => Predicate::and(terms.into_iter().map(canonicalize).collect()),
            Predicate::Or(terms) => Predicate::or(terms.into_iter().map(canonicalize).collect()),
            leaf => leaf,
        }
    }

    /// A whole predicate. Built with [`prop_recursive`] so nesting is bounded
    /// (a small depth / node budget): an unbounded recursive generator builds a
    /// value tree deep enough to overflow the test thread's stack during
    /// *generation* on CI's default (~2 MiB) stack. The recursion adds `not`,
    /// `and`, and `or` over the leaf `term`s; the result is canonicalised so it
    /// matches the parser/serialiser's flattened, singleton-collapsed shape.
    fn predicate() -> impl Strategy<Value = Predicate> {
        term()
            .prop_recursive(4, 16, 3, |inner| {
                prop_oneof![
                    inner.clone().prop_map(|p| Predicate::Not(Box::new(p))),
                    prop::collection::vec(inner.clone(), 2..=3).prop_map(Predicate::And),
                    prop::collection::vec(inner, 2..=3).prop_map(Predicate::Or),
                ]
            })
            .prop_map(canonicalize)
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
#[tokio::test]
async fn rfc0002_1_predicate_compiles_to_a_filter() {
    use crate::common::{DEFAULT_WINDOW_NS, NOW, TS0, simple, write_all};
    use ourios_core::tenant::TenantId;
    use ourios_querier::{Querier, QueryRequest};

    // Arrange — a small corpus where template 1 and template 2 sit in
    // distinct hours (distinct files / row groups), so a `template_id`
    // filter prunes a whole row group via statistics.
    let bucket = tempfile::TempDir::new().expect("temp");
    write_all(
        bucket.path(),
        &[simple("a", 1, TS0), simple("a", 1, TS0 + 1_000_000)],
    );
    write_all(
        bucket.path(),
        &[simple("a", 2, TS0 + crate::common::HOUR_NS)],
    );
    let q = Querier::new(bucket.path());
    let tenant = TenantId::new("a");

    // Act — the compiled DSL predicate over the indexed `template_id` key …
    let query = ourios_querier::dsl::parse("template_id == 1").expect("parse");
    let dsl = q
        .run_query(
            &query,
            &tenant,
            NOW,
            DEFAULT_WINDOW_NS,
            Some(&crate::common::no_aliases()),
        )
        .await
        .expect("run_query");
    // … and the equivalent direct execution request.
    let direct = q
        .run(QueryRequest {
            tenant: tenant.clone(),
            time_range: Some((NOW - DEFAULT_WINDOW_NS, NOW)),
            template_id: Some(1),
            severity_text: None,
            limit: None,
        })
        .await
        .expect("run");

    // Assert — identical result AND identical pruning: the DSL compiles to
    // the same pushdown the structured request produces (RFC0002.1 / §4.3).
    assert_eq!(dsl.rows, 2, "two template-1 rows match");
    assert_eq!(dsl.rows, direct.rows, "DSL and request count agree");
    assert_eq!(
        dsl.stats, direct.stats,
        "DSL prunes the scan identically to the request; dsl={:?} direct={:?}",
        dsl.stats, direct.stats,
    );
    assert!(
        dsl.stats.row_groups_pruned >= 1,
        "the template-2 row group is pruned by statistics; stats={:?}",
        dsl.stats,
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
///
/// The compile path's public surface is `run_query` returning the
/// Ourios-owned `QueryResult` / `QueryError` — no `datafusion`/`arrow`/SQL
/// type crosses it (a compile-time guarantee, since the function signature
/// names only Ourios + std types). This asserts the runtime string boundary:
/// an error message from the compile path names no engine token (§4.6).
#[tokio::test]
async fn rfc0002_3_no_datafusion_leakage() {
    use crate::common::{DEFAULT_WINDOW_NS, NOW, TS0, simple, write_all};
    use ourios_core::tenant::TenantId;
    use ourios_querier::{Querier, QueryError};

    // Engine/SQL tokens that must never surface in a compile-path error.
    const LEAK_TOKENS: &[&str] = &[
        "datafusion",
        "arrow",
        "sql",
        "logicalplan",
        "logical plan",
        "recordbatch",
        "select",
        "schema",
    ];

    // Arrange — a real store, and a query whose compilation fails
    // (ordering operator on a JSON-encoded attribute is out of scope).
    let bucket = tempfile::TempDir::new().expect("temp");
    write_all(bucket.path(), &[simple("a", 1, TS0)]);
    let q = Querier::new(bucket.path());
    let tenant = TenantId::new("a");
    let query = ourios_querier::dsl::parse("attr.k > \"x\"").expect("parse");

    // Act
    let err = q
        .run_query(
            &query,
            &tenant,
            NOW,
            DEFAULT_WINDOW_NS,
            Some(&crate::common::no_aliases()),
        )
        .await
        .expect_err("an out-of-scope attribute comparison must be rejected");

    // Assert — a non-empty, leak-free, Ourios-owned error.
    assert!(matches!(err, QueryError::InvalidQuery { .. }));
    let msg = err.to_string().to_ascii_lowercase();
    assert!(!msg.trim().is_empty(), "compile error must be non-empty");
    for token in LEAK_TOKENS {
        assert!(
            !msg.contains(token),
            "compile-path error leaked engine token {token:?}: {msg:?}",
        );
    }
}

/// Scenario RFC0002.4 — A query without an explicit range gets the tenant default window.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[tokio::test]
async fn rfc0002_4_default_time_window() {
    use crate::common::{HOUR_NS, simple, write_all};
    use ourios_core::tenant::TenantId;
    use ourios_querier::Querier;

    // Arrange — `now` and a 1-hour default window. One row sits inside
    // `[now - 1h, now]`; one sits two hours before `now`, outside it.
    let now = crate::common::TS0 + 10 * HOUR_NS;
    let window = HOUR_NS;
    let in_window = now - HOUR_NS / 2;
    let out_of_window = now - 2 * HOUR_NS;

    let bucket = tempfile::TempDir::new().expect("temp");
    write_all(
        bucket.path(),
        &[simple("a", 1, in_window), simple("a", 1, out_of_window)],
    );
    let q = Querier::new(bucket.path());
    let tenant = TenantId::new("a");

    // Act — a query with NO range stage (match-all predicate).
    let query = ourios_querier::dsl::parse("true").expect("parse");
    let r = q
        .run_query(
            &query,
            &tenant,
            now,
            window,
            Some(&crate::common::no_aliases()),
        )
        .await
        .expect("run_query");

    // Assert — the default window applied: only the in-window row matches,
    // the out-of-window row is excluded. (A bug that left the scan unbounded
    // would count both.)
    assert_eq!(
        r.rows, 1,
        "no-range query is bounded to [now - W, now]; the older row is excluded",
    );
}

/// Scenario RFC0002.5 — Bare-identifier severity maps to its `SeverityNumber`.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[tokio::test]
async fn rfc0002_5_severity_name_maps_to_severity_number() {
    use crate::common::{DEFAULT_WINDOW_NS, NOW, TS0, rec, write_all};
    use ourios_core::tenant::TenantId;
    use ourios_querier::Querier;

    // Arrange — one row at each band floor: trace1, debug5, info9, warn13,
    // error17, fatal21, plus one above-floor error (20).
    let bucket = tempfile::TempDir::new().expect("temp");
    let sev = |n: u8, i: u64| {
        rec(
            "a",
            1,
            TS0 + i * 1_000_000,
            n,
            "api",
            "lib.cart",
            None,
            None,
        )
    };
    write_all(
        bucket.path(),
        &[
            sev(1, 0),
            sev(5, 1),
            sev(9, 2),
            sev(13, 3),
            sev(17, 4),
            sev(20, 5),
            sev(21, 6),
        ],
    );
    let q = Querier::new(bucket.path());
    let tenant = TenantId::new("a");

    let run = |text: &'static str| {
        let q = q.clone();
        let tenant = tenant.clone();
        async move {
            let query = ourios_querier::dsl::parse(text).expect("parse");
            q.run_query(
                &query,
                &tenant,
                NOW,
                DEFAULT_WINDOW_NS,
                Some(&crate::common::no_aliases()),
            )
            .await
            .expect("run_query")
            .rows
        }
    };

    // Act / Assert — the name maps to the floor and compiles identically to
    // the numeric form: `severity >= error` ≡ `severity >= 17` (rows with
    // number 17, 20, 21 ⇒ 3).
    assert_eq!(run("severity >= error").await, 3, "error floor is 17");
    assert_eq!(
        run("severity >= error").await,
        run("severity >= 17").await,
        "bare name and numeric form compile identically",
    );
    // Case-insensitive, and the other floors.
    assert_eq!(
        run("severity >= ERROR").await,
        3,
        "name is case-insensitive"
    );
    assert_eq!(
        run("severity >= warn").await,
        4,
        "warn floor is 13 ⇒ 13,17,20,21"
    );
    assert_eq!(run("severity >= info").await, 5, "info floor is 9");
    assert_eq!(
        run("severity >= trace").await,
        7,
        "trace floor is 1 ⇒ all rows"
    );
    assert_eq!(run("severity >= fatal").await, 1, "fatal floor is 21");
    assert_eq!(run("severity == 20").await, 1, "exact numeric severity");
}

/// Scenario RFC0002.5 (band semantics) — a *named* severity equality denotes
/// the whole `OTel` four-wide band, not just its floor; ordering still uses the
/// floor. See `docs/rfcs/0002-query-dsl.md` §5 / §6.1.
#[tokio::test]
async fn rfc0002_5_named_severity_equality_is_a_band() {
    use crate::common::{DEFAULT_WINDOW_NS, NOW, TS0, rec, write_all};
    use ourios_core::tenant::TenantId;
    use ourios_querier::Querier;

    // Arrange — rows inside the error band (17..=20) at 17 and 19, one above
    // the band (21 = fatal), and one below (16 = warn ceiling).
    let bucket = tempfile::TempDir::new().expect("temp");
    let sev = |n: u8, i: u64| {
        rec(
            "a",
            1,
            TS0 + i * 1_000_000,
            n,
            "api",
            "lib.cart",
            None,
            None,
        )
    };
    write_all(
        bucket.path(),
        &[sev(16, 0), sev(17, 1), sev(19, 2), sev(21, 3)],
    );
    let q = Querier::new(bucket.path());
    let tenant = TenantId::new("a");

    let run = |text: &'static str| {
        let q = q.clone();
        let tenant = tenant.clone();
        async move {
            let query = ourios_querier::dsl::parse(text).expect("parse");
            q.run_query(
                &query,
                &tenant,
                NOW,
                DEFAULT_WINDOW_NS,
                Some(&crate::common::no_aliases()),
            )
            .await
            .expect("run_query")
            .rows
        }
    };

    // Act / Assert — `== error` matches the 17 and 19 rows (the band), not
    // just the 17 floor; the 19 row is the regression guard.
    assert_eq!(
        run("severity == error").await,
        2,
        "== error is the 17..=20 band, so 19 matches",
    );
    // `!= error` is everything OUTSIDE the band (16 and 21).
    assert_eq!(
        run("severity != error").await,
        2,
        "!= error excludes the whole band",
    );
    // Ordering still uses the floor (RFC0002.5): `>= error` ≥ 17 ⇒ 17,19,21.
    assert_eq!(
        run("severity >= error").await,
        3,
        ">= error keeps the floor (17)",
    );
    // A numeric equality stays exact — 19 only.
    assert_eq!(run("severity == 19").await, 1, "numeric == is exact");
}

/// Scenario RFC0002.6 — First-class OTel-canonical fields resolve correctly.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[tokio::test]
async fn rfc0002_6_first_class_fields_resolve() {
    use crate::common::{DEFAULT_WINDOW_NS, NOW, TS0, rec, write_all};
    use ourios_core::tenant::TenantId;
    use ourios_querier::Querier;

    // Arrange — three rows distinguished on every first-class field:
    //   row A: service "api",     scope "lib.cart", a trace+span id, ts TS0
    //   row B: service "web",     scope "lib.web",  no ids,           ts TS0+1ms
    //   row C: service "api",     scope "lib.cart", no ids,           ts TS0+2ms
    let trace = [0x11u8; 16];
    let span = [0x22u8; 8];
    let bucket = tempfile::TempDir::new().expect("temp");
    write_all(
        bucket.path(),
        &[
            rec("a", 1, TS0, 9, "api", "lib.cart", Some(trace), Some(span)),
            rec("a", 1, TS0 + 1_000_000, 9, "web", "lib.web", None, None),
            rec("a", 1, TS0 + 2_000_000, 9, "api", "lib.cart", None, None),
        ],
    );
    let q = Querier::new(bucket.path());
    let tenant = TenantId::new("a");

    let run = |text: String| {
        let q = q.clone();
        let tenant = tenant.clone();
        async move {
            let query = ourios_querier::dsl::parse(&text).expect("parse");
            q.run_query(
                &query,
                &tenant,
                NOW,
                DEFAULT_WINDOW_NS,
                Some(&crate::common::no_aliases()),
            )
            .await
            .expect("run_query")
            .rows
        }
    };

    // service → resource["service.name"] (JSON-substring match).
    assert_eq!(run("service == \"api\"".into()).await, 2, "two api rows");
    assert_eq!(run("service == \"web\"".into()).await, 1, "one web row");
    // scope → scope_name.
    assert_eq!(
        run("scope == \"lib.web\"".into()).await,
        1,
        "scope resolves"
    );
    // trace_id / span_id → the dedicated byte columns (hex-decoded).
    assert_eq!(
        run("trace_id == \"11111111111111111111111111111111\"".into()).await,
        1,
        "trace_id resolves to the byte column",
    );
    assert_eq!(
        run("span_id == \"2222222222222222\"".into()).await,
        1,
        "span_id resolves to the byte column",
    );
    // ts → time_unix_nano: a lower bound *between* row A's `ts` (TS0) and its
    // `observed_ts` (TS0 + 1_000) — so the count flips (2 → 3) if `ts` is
    // wrongly resolved to `observed_time_unix_nano`. Excludes row A on `ts`.
    let after = TS0 + 500;
    assert_eq!(
        run(format!("ts >= {after}")).await,
        2,
        "ts resolves to the time column, not observed_time_unix_nano"
    );
    // observed_ts → observed_time_unix_nano (each row's is ts + 1000).
    assert_eq!(
        run(format!("observed_ts >= {}", TS0 + 1_000_001)).await,
        2,
        "observed_ts resolves to its column",
    );
}

/// Scenario RFC0002.6 (attribute `!=` present-key guard) — `attr.k != "v"`
/// (and `service`/`resource.k`) must require the key PRESENT with a different
/// value; a row missing the key entirely does not match. See
/// `docs/rfcs/0002-query-dsl.md` §5 and issue #147 (the JSON-LIKE stopgap).
#[tokio::test]
async fn rfc0002_6_attr_not_equal_requires_present_key() {
    use crate::common::{DEFAULT_WINDOW_NS, NOW, TS0, kv, rec_with_resource, write_all};
    use ourios_core::tenant::TenantId;
    use ourios_querier::Querier;

    // Arrange — three rows on the resource `region` key:
    //   row A: region = "eu"      row B: region = "us"      row C: no region
    let bucket = tempfile::TempDir::new().expect("temp");
    write_all(
        bucket.path(),
        &[
            rec_with_resource("a", TS0, vec![kv("region", "eu")]),
            rec_with_resource("a", TS0 + 1_000_000, vec![kv("region", "us")]),
            rec_with_resource("a", TS0 + 2_000_000, Vec::new()),
        ],
    );
    let q = Querier::new(bucket.path());
    let tenant = TenantId::new("a");

    let run = |text: &'static str| {
        let q = q.clone();
        let tenant = tenant.clone();
        async move {
            let query = ourios_querier::dsl::parse(text).expect("parse");
            q.run_query(
                &query,
                &tenant,
                NOW,
                DEFAULT_WINDOW_NS,
                Some(&crate::common::no_aliases()),
            )
            .await
            .expect("run_query")
            .rows
        }
    };

    // Act / Assert — `!=` matches only the key-present, different-value row
    // (row B), NOT the missing-key row C.
    assert_eq!(
        run("resource.region != \"eu\"").await,
        1,
        "!= requires the key present with a different value (missing-key C excluded)",
    );
    // Equality stays present+exact (row A only).
    assert_eq!(
        run("resource.region == \"eu\"").await,
        1,
        "== is present+exact"
    );
}

/// Scenario RFC0002.6 (compile-time column-type rejections) — regex on a
/// non-text column and a string function on a binary id column are rejected
/// at compile with a specific, leak-free error. See
/// `docs/rfcs/0002-query-dsl.md` §5 / hazard `CLAUDE.md` §4.6.
#[tokio::test]
async fn rfc0002_6_non_text_operators_rejected() {
    use crate::common::{DEFAULT_WINDOW_NS, NOW, TS0, simple, write_all};
    use ourios_core::tenant::TenantId;
    use ourios_querier::{Querier, QueryError};

    const LEAK_TOKENS: &[&str] = &["datafusion", "arrow", "sql", "fixedsizebinary", "binary("];

    let bucket = tempfile::TempDir::new().expect("temp");
    write_all(bucket.path(), &[simple("a", 1, TS0)]);
    let q = Querier::new(bucket.path());
    let tenant = TenantId::new("a");

    // (query, fragment the message must name)
    let cases: &[(&str, &str)] = &[
        ("template_id =~ \"4\"", "template_id"),
        ("flags =~ \"1\"", "flags"),
        ("confidence =~ \"0\"", "confidence"),
        ("lossy =~ \"true\"", "lossy"),
        ("starts_with(trace_id, \"11\")", "trace_id"),
        ("contains(span_id, \"22\")", "span_id"),
    ];

    for (text, fragment) in cases {
        // Act
        let query = ourios_querier::dsl::parse(text).expect("parse");
        let err = q
            .run_query(
                &query,
                &tenant,
                NOW,
                DEFAULT_WINDOW_NS,
                Some(&crate::common::no_aliases()),
            )
            .await
            .expect_err(&format!("{text:?} must be rejected at compile"));

        // Assert — Ourios-owned, names the field, leaks no engine type.
        assert!(matches!(err, QueryError::InvalidQuery { .. }), "{text:?}");
        let msg = err.to_string().to_ascii_lowercase();
        assert!(
            msg.contains(fragment),
            "{text:?} should name {fragment:?}: {msg:?}"
        );
        for token in LEAK_TOKENS {
            assert!(!msg.contains(token), "{text:?} leaked {token:?}: {msg:?}");
        }
    }
}

/// Scenario RFC0002.6 (unsupported stage rejection) — a pipeline stage this
/// slice does not execute (`count`/aggregation/`sort`/`project`/`render`) is
/// rejected with a clear error rather than silently dropped.
#[tokio::test]
async fn rfc0002_6_unsupported_stage_rejected() {
    use crate::common::{DEFAULT_WINDOW_NS, NOW, TS0, simple, write_all};
    use ourios_core::tenant::TenantId;
    use ourios_querier::{Querier, QueryError};

    let bucket = tempfile::TempDir::new().expect("temp");
    write_all(bucket.path(), &[simple("a", 1, TS0)]);
    let q = Querier::new(bucket.path());
    let tenant = TenantId::new("a");

    // `range` + `limit` stay supported; the rest must error, not no-op.
    let cases: &[(&str, &str)] = &[
        ("body == \"x\" | count", "count"),
        ("body == \"x\" | sum(confidence)", "aggregation"),
        ("body == \"x\" | sort ts", "sort"),
        ("body == \"x\" | project body", "project"),
        ("body == \"x\" | render", "render"),
    ];
    for (text, fragment) in cases {
        let query = ourios_querier::dsl::parse(text).expect("parse");
        let err = q
            .run_query(
                &query,
                &tenant,
                NOW,
                DEFAULT_WINDOW_NS,
                Some(&crate::common::no_aliases()),
            )
            .await
            .expect_err(&format!("{text:?} must be rejected, not dropped"));
        assert!(matches!(err, QueryError::InvalidQuery { .. }), "{text:?}");
        let msg = err.to_string().to_ascii_lowercase();
        assert!(
            msg.contains(fragment),
            "{text:?} should name {fragment:?}: {msg:?}"
        );
    }

    // A supported pipeline still runs.
    let ok = ourios_querier::dsl::parse("body == \"x\" | limit 5").expect("parse");
    q.run_query(
        &ok,
        &tenant,
        NOW,
        DEFAULT_WINDOW_NS,
        Some(&crate::common::no_aliases()),
    )
    .await
    .expect("range + limit stay supported");
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
    // The bounded generator keeps predicate trees shallow, but run the property
    // on an explicit large-stack thread anyway so the recursive parse / compare
    // never rides the harness's default (~2 MiB) test-thread stack.
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(round_trip_property)
        .expect("spawn round-trip worker")
        .join()
        .expect("round-trip worker panicked");
}

fn round_trip_property() {
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

    // Arrange — a table of malformed queries, each with the offending construct
    // it must be rejected for and a fragment the error message must name (so a
    // generic "parse error" can't pass — RFC0002.8 requires the message to cite
    // the rejected construct).
    let cases: &[(&str, &str, &str)] = &[
        ("body == ", "missing literal", "literal"),
        (
            "severity =~ error",
            "severity with a regex operator",
            "severity",
        ),
        ("frobnicate == 1", "unknown field", "frobnicate"),
        (
            "body == \"unterminated",
            "unterminated string",
            "unterminated",
        ),
        (
            "body == \"line\nbreak\"",
            "literal newline in a string",
            "newline",
        ),
        ("contains(body)", "wrong function arity", "contains"),
        (
            "contains(body, \"x\", \"y\")",
            "too many function arguments",
            "contains",
        ),
        (
            "template_id == X",
            "bare identifier as a value",
            "identifier",
        ),
        ("body =! 1", "bad operator token", "comparison"),
        ("(body == 1", "unclosed group", "group"),
        ("body == 1 |", "trailing pipe with no stage", "stage"),
        ("body == 1 | limit -3", "negative limit", "limit"),
        (
            "severity == louder",
            "non-severity name on severity",
            "severity",
        ),
    ];

    for (query, what, expected_fragment) in cases {
        // Act
        let err = ourios_querier::dsl::parse(query)
            .expect_err(&format!("expected {what} to be rejected: {query:?}"));

        // Assert — a specific, non-empty message that names the offending
        // construct …
        let msg = err.to_string();
        assert!(
            !msg.trim().is_empty(),
            "error for {what} ({query:?}) must be non-empty",
        );
        let lower = msg.to_ascii_lowercase();
        let fragment = expected_fragment.to_ascii_lowercase();
        assert!(
            lower.contains(&fragment),
            "error for {what} ({query:?}) should name {fragment:?}: {msg:?}",
        );
        // … that leaks no engine/SQL token (case-insensitive).
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
///
/// `resolves_to(n)` expands through the RFC 0001 §6.7 alias map: against an
/// operator-built map where template `B` aliases template `A` under tenant
/// `T`, `resolves_to(A)` matches BOTH `A` and `B` rows (and excludes an
/// un-aliased control `C`), while bare `template_id == A` matches only `A` —
/// the distinction is the whole point. The expansion is symmetric
/// (`resolves_to(B)` is the same class) and strictly per-tenant `[§3.7]`:
/// the alias asserted under `T` does not leak to a tenant `T2` whose map is
/// empty, so the same `resolves_to(A)` there matches only `A`. The other
/// §6.3 template + correctness primitives are covered too: `template_id == n`
/// (the un-expanded singleton), `lossy == true`, and `confidence < 0.7`.
#[tokio::test]
async fn rfc0002_9_resolves_to_expands_via_alias_map() {
    use crate::common::{DEFAULT_WINDOW_NS, NOW, TS0, simple, write_all};
    use ourios_core::alias::{ActorId, AliasMap, Operator};
    use ourios_core::audit::InMemoryAuditSink;
    use ourios_core::tenant::TenantId;
    use ourios_querier::Querier;

    // Arrange — three distinct templates in tenant T (A, B, C) plus the same
    // template ids under a second tenant T2, so cross-tenant isolation has
    // rows to (not) match. Each id sits in its own hour so a `template_id`
    // filter prunes by row-group statistics.
    const A: u64 = 10;
    const B: u64 = 20;
    const C: u64 = 30;
    let bucket = tempfile::TempDir::new().expect("temp");
    write_all(
        bucket.path(),
        &[
            simple("T", A, TS0),
            simple("T", A, TS0 + 1_000),
            simple("T", B, TS0 + crate::common::HOUR_NS),
            simple("T", C, TS0 + 2 * crate::common::HOUR_NS),
            simple("T2", A, TS0),
            simple("T2", B, TS0 + crate::common::HOUR_NS),
            // Distinct-id rows under T so the other template primitives have
            // something to match: one lossy (a lossy String row retains its
            // body, invariant §3.3) and one low-confidence.
            ourios_core::record::MinedRecord {
                template_id: 40,
                lossy_flag: true,
                body: Some("raw lossy line".to_string()),
                ..simple("T", 40, TS0 + 2_000)
            },
            ourios_core::record::MinedRecord {
                template_id: 50,
                confidence: 0.5,
                ..simple("T", 50, TS0 + 3_000)
            },
        ],
    );
    let q = Querier::new(bucket.path());
    let t = TenantId::new("T");
    let t2 = TenantId::new("T2");

    // Build the alias map via the ourios-core operator API: assert B ≡ A
    // under T (and nothing under T2).
    let mut aliases = AliasMap::new();
    let mut sink = InMemoryAuditSink::new();
    let by = Operator::now(ActorId::new("op-test").expect("actor"), "merge drift");
    aliases
        .assert(&mut sink, &t, A, vec![B], by)
        .expect("assert");

    let rows = async |text: &str, tenant: &TenantId, map: &AliasMap| {
        let query = ourios_querier::dsl::parse(text).expect("parse");
        q.run_query(&query, tenant, NOW, DEFAULT_WINDOW_NS, Some(map))
            .await
            .expect("run_query")
            .rows
    };

    // Act / Assert — resolves_to(A) expands to the {A,B} class: 2 A-rows + 1
    // B-row = 3, and the C row is excluded.
    assert_eq!(
        rows("resolves_to(10)", &t, &aliases).await,
        3,
        "resolves_to(A) matches both A and B, not C",
    );
    // Bare template_id == A is the un-expanded singleton: only the 2 A rows.
    assert_eq!(
        rows("template_id == 10", &t, &aliases).await,
        2,
        "template_id == A matches only A — the distinction from resolves_to",
    );
    // Symmetry — resolves_to(B) is the same class.
    assert_eq!(
        rows("resolves_to(20)", &t, &aliases).await,
        3,
        "resolves_to(B) matches the same {{A,B}} class",
    );
    // Cross-tenant isolation `[§3.7]` — using the SAME populated map, the
    // alias asserted under T must not affect T2: resolves_to(A) for T2 is
    // scoped per-tenant and matches only A's row.
    assert_eq!(
        rows("resolves_to(10)", &t2, &aliases).await,
        1,
        "the T alias must not leak into T2 (same map, per-tenant scope): only A's row matches",
    );

    // The remaining template + correctness primitives compile and filter —
    // RFC0002.9 covers template_id / resolves_to / lossy / confidence (§6.3).
    assert_eq!(
        rows("lossy == true", &t, &aliases).await,
        1,
        "lossy == true matches the one lossy row",
    );
    assert_eq!(
        rows("confidence < 0.7", &t, &aliases).await,
        1,
        "confidence < 0.7 matches the one low-confidence row",
    );
}

/// Scenario RFC0002.9, storage-backed (RFC 0005 §3.7.1 / RFC0005.14;
/// issue #148 step 3): the same `resolves_to` expansion as the test
/// above, but with NO injected map — the alias assertion is written
/// to the real RFC 0005 `audit/` stream via the production
/// `ParquetAuditSink`, and the querier DERIVES tenant `T`'s map from
/// storage at compile time. `resolves_to(A)` returns A ∪ {B} while
/// bare `template_id == A` stays exactly A, and the assertion under
/// `T` is invisible to `T2`'s derived map (`CLAUDE.md` §3.7).
#[tokio::test]
async fn rfc0002_9_storage_backed_resolves_to_expands_via_derived_map() {
    use crate::common::{DEFAULT_WINDOW_NS, NOW, TS0, at, simple, write_all, write_audit};
    use ourios_core::alias::ActorId;
    use ourios_core::audit::{AuditEvent, AuditPayload};
    use ourios_core::tenant::TenantId;
    use ourios_querier::Querier;

    // Arrange — the same three-template fixture under T (A, B, C) plus
    // the same ids under T2, and ONE alias assertion B ≡ A for T,
    // persisted through the audit sink rather than handed in.
    const A: u64 = 10;
    const B: u64 = 20;
    const C: u64 = 30;
    let bucket = tempfile::TempDir::new().expect("temp");
    write_all(
        bucket.path(),
        &[
            simple("T", A, TS0),
            simple("T", A, TS0 + 1_000),
            simple("T", B, TS0 + crate::common::HOUR_NS),
            simple("T", C, TS0 + 2 * crate::common::HOUR_NS),
            simple("T2", A, TS0),
            simple("T2", B, TS0 + crate::common::HOUR_NS),
        ],
    );
    write_audit(
        bucket.path(),
        &[AuditEvent {
            tenant_id: TenantId::new("T"),
            timestamp: at(TS0),
            payload: AuditPayload::AliasAsserted {
                representative_id: A,
                member_ids: vec![B],
                actor: ActorId::new("op-test").expect("actor"),
                reason: "deploy re-split the login template".to_string(),
            },
        }],
    );
    let q = Querier::new(bucket.path());
    let t = TenantId::new("T");
    let t2 = TenantId::new("T2");

    // No injected map: `None` selects the §3.7.1 storage derivation.
    let rows = async |text: &str, tenant: &TenantId| {
        let query = ourios_querier::dsl::parse(text).expect("parse");
        q.run_query(&query, tenant, NOW, DEFAULT_WINDOW_NS, None)
            .await
            .expect("run_query")
            .rows
    };

    // Act / Assert — resolves_to(A) expands via the DERIVED {A,B}
    // class: 2 A-rows + 1 B-row, C excluded …
    assert_eq!(
        rows("resolves_to(10)", &t).await,
        3,
        "resolves_to(A) expands via the storage-derived map",
    );
    // … while bare template_id == A stays exactly A.
    assert_eq!(
        rows("template_id == 10", &t).await,
        2,
        "template_id == A is unaffected by the derived alias class",
    );
    // T's stored assertion never folds into T2's derived map.
    assert_eq!(
        rows("resolves_to(10)", &t2).await,
        1,
        "the stored T alias must not leak into T2's derived map",
    );
}

/// Scenario RFC0002.10 — A query is a YAML-safe single-line scalar.
/// See `docs/rfcs/0002-query-dsl.md` §5.
///
/// The Perses-embedding guarantee (§4 P7): the canonical β serialisation of
/// any well-formed query is a single-line scalar that, embedded as a YAML
/// value, survives a YAML round-trip and re-parses to the same query. Two
/// halves cover it: a hand-written table spanning every surface construct, and
/// the bounded `wellformed::query` generator (the RFC0002.7 strategy) so the
/// property holds for arbitrary shapes, not just the table.
#[test]
fn rfc0002_10_yaml_safe_single_line() {
    // A diverse table: predicates (bare/first-class/severity/attr/resource,
    // combinators, negation, calls, escaped + special-char strings, the regex
    // operators, durations/timestamps in `range`) and the full stage set.
    let queries: &[&str] = &[
        "true",
        "false",
        "service == \"api\"",
        "severity >= error",
        "severity == 17",
        "severity != warn",
        "attr.http.status_code == 500",
        "resource.k8s.pod.name == \"web-0\"",
        "body =~ \"timeout.*\" and not lossy == true",
        "template_id == 42 or confidence < 0.7",
        "contains(body, \"connection refused\")",
        "starts_with(service, \"api-\") and ends_with(scope, \".cart\")",
        "matches(body, \"^GET /\") and resolves_to(7)",
        "body == \"with \\\"quotes\\\" and \\\\backslash and \\t tab\"",
        "body == \"a: b, c | d # e & f - g\"",
        "body == \"x\" | range(-1h, now) | count by template_id",
        "true | range(2026-01-02T03:04:05Z, now) | sum(confidence) by service",
        "service == \"api\" | sort count desc | limit 10",
        "true | project body, ts, severity | render",
        "(severity >= error or template_id == 1) and service == \"api\"",
        "ts >= 1700000000000 and observed_ts < 1800000000000",
    ];

    for text in queries {
        // Arrange — the canonical β form of a well-formed query.
        let original = ourios_querier::dsl::parse(text)
            .unwrap_or_else(|e| panic!("table query must parse: {text:?}: {e}"));
        let beta = ourios_querier::dsl::serialize(&original);

        assert_round_trips_through_yaml(&beta, &original);
    }

    // The bounded generator (§7-admissible shapes) so the guarantee is not
    // limited to the hand-written table. Run on a large-stack thread for the
    // same reason RFC0002.7 does — the recursive parse never rides the
    // harness's ~2 MiB default test-thread stack.
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(yaml_round_trip_property)
        .expect("spawn YAML round-trip worker")
        .join()
        .expect("YAML round-trip worker panicked");
}

/// Embed `beta` as a YAML scalar (`query: <scalar>`), round-trip through a YAML
/// parser, extract the scalar, and assert it re-parses to `expected`. Also
/// asserts the serialised form is genuinely single-line (the P7 constraint —
/// the value may need YAML quoting, but the query itself is one line).
fn assert_round_trips_through_yaml(beta: &str, expected: &ourios_querier::dsl::Query) {
    assert!(
        !beta.contains('\n'),
        "canonical serialisation must be single-line: {beta:?}",
    );

    // serde_yaml_ng emits a correctly-quoted scalar for any string, so building
    // the document via the serializer is itself part of the round-trip.
    let mut doc = std::collections::BTreeMap::new();
    doc.insert("query".to_string(), beta.to_string());
    let yaml = serde_yaml_ng::to_string(&doc).expect("embed query as a YAML scalar");

    let recovered: std::collections::BTreeMap<String, String> =
        serde_yaml_ng::from_str(&yaml).expect("YAML round-trips");
    let extracted = recovered.get("query").expect("query scalar survives");

    let reparsed = ourios_querier::dsl::parse(extracted)
        .unwrap_or_else(|e| panic!("YAML-recovered query failed to re-parse: {e}\n  {extracted}"));
    assert_eq!(
        &reparsed, expected,
        "YAML round-trip changed the query; recovered scalar was: {extracted}",
    );
}

fn yaml_round_trip_property() {
    use proptest::prelude::*;

    let mut runner = proptest::test_runner::TestRunner::default();
    runner
        .run(&wellformed::query(), |query| {
            let beta = ourios_querier::dsl::serialize(&query);
            prop_assert!(
                !beta.contains('\n'),
                "canonical serialisation must be single-line: {}",
                beta
            );

            let mut doc = std::collections::BTreeMap::new();
            doc.insert("query".to_string(), beta.clone());
            let yaml = serde_yaml_ng::to_string(&doc)
                .map_err(|e| TestCaseError::fail(format!("YAML embed failed: {e}")))?;
            let recovered: std::collections::BTreeMap<String, String> =
                serde_yaml_ng::from_str(&yaml)
                    .map_err(|e| TestCaseError::fail(format!("YAML parse failed: {e}")))?;
            let extracted = recovered
                .get("query")
                .ok_or_else(|| TestCaseError::fail("query scalar missing".to_string()))?;
            let reparsed = ourios_querier::dsl::parse(extracted).map_err(|e| {
                TestCaseError::fail(format!(
                    "recovered query failed to re-parse: {e}\n  {extracted}"
                ))
            })?;

            prop_assert_eq!(
                reparsed,
                query,
                "YAML round-trip changed the IR: {}",
                extracted
            );
            Ok(())
        })
        .expect("RFC0002.10 YAML round-trip property holds for all generated queries");
}

/// Scenario RFC0002.11 — The structured surface validates against its published schema.
/// See `docs/rfcs/0002-query-dsl.md` §5.
///
/// Two obligations: the schema is **published + snapshot-tested** (committed
/// beside the parser and served by `structured_query_schema`, so drift is
/// PR-visible — §6.6, like the §7 grammar snapshot), and it is the **gate**
/// (well-formed structured requests pass the JSON Schema *and* parse; malformed
/// ones are rejected by the schema before reaching the planner).
#[test]
fn rfc0002_11_structured_surface_schema_validation() {
    use jsonschema::Validator;

    // Snapshot — the served schema is exactly the committed file, and it is a
    // compilable JSON Schema (not just any JSON). A drift between the file and
    // the source-of-truth `include_str!` cannot happen silently.
    let served = ourios_querier::dsl::structured_query_schema();
    let committed = include_str!("../../src/dsl/structured_query.schema.json");
    assert_eq!(
        served, committed,
        "the served schema must equal the committed snapshot (RFC0002.11 / §6.6)",
    );
    let schema_doc: serde_json::Value =
        serde_json::from_str(served).expect("the published schema is valid JSON");
    let validator = Validator::new(&schema_doc).expect("the published schema compiles");

    assert_well_formed_requests_pass(&validator);
    assert_malformed_requests_rejected(&validator);
}

/// Well-formed requests must PASS the schema and then parse to an IR (RFC0002.2).
fn assert_well_formed_requests_pass(validator: &jsonschema::Validator) {
    let valid: &[&str] = &[
        r#"{"predicate":{"const":true}}"#,
        r#"{"predicate":{"field":"service","op":"==","value":"api"}}"#,
        r#"{"predicate":{"field":"severity","op":">=","value":"error"}}"#,
        r#"{"predicate":{"field":{"attr":"http.status_code"},"op":"==","value":500}}"#,
        r#"{"predicate":{"field":{"resource":"k8s.pod.name"},"op":"!=","value":"web-0"}}"#,
        r#"{"predicate":{"call":"contains","args":["body","timeout"]}}"#,
        r#"{"predicate":{"call":"resolves_to","args":[7]}}"#,
        r#"{"predicate":{"field":"severity","op":">=","value":9}}"#,
        r#"{"predicate":{"and":[{"const":true},{"not":{"field":"lossy","op":"==","value":true}}]}}"#,
        r#"{"predicate":{"const":true},"stages":[
            {"range":{"from":"-1h","to":"now"}},
            {"count":{"by":["template_id","service"]}},
            {"avg":"confidence","by":["service"]},
            {"sort":{"key":"count","desc":true}},
            {"project":["body","ts"]},
            {"limit":10},
            {"render":{}}
        ]}"#,
        r#"{"predicate":{"const":true},"stages":[{"render":null}]}"#,
    ];
    for req in valid {
        let instance: serde_json::Value =
            serde_json::from_str(req).expect("a well-formed request is JSON");
        assert!(
            validator.is_valid(&instance),
            "well-formed request must pass the schema: {req}\n  errors: {:?}",
            validator
                .iter_errors(&instance)
                .map(|e| e.to_string())
                .collect::<Vec<_>>(),
        );
        ourios_querier::dsl::parse_structured(req)
            .unwrap_or_else(|e| panic!("schema-valid request must parse: {req}: {e}"));
    }
}

/// Malformed requests must each be REJECTED by the schema *before* the planner.
/// The second tuple element names the construct each case exercises.
fn assert_malformed_requests_rejected(validator: &jsonschema::Validator) {
    let invalid: &[(&str, &str)] = &[
        (r#"{"stages":[]}"#, "missing predicate"),
        (
            r#"{"predicate":{"const":true},"bogus":1}"#,
            "unknown top-level key",
        ),
        (
            r#"{"predicate":{"field":"frobnicate","op":"==","value":1}}"#,
            "unknown field name",
        ),
        (
            r#"{"predicate":{"field":"body","op":"like","value":"x"}}"#,
            "bad operator",
        ),
        (
            r#"{"predicate":{"field":"body","op":"==","value":[1,2]}}"#,
            "non-primitive value",
        ),
        (
            r#"{"predicate":{"field":"body","op":"==","value":1,"extra":true}}"#,
            "extra node key",
        ),
        (
            r#"{"predicate":{"field":{"attr":"k","typo":"x"},"op":"==","value":1}}"#,
            "extra field-object key",
        ),
        (
            r#"{"predicate":{"field":{"resource":"r","attr":"a"},"op":"==","value":1}}"#,
            "both resource and attr",
        ),
        (r#"{"predicate":{"and":[]}}"#, "empty combinator"),
        (
            r#"{"predicate":{"call":"nope","args":["body","x"]}}"#,
            "unknown function",
        ),
        (
            r#"{"predicate":{"const":true},"stages":[{"range":{"from":"-1h","to":"now","step":"5m"}}]}"#,
            "extra stage-body key",
        ),
        (
            r#"{"predicate":{"const":true},"stages":[{"limit":-3}]}"#,
            "negative limit",
        ),
        (
            r#"{"predicate":{"const":true},"stages":[{"frobnicate":1}]}"#,
            "unknown stage kind",
        ),
        (
            r#"{"predicate":{"field":"severity","op":"=~","value":"error"}}"#,
            "regex operator on severity",
        ),
        (
            r#"{"predicate":{"field":"severity","op":"==","value":1.5}}"#,
            "non-integer severity value",
        ),
        (
            r#"{"predicate":{"call":"resolves_to","args":[7,8]}}"#,
            "wrong-arity resolves_to",
        ),
        (
            r#"{"predicate":{"call":"resolves_to","args":[-1]}}"#,
            "negative resolves_to template id",
        ),
        (
            r#"{"predicate":{"call":"contains","args":["body",1]}}"#,
            "non-string second arg to contains",
        ),
        (
            r#"{"predicate":{"call":"contains","args":[1,"x"]}}"#,
            "non-field first arg to contains",
        ),
        (
            r#"{"predicate":{"const":true},"stages":[{"sort":{"key":"attr.http.status_code"}}]}"#,
            "non-identifier sort key",
        ),
    ];
    for (req, what) in invalid {
        let instance: serde_json::Value =
            serde_json::from_str(req).expect("malformed-but-still-JSON request");
        assert!(
            !validator.is_valid(&instance),
            "{what}: schema must reject {req} before the planner",
        );
    }
}

/// Scenario RFC0002.12 — `count [by …]` executes end-to-end and matches a
/// naive oracle. See `docs/rfcs/0002-query-dsl.md` §5 (amendment
/// 2026-07-15).
#[test]
#[ignore = "RFC0002.12 stub — implemented in the aggregation-execution green slice"]
fn rfc0002_12_count_by_matches_naive_oracle() {
    todo!(
        "RFC0002.12 — populated tenant store; `<predicate> | range(…) | \
         count by <field, …>` over ordinary §7 fields (template_id, \
         service) and the bare `count`: the result is the \
         group_key → count map (bare count: the single total), equals \
         a naive oracle that filters + counts the same rows outside \
         the query path, and compile::validate no longer rejects the \
         count stage"
    );
}

/// Scenario RFC0002.13 — `count by param(n), bucket(w)` yields the L4
/// grouped-count map. See `docs/rfcs/0002-query-dsl.md` §5 (amendment
/// 2026-07-15; RFC0031.5).
#[test]
#[ignore = "RFC0002.13 stub — implemented in the aggregation-execution green slice"]
fn rfc0002_13_count_by_param_bucket_grouped_map() {
    todo!(
        "RFC0002.13 — predicate pinning exactly one template_id (§6.3 \
         pinning rule) with `count by param(0), bucket(5m)`: the \
         result is the (bucket, group_key) → count map — buckets the \
         half-open epoch-aligned windows [k·w, (k+1)·w) over the \
         effective timestamp, group keys the stored string form of \
         params slot 0 — equal to a naive oracle and shape-identical \
         to the RFC 0031 §3.5 / RFC0031.1 L4-equivalence map"
    );
}

/// Scenario RFC0002.14 — `param(n)` misuse is a specific compile-time
/// error. See `docs/rfcs/0002-query-dsl.md` §5 (amendment 2026-07-15).
#[test]
#[ignore = "RFC0002.14 stub — implemented in the grammar/compile-validation green slice"]
fn rfc0002_14_param_misuse_specific_error() {
    todo!(
        "RFC0002.14 — (i) `service == \"api\" | count by param(0)` (no \
         template_id pin), (ii) `template_id == 4 or template_id == 7 \
         | count by param(0)` (a disjunction pins nothing), (iii) \
         `resolves_to(4) | count by param(0)` (an alias set, not a \
         pin), (iv) `param(0)` outside a by-list (predicate path or \
         project field): each fails with a specific, leak-free error \
         (RFC0002.8) — (i)–(iii) at compile time citing the \
         single-template pinning rule, (iv) at parse time citing the \
         §7 v1.1 grammar (group_term is confined to by-lists); no \
         query reaches execution"
    );
}

/// Scenario RFC0002.15 — short/NULL params rows are excluded and tallied.
/// See `docs/rfcs/0002-query-dsl.md` §5 (amendment 2026-07-15).
#[test]
#[ignore = "RFC0002.15 stub — implemented in the aggregation-execution green slice"]
fn rfc0002_15_short_params_excluded_and_tallied() {
    todo!(
        "RFC0002.15 — pinned-template rows whose params list is shorter \
         than n + 1 (or whose slot n is NULL) alongside rows carrying \
         slot n; `count by param(n), …`: the short/NULL rows \
         contribute to no group (no synthetic absent bucket), the \
         returned groups equal the naive oracle over the remaining \
         rows, and the excluded-row count is reported per query (a \
         QueryStats field on the RFC 0016 query-metrics path) so the \
         exclusion is observable, not silent"
    );
}

/// Scenario RFC0002.16 — the aggregation path's honest bytes total is the
/// group-column scan alone. See `docs/rfcs/0002-query-dsl.md` §5
/// (amendment 2026-07-15; RFC 0031 §3.6).
#[test]
#[ignore = "RFC0002.16 stub — implemented in the aggregation-execution green slice"]
fn rfc0002_16_honest_bytes_is_group_column_scan_only() {
    todo!(
        "RFC0002.16 — an L4-shaped query (`template_id == N | range(…) \
         | count by param(n), bucket(w)`) under RFC 0031 §3.6 \
         honest-total accounting: the total is the count-scan \
         component only — within surviving row groups, the column \
         chunks read are the predicate + group-term columns \
         (template_id, the effective-time column, params iff a \
         param(n) term is present), never body/separators; the \
         row-materialization component is zero (a map is returned, \
         not rows) and registry_bytes_read is zero (nothing is \
         rendered)"
    );
}
