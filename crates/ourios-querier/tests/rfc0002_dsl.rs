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

/// Shared fixtures: a real RFC 0005 store written by `ourios-parquet`, the
/// same way `tests/execution.rs` builds one, so the compiled DSL runs against
/// genuine Parquet (predicate pushdown + statistics, not a mock).
#[cfg(test)]
mod fixtures {
    use std::collections::HashMap;
    use std::path::Path;

    use ourios_core::audit::ParamType;
    use ourios_core::otlp::any_value::Value as AvValue;
    use ourios_core::otlp::{AnyValue, KeyValue};
    use ourios_core::record::{BodyKind, MinedRecord, Param};
    use ourios_core::tenant::TenantId;
    use ourios_parquet::{PartitionKey, Writer};

    /// 2026-04-02T10:58:00 UTC — the same base instant the execution tests
    /// use, so all fixture rows land in one `hour=` partition unless bumped.
    pub const TS0: u64 = 1_775_127_480_000_000_000;
    /// One hour in nanoseconds.
    pub const HOUR_NS: u64 = 3_600_000_000_000;

    pub fn kv(key: &str, value: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(AvValue::StringValue(value.to_string())),
            }),
            ..Default::default()
        }
    }

    /// A fully-populated fixture record so every first-class field (§6.2) has
    /// a non-trivial value to query: a `service.name` resource attribute, a
    /// `scope`, an explicit severity, and optional trace/span ids.
    #[allow(clippy::too_many_arguments)]
    pub fn rec(
        tenant: &str,
        template_id: u64,
        ts_ns: u64,
        severity_number: u8,
        service: &str,
        scope: &str,
        trace_id: Option<[u8; 16]>,
        span_id: Option<[u8; 8]>,
    ) -> MinedRecord {
        MinedRecord {
            tenant_id: TenantId::new(tenant),
            template_id,
            template_version: 1,
            severity_number,
            severity_text: None,
            scope_name: Some(scope.to_string()),
            scope_version: Some("1.0.0".to_string()),
            time_unix_nano: ts_ns,
            observed_time_unix_nano: Some(ts_ns + 1_000),
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            resource_attributes: vec![kv("service.name", service)],
            trace_id,
            span_id,
            flags: 0x01,
            event_name: None,
            body_kind: BodyKind::String,
            params: vec![Param {
                type_tag: ParamType::Num,
                value: "42".to_string(),
            }],
            separators: vec![String::new(), " ".to_string()],
            body: None,
            confidence: 1.0,
            lossy_flag: false,
        }
    }

    /// A minimal record: template 1, INFO, service "api", scope "lib.cart".
    pub fn simple(tenant: &str, template_id: u64, ts_ns: u64) -> MinedRecord {
        rec(tenant, template_id, ts_ns, 9, "api", "lib.cart", None, None)
    }

    /// A record with explicit resource attributes (overriding the default
    /// single `service.name`), so a test can give one row a key and another
    /// row none.
    pub fn rec_with_resource(
        tenant: &str,
        ts_ns: u64,
        resource_attributes: Vec<KeyValue>,
    ) -> MinedRecord {
        MinedRecord {
            resource_attributes,
            ..rec(tenant, 1, ts_ns, 9, "api", "lib.cart", None, None)
        }
    }

    pub fn write_all(bucket: &Path, recs: &[MinedRecord]) {
        let mut by_part: HashMap<PartitionKey, Vec<MinedRecord>> = HashMap::new();
        for r in recs {
            by_part
                .entry(PartitionKey::derive(r).expect("derive partition"))
                .or_default()
                .push(r.clone());
        }
        for (part, rs) in by_part {
            let mut w = Writer::open(bucket, part).expect("open writer");
            w.append_records(&rs).expect("append");
            w.close().expect("close");
        }
    }

    /// A window wide enough that a query with no `range(...)` (which gets the
    /// default look-back ending at `now`) still covers all fixture rows.
    pub const DEFAULT_WINDOW_NS: u64 = 30 * 24 * HOUR_NS;
    /// A `now` reference comfortably after the fixture instants.
    pub const NOW: u64 = TS0 + 24 * HOUR_NS;
}

