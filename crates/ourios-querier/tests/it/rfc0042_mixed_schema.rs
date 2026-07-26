//! RFC 0042 §5 — the RFC0042.5 mixed-schema scan (absent /
//! string-promoted / typed-promoted files coexisting), §3.3's rules
//! live: the scan does not error, a type-mismatched promoted column
//! reads as absent (typed `NULL`, never a cast — Arrow's safe
//! `Utf8 → Int64` cast would *parse* string content), and aggregation
//! covers exactly the current-declaration file. The predicate-typing
//! half of the criterion (`==` via the JSON arm on `i64` keys) lands
//! with the predicate-compilation slice.

use ourios_parquet::{PromotedAttributes, PromotedClass, PromotedKey};

use crate::common::{DEFAULT_WINDOW_NS, NOW, TS0, kv, rec_with_attrs, write_all_with_promoted};
use ourios_core::otlp::any_value::Value as AvValue;
use ourios_core::otlp::{AnyValue, KeyValue};

fn kv_int(key: &str, value: i64) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(AvValue::IntValue(value)),
        }),
        ..Default::default()
    }
}

fn typed_i64(key: &str) -> PromotedKey {
    PromotedKey {
        key: key.into(),
        class: PromotedClass::I64,
    }
}

/// Scenario RFC0042.5 — three files in one partition: written with the
/// key unpromoted, promoted as `string`, and promoted as the declared
/// `i64`. See `docs/rfcs/0042-typed-numeric-promotion.md` §5.
#[tokio::test]
async fn rfc0042_5_mixed_schema_scan() {
    let bucket = tempfile::TempDir::new().expect("temp");

    // File A: pre-declaration — `input_tokens` not promoted at all.
    let a = vec![rec_with_attrs(
        "a",
        TS0,
        vec![kv("service.name", "agent")],
        vec![kv_int("input_tokens", 7)],
    )];
    write_all_with_promoted(bucket.path(), &a, &PromotedAttributes::default());

    // File B: promoted under the STRING class, and carrying a
    // string-encoded number — the no-coercion sentinel. If the scan
    // cast (rather than nulled) the mismatched Utf8 column, "1000"
    // would parse and poison the sum below.
    let b = vec![rec_with_attrs(
        "a",
        TS0 + 1_000,
        vec![kv("service.name", "agent")],
        vec![kv("input_tokens", "1000")],
    )];
    let string_class =
        PromotedAttributes::new(Vec::<String>::new(), vec!["input_tokens".to_string()]);
    write_all_with_promoted(bucket.path(), &b, &string_class);

    // File C: the current declaration — `i64` class, real values.
    let c = vec![
        rec_with_attrs(
            "a",
            TS0 + 2_000,
            vec![kv("service.name", "agent")],
            vec![kv_int("input_tokens", 40)],
        ),
        rec_with_attrs(
            "a",
            TS0 + 3_000,
            vec![kv("service.name", "agent")],
            vec![kv_int("input_tokens", 2)],
        ),
    ];
    let i64_class = PromotedAttributes::new_typed([], [typed_i64("input_tokens")]);
    write_all_with_promoted(bucket.path(), &c, &i64_class);

    let querier =
        ourios_querier::Querier::new(bucket.path()).with_promoted_attributes(i64_class.clone());
    let run = |dsl: &'static str| {
        let querier = &querier;
        async move {
            let query = ourios_querier::dsl::parse(dsl).expect("parse DSL");
            querier
                .run_query(
                    &query,
                    &ourios_core::tenant::TenantId::new("a"),
                    NOW,
                    DEFAULT_WINDOW_NS,
                    Some(&crate::common::no_aliases()),
                )
                .await
                .expect("mixed-schema scan must not error")
        }
    };

    // The union schema carries the declared Int64 type; the scan spans
    // all three files without erroring, and a bare count sees every row.
    let all = run("template_id == 1 | count").await;
    assert_eq!(all.rows, 4, "all four rows across the three files");

    // Aggregation covers exactly the current-declaration file: A's
    // column is absent (NULL-filled), B's Utf8 column is
    // type-mismatched so it reads as absent — its "1000" must NOT
    // parse into the sum. C contributes 40 + 2.
    let sum = run("template_id == 1 | sum(attr.input_tokens)").await;
    let group = &sum.aggregate.as_ref().expect("aggregate map")[0];
    assert_eq!(
        group.value.flatten(),
        Some(42.0),
        "sum covers the declared-class file only (no string parsing)"
    );
    assert_eq!(group.count, 4, "every row still counts");
}

/// The undeclared-conflict half of §3.3: with NO declaration for the
/// key (a default querier), a scan spanning a `string`-promoted and an
/// `i64`-promoted file resolves the union column to `Utf8` — and still
/// does not error.
#[tokio::test]
async fn rfc0042_5_undeclared_conflict_resolves_to_utf8() {
    let bucket = tempfile::TempDir::new().expect("temp");
    let b = vec![rec_with_attrs(
        "a",
        TS0,
        vec![kv("service.name", "agent")],
        vec![kv("input_tokens", "junk")],
    )];
    write_all_with_promoted(
        bucket.path(),
        &b,
        &PromotedAttributes::new(Vec::<String>::new(), vec!["input_tokens".to_string()]),
    );
    let c = vec![rec_with_attrs(
        "a",
        TS0 + 1_000,
        vec![kv("service.name", "agent")],
        vec![kv_int("input_tokens", 40)],
    )];
    write_all_with_promoted(
        bucket.path(),
        &c,
        &PromotedAttributes::new_typed([], [typed_i64("input_tokens")]),
    );

    let querier = ourios_querier::Querier::new(bucket.path());
    let run = |dsl: &'static str| {
        let querier = &querier;
        async move {
            let query = ourios_querier::dsl::parse(dsl).expect("parse DSL");
            querier
                .run_query(
                    &query,
                    &ourios_core::tenant::TenantId::new("a"),
                    NOW,
                    DEFAULT_WINDOW_NS,
                    Some(&crate::common::no_aliases()),
                )
                .await
                .expect("undeclared conflict must not error the scan")
        }
    };
    let result = run("template_id == 1 | count").await;
    assert_eq!(result.rows, 2);

    // Distinguish the Utf8 union from a wrong numeric one: under Utf8,
    // the string file's "junk" cell try_casts to NULL and the i64
    // file's Int64 column is type-mismatched (reads as absent), so the
    // sum is an all-NULL group. A numeric union would have let the i64
    // file contribute 40.
    let sum = run("template_id == 1 | sum(attr.input_tokens)").await;
    let group = &sum.aggregate.as_ref().expect("aggregate map")[0];
    assert_eq!(
        group.value.flatten(),
        None,
        "Utf8 union: no file contributes a numeric value"
    );
    assert_eq!(group.count, 2, "both rows still count");
}
