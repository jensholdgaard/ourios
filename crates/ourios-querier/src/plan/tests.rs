//! The compile-module test suite, moved verbatim with the split
//! (epic #745 wave 3); `super::*` sees the whole `plan` surface
//! through the parent re-glue.

use super::*;
use crate::dsl::ir::SeverityName;
use datafusion::common::tree_node::{TreeNode, TreeNodeRecursion};
use datafusion::logical_expr::Operator;

/// Structural helpers: assert on the lowered `Expr` tree rather than its
/// `Display` output. `DataFusion`'s rendering is not a stable API and this
/// workspace tracks `DataFusion` upgrades (RFC 0021), so a formatting change
/// must not be able to fail these tests without a semantic change.
fn top_operator(e: &Expr) -> Option<Operator> {
    match e {
        Expr::BinaryExpr(b) => Some(b.op),
        _ => None,
    }
}

/// True when `e` is exactly `severity_number <op> 0`.
fn is_severity_vs_unspecified(e: &Expr, want: Operator) -> bool {
    let Expr::BinaryExpr(b) = e else {
        return false;
    };
    if b.op != want {
        return false;
    }
    let lhs_is_severity =
        matches!(b.left.as_ref(), Expr::Column(c) if c.name == columns::SEVERITY_NUMBER);
    let rhs_is_zero = matches!(
        b.right.as_ref(),
        Expr::Literal(ScalarValue::Int64(Some(0)), ..)
    );
    lhs_is_severity && rhs_is_zero
}

fn lowered(op: OrdOp, value: &SeverityValue) -> Expr {
    match compile_severity(op, value) {
        PredExpr::Filter(e) => e,
        _ => panic!("expected a Filter for a severity predicate"),
    }
}

/// The `&DFSchema` seam (epic #745 wave 3): predicate lowering runs
/// against a bare schema — no session, no `DataFrame`, no table
/// registration. A leaf over a present column lowers to a filter; the
/// same leaf over a schema missing that OPTIONAL column collapses to
/// match-none (the RFC 0005 §3.9 absent-column disposition), all
/// decided from the schema alone.
#[test]
fn lowering_needs_only_a_schema() {
    use datafusion::arrow::datatypes::{Field as ArrowField, Schema};

    let leaf = Predicate::Comparison {
        field: Field::Scope,
        op: CmpOp::Ord(OrdOp::Eq),
        value: Value::Str("checkout".to_string()),
    };
    let with_scope = DFSchema::try_from(Schema::new(vec![ArrowField::new(
        columns::SCOPE_NAME,
        DataType::Utf8,
        true,
    )]))
    .expect("schema");
    let lowered =
        compile_predicate(&leaf, &with_scope, &BTreeMap::new(), &BTreeMap::new()).expect("lowers");
    assert!(matches!(lowered, PredExpr::Filter(_)));

    let without_scope = DFSchema::try_from(Schema::new(vec![ArrowField::new(
        columns::TEMPLATE_ID,
        DataType::UInt64,
        true,
    )]))
    .expect("schema");
    let lowered = compile_predicate(&leaf, &without_scope, &BTreeMap::new(), &BTreeMap::new())
        .expect("lowers");
    assert!(matches!(lowered, PredExpr::None));
}

/// RFC0002.21 lowering: a floor admits unspecified, a ceiling excludes it,
/// and an explicit `0` threshold does neither. Asserted on the lowered
/// predicate rather than end-to-end, so a regression is caught at the
/// compile step even if no fixture happens to contain a `0` row.
#[test]
fn severity_lowering_special_cases_unspecified_only_above_zero() {
    // Floors admit unspecified: the lowered predicate is a disjunction
    // whose left arm is `= 0`. That arm is also what defeats min/max
    // pruning, so this doubles as the pruning-correctness guard.
    for op in [OrdOp::Ge, OrdOp::Gt] {
        let e = lowered(op, &SeverityValue::Number(17));
        assert_eq!(
            top_operator(&e),
            Some(Operator::Or),
            "{op:?} against a positive threshold must lower to a disjunction"
        );
        let Expr::BinaryExpr(b) = &e else {
            panic!("checked above")
        };
        assert!(
            is_severity_vs_unspecified(b.left.as_ref(), Operator::Eq),
            "{op:?} must carry a `severity_number = 0` arm"
        );
    }

    // Ceilings exclude it, so a predicate and its negation still partition.
    for op in [OrdOp::Lt, OrdOp::Le] {
        let e = lowered(op, &SeverityValue::Number(17));
        assert_eq!(
            top_operator(&e),
            Some(Operator::And),
            "{op:?} against a positive threshold must lower to a conjunction"
        );
        let Expr::BinaryExpr(b) = &e else {
            panic!("checked above")
        };
        assert!(
            is_severity_vs_unspecified(b.left.as_ref(), Operator::NotEq),
            "{op:?} must carry a `severity_number != 0` arm"
        );
    }

    // A `0` threshold is a question *about* unspecified, not a floor, so
    // `severity > 0` keeps meaning "has a specified severity".
    for op in [OrdOp::Ge, OrdOp::Gt, OrdOp::Lt, OrdOp::Le] {
        let e = lowered(op, &SeverityValue::Number(0));
        let top = top_operator(&e);
        assert!(
            top != Some(Operator::Or) && top != Some(Operator::And),
            "{op:?} against a 0 threshold must lower to a plain comparison, got {top:?}"
        );
    }
}