/// Scenario RFC0002.1 — A Branch-B predicate parses and compiles to a filter.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[tokio::test]
async fn rfc0002_1_predicate_compiles_to_a_filter() {
    use fixtures::{DEFAULT_WINDOW_NS, NOW, TS0, simple, write_all};
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
    write_all(bucket.path(), &[simple("a", 2, TS0 + fixtures::HOUR_NS)]);
    let q = Querier::new(bucket.path());
    let tenant = TenantId::new("a");

    // Act — the compiled DSL predicate over the indexed `template_id` key …
    let query = ourios_querier::dsl::parse("template_id == 1").expect("parse");
    let dsl = q
        .run_query(&query, &tenant, NOW, DEFAULT_WINDOW_NS)
        .await
        .expect("run_query");
    // … and the equivalent direct execution request.
    let direct = q
        .run(QueryRequest {
            tenant: tenant.clone(),
            time_range: Some((NOW - DEFAULT_WINDOW_NS, NOW)),
            template_id: Some(1),
            severity_text: None,
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
    use fixtures::{DEFAULT_WINDOW_NS, NOW, TS0, simple, write_all};
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
        .run_query(&query, &tenant, NOW, DEFAULT_WINDOW_NS)
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
    use fixtures::{HOUR_NS, simple, write_all};
    use ourios_core::tenant::TenantId;
    use ourios_querier::Querier;

    // Arrange — `now` and a 1-hour default window. One row sits inside
    // `[now - 1h, now]`; one sits two hours before `now`, outside it.
    let now = fixtures::TS0 + 10 * HOUR_NS;
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
        .run_query(&query, &tenant, now, window)
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
    use fixtures::{DEFAULT_WINDOW_NS, NOW, TS0, rec, write_all};
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
            q.run_query(&query, &tenant, NOW, DEFAULT_WINDOW_NS)
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
    use fixtures::{DEFAULT_WINDOW_NS, NOW, TS0, rec, write_all};
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
            q.run_query(&query, &tenant, NOW, DEFAULT_WINDOW_NS)
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
    use fixtures::{DEFAULT_WINDOW_NS, NOW, TS0, rec, write_all};
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
            q.run_query(&query, &tenant, NOW, DEFAULT_WINDOW_NS)
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
    use fixtures::{DEFAULT_WINDOW_NS, NOW, TS0, kv, rec_with_resource, write_all};
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
            q.run_query(&query, &tenant, NOW, DEFAULT_WINDOW_NS)
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
    use fixtures::{DEFAULT_WINDOW_NS, NOW, TS0, simple, write_all};
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
            .run_query(&query, &tenant, NOW, DEFAULT_WINDOW_NS)
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
    use fixtures::{DEFAULT_WINDOW_NS, NOW, TS0, simple, write_all};
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
            .run_query(&query, &tenant, NOW, DEFAULT_WINDOW_NS)
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
    q.run_query(&ok, &tenant, NOW, DEFAULT_WINDOW_NS)
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
/// `template_id == 42`, `resolves_to(42)`, `lossy == true`, and
/// `confidence < 0.7` each compile to the documented plan and run against a
/// real RFC 0005 store.
///
/// `resolves_to(n)` is *specified* to expand to the RFC 0001 §6.7 cross-alias
/// set, but no alias index is reachable by the querier yet (RFC 0001 §6.7/§9
/// leave the alias-index write path unspecified; RFC 0005 has no alias
/// column), so it honestly compiles to the base member `template_id == n`.
/// This test pins that current semantics: `resolves_to(42)` returns exactly
/// the same rows as `template_id == 42` (every version of leaf 42, since
/// `template_id` is stable across widenings — RFC 0001 §6.1), and crucially
/// does NOT pull in template 99. When the alias index lands (#148) this test
/// gets a sibling that asserts the cross-alias widening; the base-member
/// behaviour asserted here stays correct.
#[tokio::test]
async fn rfc0002_9_template_primitives_compile() {
    use fixtures::{DEFAULT_WINDOW_NS, NOW, TS0, rec, simple, write_all};
    use ourios_core::record::MinedRecord;
    use ourios_core::tenant::TenantId;
    use ourios_querier::Querier;

    // Arrange — template 42 in two rows, an unrelated template 99 (the
    // "not an alias" control), one lossy row, and one low-confidence row.
    let lossy_row = MinedRecord {
        lossy_flag: true,
        // A lossy reconstruction MUST retain the original body (invariant §3.3 /
        // RFC 0005 §3.2), or the writer rejects the row.
        body: Some("the original line".to_string()),
        ..rec("a", 7, TS0 + 3_000_000, 9, "api", "lib.cart", None, None)
    };
    let low_conf_row = MinedRecord {
        confidence: 0.5,
        ..rec("a", 8, TS0 + 4_000_000, 9, "api", "lib.cart", None, None)
    };
    let bucket = tempfile::TempDir::new().expect("temp");
    write_all(
        bucket.path(),
        &[
            simple("a", 42, TS0),
            simple("a", 42, TS0 + 1_000_000),
            simple("a", 99, TS0 + 2_000_000),
            lossy_row,
            low_conf_row,
        ],
    );
    let q = Querier::new(bucket.path());
    let tenant = TenantId::new("a");

    let run = |text: &'static str| {
        let q = q.clone();
        let tenant = tenant.clone();
        async move {
            let query = ourios_querier::dsl::parse(text).expect("parse");
            q.run_query(&query, &tenant, NOW, DEFAULT_WINDOW_NS)
                .await
                .expect("run_query")
                .rows
        }
    };

    // Act / Assert — exact template id matches its two rows, not template 99.
    assert_eq!(run("template_id == 42").await, 2, "two template-42 rows");
    // `resolves_to(42)` compiles to the base member today (#148): same rows as
    // `template_id == 42`, and it must NOT pull in the unrelated template 99.
    assert_eq!(
        run("resolves_to(42)").await,
        run("template_id == 42").await,
        "resolves_to(n) is the base member until the alias index lands (#148)",
    );
    assert_eq!(
        run("resolves_to(99)").await,
        1,
        "resolves_to(99) matches only its own (single) row — no alias bleed",
    );
    // The correctness primitives compile and filter on their columns.
    assert_eq!(run("lossy == true").await, 1, "one lossy row");
    assert_eq!(run("confidence < 0.7").await, 1, "one low-confidence row");
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