/// Membership (`==`/`!=`) is unchanged by RFC0002.21 — it is a range or
/// equality test, not a minimum-severity floor, so unspecified gets no
/// special treatment. Covers the bare-name band (`error` ⇒ 17..=20), which
/// is the form the DSL actually encourages, as well as the exact numeric
/// form.
#[test]
fn severity_membership_is_untouched_by_the_unspecified_rule() {
    for (label, value) in [
        ("named band", SeverityValue::Name(SeverityName::Error)),
        ("exact number", SeverityValue::Number(17)),
    ] {
        for op in [OrdOp::Eq, OrdOp::Ne] {
            let e = lowered(op, &value);
            // No arm anywhere in the tree may test against unspecified.
            let mut carries_unspecified = false;
            e.apply(|node| {
                if is_severity_vs_unspecified(node, Operator::Eq)
                    || is_severity_vs_unspecified(node, Operator::NotEq)
                {
                    carries_unspecified = true;
                }
                Ok(TreeNodeRecursion::Continue)
            })
            .expect("walk");
            assert!(
                !carries_unspecified,
                "{label} {op:?} must not gain the unspecified special case"
            );
        }
    }

    // And the band really is a range, so the loop above exercises the band
    // path rather than silently lowering like an exact compare.
    let band = lowered(OrdOp::Eq, &SeverityValue::Name(SeverityName::Error));
    let mut bounds = Vec::new();
    band.apply(|node| {
        if let Expr::Literal(ScalarValue::Int64(Some(v)), ..) = node {
            bounds.push(*v);
        }
        Ok(TreeNodeRecursion::Continue)
    })
    .expect("walk");
    assert!(
        bounds.contains(&17) && bounds.contains(&20),
        "`== error` must lower to the 17..=20 band, got literals {bounds:?}"
    );
}

#[test]
fn duration_nanos_covers_all_units() {
    // Arrange / Act / Assert
    assert_eq!(duration_nanos("30s").unwrap(), 30 * NS_PER_SECOND);
    assert_eq!(duration_nanos("2m").unwrap(), 120 * NS_PER_SECOND);
    assert_eq!(duration_nanos("1h").unwrap(), 3_600 * NS_PER_SECOND);
    assert_eq!(duration_nanos("1d").unwrap(), 86_400 * NS_PER_SECOND);
    assert_eq!(duration_nanos("1w").unwrap(), 7 * 86_400 * NS_PER_SECOND);
}

#[test]
fn bucket_width_beyond_i64_nanoseconds_is_rejected_at_validation() {
    // Arrange — a width that fits `u64` (`duration_nanos` succeeds) but
    // exceeds `i64::MAX` nanoseconds, the type `bucket_expr`'s execution
    // lowering casts into (§6.5 floor-division): 20,000 weeks ≈
    // 12.096e18 ns, past i64::MAX ≈ 9.223e18 ns.
    let width = "20000w";
    let i64_max_ns = u64::try_from(i64::MAX).expect("i64::MAX is non-negative");
    assert!(
        duration_nanos(width).unwrap() > i64_max_ns,
        "the fixture width must actually exceed i64::MAX ns",
    );

    // Act
    let err = validate_group_terms(
        &[GroupTerm::Bucket(width.to_string())],
        &Predicate::Bool(true),
    )
    .expect_err("an i64-overflowing bucket width must fail validation");

    // Assert — rejected here, at the same compile-time gate as every
    // other `by`-list rule, not later during `bucket_expr`'s own cast.
    let QueryError::InvalidQuery { detail } = err else {
        panic!("expected InvalidQuery, got {err:?}");
    };
    assert!(
        detail.contains("i64"),
        "the error names the i64 bound: {detail}",
    );
}

#[test]
fn resolve_window_defaults_to_lookback_when_no_range() {
    // Arrange — no range stage.
    let now = 1_000 * NS_PER_SECOND;
    let w = 60 * NS_PER_SECOND;
    // Act
    let (start, end) = resolve_window(&[], now, w).unwrap();
    // Assert — `[now - W, now]`, never unbounded.
    assert_eq!(end, now);
    assert_eq!(start, now - w);
}

#[test]
fn resolve_window_uses_explicit_range() {
    // Arrange — range(-1h, now).
    let now = 10_000 * NS_PER_SECOND;
    let stages = [Stage::Range(
        Time::Duration {
            neg: true,
            literal: "1h".into(),
        },
        Time::Now,
    )];
    // Act
    let (start, end) = resolve_window(&stages, now, 1).unwrap();
    // Assert
    assert_eq!(end, now);
    assert_eq!(start, now - 3_600 * NS_PER_SECOND);
}

#[test]
fn hex_bytes_decodes_case_insensitively() {
    // Arrange / Act
    let b = hex_bytes(&Field::SpanId, "00Ff10aB00112233").unwrap();
    // Assert
    assert_eq!(b, vec![0x00, 0xff, 0x10, 0xab, 0x00, 0x11, 0x22, 0x33]);
    // Wrong length is rejected.
    assert!(hex_bytes(&Field::SpanId, "00ff").is_err());
    assert!(hex_bytes(&Field::TraceId, "00ff").is_err());
}

#[test]
fn timestamp_nanos_parses_rfc3339() {
    // Arrange / Act
    let ns = timestamp_nanos("1970-01-01T00:00:01Z").unwrap();
    // Assert
    assert_eq!(ns, NS_PER_SECOND);
    assert!(timestamp_nanos("not-a-time").is_err());
}

#[test]
fn pinned_template_id_follows_the_amendment_rule() {
    let pin = |q: &str| pinned_template_id(&crate::dsl::parse(q).unwrap().predicate);
    assert_eq!(pin("template_id == 4"), Some(4));
    assert_eq!(pin("template_id == 4 and service == \"api\""), Some(4));
    assert_eq!(pin("template_id == 4 and template_id == 4"), Some(4));
    // A disjunction / negation pins nothing; conflicting top-level ids
    // pin nothing; `resolves_to` is an alias *set*, not a pin.
    assert_eq!(pin("template_id == 4 or template_id == 7"), None);
    assert_eq!(pin("not template_id == 4"), None);
    assert_eq!(pin("template_id == 4 and template_id == 7"), None);
    assert_eq!(pin("resolves_to(4)"), None);
    assert_eq!(pin("service == \"api\""), None);
}

#[test]
fn validate_enforces_group_term_rules() {
    let v = |q: &str| {
        validate(
            &crate::dsl::parse(q).unwrap(),
            1_000 * NS_PER_SECOND,
            NS_PER_SECOND,
        )
    };
    assert!(v("template_id == 4 | count by param(0), bucket(5m)").is_ok());
    // `bucket` alone has no pinning requirement of its own.
    assert!(v("true | count by bucket(5m)").is_ok());
    assert!(v("service == \"api\" | count by param(0)").is_err());
    assert!(v("template_id == 4 | count by bucket(0s)").is_err());
    assert!(v("template_id == 4 | count by bucket(5m), bucket(1h)").is_err());
    assert!(v("template_id == 4 | count by param(0), param(0)").is_err());
    assert!(v("true | count | count").is_err());
}

#[test]
fn attr_fragment_matches_canonical_json() {
    // The needle the compiler builds for `service == "api"` must be a
    // substring of the canonical JSON the writer stores — this is the
    // contract that keeps the JSON-substring match correct. Built here
    // independently of `attr_match` (which needs a DataFrame) so the
    // fragment shape is pinned without the engine.
    let fragment = "{\"key\":\"service.name\",\"value\":{\"stringValue\":\"api\"}}";
    let stored = "[{\"key\":\"service.name\",\"value\":{\"stringValue\":\"api\"}}]";
    assert!(stored.contains(fragment));
}

#[test]
fn validate_rejects_limit_alongside_count() {
    // A `count [by …] | limit n` pipeline would silently drop the
    // `limit` — `Terminal::Aggregate` never consults `plan.limit`
    // (group-limiting semantics are not implemented) — so `validate`
    // must reject the combination rather than execute the wrong query.
    let v = |q: &str| {
        validate(
            &crate::dsl::parse(q).unwrap(),
            1_000 * NS_PER_SECOND,
            NS_PER_SECOND,
        )
    };
    assert!(v("true | count by service | limit 10").is_err());
    assert!(v("true | count | limit 10").is_err());
    // `limit` alone, or `count` alone, are each still fine.
    assert!(v("true | limit 10").is_ok());
    assert!(v("true | count by service").is_ok());
}

/// Property tests (CLAUDE.md §6.2) for the §6.3 amendment's planner
/// invariants: pin detection ([`pinned_template_id`]) and the `by`-list
/// rules ([`validate_group_terms`], reached here through the real
/// [`validate`] entry point). Each generated case is checked against an
/// independently-computed reference decision — ground truth tracked
/// alongside generation, not derived by calling the code under test —
/// so the properties supplement (not replace) the hand-picked examples
/// above.
mod planner_invariants {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;

    /// A top-level `and`-term together with the pin candidate it
    /// contributes, if any: a bare `template_id == n` comparison pins;
    /// wrapping it in `or`, `not`, or `resolves_to` does not (§6.3
    /// amendment).
    fn pin_term() -> impl Strategy<Value = (Predicate, Option<i64>)> {
        let id = 0i64..4;
        prop_oneof![
            id.clone().prop_map(|n| (
                Predicate::Comparison {
                    field: Field::TemplateId,
                    op: CmpOp::Ord(OrdOp::Eq),
                    value: Value::Int(n),
                },
                Some(n),
            )),
            Just((
                Predicate::Comparison {
                    field: Field::Service,
                    op: CmpOp::Ord(OrdOp::Eq),
                    value: Value::Str("svc".to_string()),
                },
                None,
            )),
            id.clone().prop_map(|n| (
                Predicate::Or(vec![
                    Predicate::Comparison {
                        field: Field::TemplateId,
                        op: CmpOp::Ord(OrdOp::Eq),
                        value: Value::Int(n),
                    },
                    Predicate::Bool(true),
                ]),
                None,
            )),
            id.clone().prop_map(|n| (
                Predicate::Not(Box::new(Predicate::Comparison {
                    field: Field::TemplateId,
                    op: CmpOp::Ord(OrdOp::Eq),
                    value: Value::Int(n),
                })),
                None,
            )),
            id.prop_map(|n| (
                #[allow(clippy::cast_sign_loss)] // `id` is 0..4, always non-negative
                Predicate::Call(Call::ResolvesTo(n as u64)),
                None,
            )),
        ]
    }

    /// A `by`-list element: a bare field (never a pin/param concern), a
    /// `param(n)` from a small pool (biases toward duplicate `n`), or a
    /// `bucket(...)` from a pool that includes a zero-width lexeme (the
    /// only non-positive width the grammar can represent — a signed
    /// literal is not a valid `bucket(...)` argument).
    fn group_term() -> impl Strategy<Value = GroupTerm> {
        prop_oneof![
            Just(GroupTerm::Field(Field::Service)),
            Just(GroupTerm::Field(Field::Body)),
            (0u32..3).prop_map(GroupTerm::Param),
            prop_oneof![
                Just("0s".to_string()),
                Just("5m".to_string()),
                Just("1h".to_string()),
            ]
            .prop_map(GroupTerm::Bucket),
        ]
    }

    proptest! {
        #[test]
        fn validate_matches_the_naive_oracle(
            terms in prop::collection::vec(pin_term(), 1..4),
            by in prop::collection::vec(group_term(), 0..5),
        ) {
            // Ground truth for the pin: all pin-candidate terms present
            // must name the same id (empty ⇒ no pin), independent of
            // `pinned_template_id`'s own traversal.
            let pins: Vec<i64> = terms.iter().filter_map(|(_, p)| *p).collect();
            let expected_pin = match pins.split_first() {
                Some((first, rest)) if rest.iter().all(|n| n == first) => Some(*first),
                _ => None,
            };
            let predicate = if terms.len() == 1 {
                terms[0].0.clone()
            } else {
                Predicate::And(terms.iter().map(|(p, _)| p.clone()).collect())
            };
            prop_assert_eq!(
                pinned_template_id(&predicate),
                expected_pin.map(|n| u64::try_from(n).expect("pin pool is non-negative")),
                "pinned_template_id disagreed with the naive oracle on {:?}",
                predicate,
            );

            // Ground truth for the `by`-list: at most one `param(n)` per
            // `n` and only under a pin, at most one `bucket(...)`, and
            // every present bucket width positive.
            let mut seen_params = BTreeSet::new();
            let mut seen_bucket = false;
            let mut expected_ok = true;
            for term in &by {
                match term {
                    GroupTerm::Param(n) => {
                        if expected_pin.is_none() || !seen_params.insert(*n) {
                            expected_ok = false;
                        }
                    }
                    GroupTerm::Bucket(width) => {
                        if seen_bucket || duration_nanos(width).unwrap_or(0) == 0 {
                            expected_ok = false;
                        }
                        seen_bucket = true;
                    }
                    GroupTerm::Field(_) => {}
                }
            }

            let query = Query {
                predicate,
                stages: vec![Stage::Count { by: by.clone() }],
            };
            let got_ok = validate(&query, 1_000 * NS_PER_SECOND, NS_PER_SECOND).is_ok();
            prop_assert_eq!(
                got_ok,
                expected_ok,
                "validate() disagreed with the naive oracle: pin={:?} by={:?}",
                expected_pin,
                by,
            );
        }
    }
}

// --- RFC 0044 plan-time + lowering units ---

fn reg(entries: &[((u64, u32), &str)]) -> crate::template_registry::TemplateRegistry {
    entries
        .iter()
        .map(|&(key, canonical)| (key, ourios_miner::tree::parse_template(canonical)))
        .collect()
}

fn body_eq_predicate(literal: &str, ne: bool) -> Predicate {
    Predicate::Comparison {
        field: Field::Body,
        op: CmpOp::Ord(if ne { OrdOp::Ne } else { OrdOp::Eq }),
        value: Value::Str(literal.to_owned()),
    }
}

/// The walker fires on `==`/`!=` string comparisons at any nesting
/// depth and on nothing else (a regex on body, a non-body equality).
#[test]
fn uses_body_equality_walks_exactly_the_equality_comparisons() {
    assert!(uses_body_equality(&body_eq_predicate("x", false)));
    assert!(uses_body_equality(&Predicate::Not(Box::new(
        Predicate::And(vec![Predicate::Bool(true), body_eq_predicate("x", true),])
    ))));
    assert!(!uses_body_equality(&Predicate::Comparison {
        field: Field::Body,
        op: CmpOp::Match,
        value: Value::Str("x".to_owned()),
    }));
    assert!(!uses_body_equality(&Predicate::Comparison {
        field: Field::Scope,
        op: CmpOp::Ord(OrdOp::Eq),
        value: Value::Str("x".to_owned()),
    }));
}

/// The collector resolves each distinct literal once (dedup), carries
/// the literal's separator sequence, and resolves a tokenizer-rejected
/// literal to no candidates and no separators.
#[test]
fn collect_body_equalities_dedups_and_handles_tokenizer_failure() {
    let registry = reg(&[((7, 1), "claude_code.api_request")]);
    let p = Predicate::Or(vec![
        body_eq_predicate("claude_code.api_request", false),
        body_eq_predicate("claude_code.api_request", true),
        body_eq_predicate("nul\0literal", false),
    ]);
    let mut out = BTreeMap::new();
    collect_body_equalities(&p, &registry, &mut out);
    assert_eq!(out.len(), 2, "one entry per distinct literal");
    let hit = &out["claude_code.api_request"];
    assert_eq!(hit.candidates.len(), 1);
    assert_eq!(hit.candidates[0].template_id, 7);
    assert_eq!(hit.separators, vec![b"".to_vec(), b"".to_vec()]);
    let rejected = &out["nul\0literal"];
    assert!(rejected.candidates.is_empty());
    assert!(rejected.separators.is_empty());
}

/// The candidate conjunction pins the version-qualified template
/// identity and the 1-based `params`/`separators` element equalities,
/// in order — asserted on the lowered `Expr` text so operand order and
/// indexing regressions are caught at the compile step.
#[test]
fn candidate_arm_lowers_version_params_and_separators_one_based() {
    let arm = candidate_arm(
        &BodyLiteralMatch {
            template_id: 9,
            template_version: 2,
            params: vec!["42".to_owned()],
        },
        &[b"".to_vec(), b" ".to_vec(), b"".to_vec()],
    )
    .to_string();
    assert!(arm.contains("template_id = UInt64(9)"), "{arm}");
    assert!(arm.contains("template_version = UInt32(2)"), "{arm}");
    assert!(
        arm.contains("array_element(params, Int64(1))"),
        "1-based param indexing: {arm}"
    );
    assert!(
        arm.contains("array_element(separators, Int64(3))"),
        "all three separator slots, 1-based: {arm}"
    );
}
