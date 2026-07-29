//! RFC0044.1/.2/.3/.4/.5 — `body ==`/`!=` over template-mined records:
//! the two-arm compile (physical body column OR the plan-time template
//! arm) makes body equality correct for records whose physical body is
//! NULL because the template carries the text — the #664 class.
//!
//! See `docs/rfcs/0044-template-aware-body-equality.md` §5.

use crate::common::{DEFAULT_WINDOW_NS, NOW, TS0, at, no_aliases, simple, write_all, write_audit};
use ourios_core::audit::{
    AuditEvent, AuditPayload, ParamType, TemplateChange, hash_triggering_line,
};
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_querier::{Querier, QueryResult};

/// A `template_created` audit event carrying the canonical (space-form)
/// template — what the registry fold parses back into tokens.
fn created(tenant: &str, template_id: u64, canonical: &str, ts_ns: u64) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: at(ts_ns),
        payload: AuditPayload::Template {
            template_id,
            triggering_line_hash: hash_triggering_line(canonical.as_bytes()),
            triggering_line_sample: None,
            change: TemplateChange::Created {
                new_template: canonical.to_owned(),
            },
        },
    }
}

/// A faithfully mined record: physical body NULL, the template + params +
/// separators carry the text (the #664 shape).
fn mined(
    tenant: &str,
    template_id: u64,
    ts_ns: u64,
    params: &[&str],
    separators: &[&str],
) -> MinedRecord {
    MinedRecord {
        params: params
            .iter()
            .map(|v| Param {
                type_tag: ParamType::Str,
                value: (*v).to_string(),
            })
            .collect(),
        separators: separators.iter().map(|s| (*s).to_string()).collect(),
        body: None,
        ..simple(tenant, template_id, ts_ns)
    }
}

async fn run(bucket: &std::path::Path, dsl: &str) -> QueryResult {
    let q = Querier::new(bucket);
    let query = ourios_querier::dsl::parse(dsl).expect("parse");
    q.run_query(
        &query,
        &TenantId::new("t"),
        NOW,
        DEFAULT_WINDOW_NS,
        Some(&no_aliases()),
    )
    .await
    .expect("run_query")
}

/// The five-record fixture the scenarios share:
/// 1. zero-param mined `claude_code.api_request` (template 1)
/// 2. parameterized mined `user 4711 logged in from 10.0.0.3` (template 2)
/// 3. parameterized mined `user 4712 logged in from 10.0.0.9` (template 2)
/// 4. retained body `kept verbatim body` (low-confidence, non-lossy)
/// 5. structured body whose canonical JSON is `{"a":1}`
fn seed(bucket: &std::path::Path) {
    write_audit(
        bucket,
        &[
            created("t", 1, "claude_code.api_request", TS0),
            created("t", 2, "user <*> logged in from <*>", TS0 + 1),
        ],
    );
    let sep6 = ["", " ", " ", " ", " ", " ", ""];
    write_all(
        bucket,
        &[
            mined("t", 1, TS0 + 10, &[], &["", ""]),
            mined("t", 2, TS0 + 20, &["4711", "10.0.0.3"], &sep6),
            mined("t", 2, TS0 + 30, &["4712", "10.0.0.9"], &sep6),
            MinedRecord {
                body: Some("kept verbatim body".to_owned()),
                confidence: 0.2,
                params: Vec::new(),
                separators: vec![String::new(), String::new()],
                ..simple("t", 3, TS0 + 40)
            },
            MinedRecord {
                body_kind: BodyKind::Structured,
                body: Some(r#"{"a":1}"#.to_owned()),
                params: Vec::new(),
                separators: Vec::new(),
                ..simple("t", 4, TS0 + 50)
            },
        ],
    );
}

/// Scenario RFC0044.1 — the #664 reproduction matches.
/// See `docs/rfcs/0044-template-aware-body-equality.md` §5.
#[tokio::test]
async fn rfc0044_1_zero_param_mined_body_matches() {
    let bucket = tempfile::TempDir::new().expect("temp");
    seed(bucket.path());
    let result = run(bucket.path(), r#"body == "claude_code.api_request""#).await;
    assert_eq!(
        result.rows, 1,
        "the mined record whose body IS the template must match"
    );
    assert!(
        result.stats.row_groups_scanned > 0,
        "the matching row group is scanned, not pruned: {:?}",
        result.stats,
    );
}

/// Scenario RFC0044.2 — parameterized unification matches exactly the
/// record whose original line equals the literal.
/// See `docs/rfcs/0044-template-aware-body-equality.md` §5.
#[tokio::test]
async fn rfc0044_2_parameterized_literal_matches_exactly_one_record() {
    let bucket = tempfile::TempDir::new().expect("temp");
    seed(bucket.path());
    let hit = run(
        bucket.path(),
        r#"body == "user 4711 logged in from 10.0.0.3""#,
    )
    .await;
    assert_eq!(hit.rows, 1, "only the 4711/10.0.0.3 record");
    let sibling = run(
        bucket.path(),
        r#"body == "user 4712 logged in from 10.0.0.9""#,
    )
    .await;
    assert_eq!(sibling.rows, 1, "only the 4712/10.0.0.9 record");
    let none = run(
        bucket.path(),
        r#"body == "user 4711 logged in from 10.0.0.9""#,
    )
    .await;
    assert_eq!(
        none.rows, 0,
        "a cross of the two param sets matches nothing"
    );
}

/// Scenario RFC0044.3 — a retained (low-confidence) body matches via the
/// physical arm. See `docs/rfcs/0044-template-aware-body-equality.md` §5.
#[tokio::test]
async fn rfc0044_3_retained_body_matches_via_the_physical_arm() {
    let bucket = tempfile::TempDir::new().expect("temp");
    seed(bucket.path());
    let result = run(bucket.path(), r#"body == "kept verbatim body""#).await;
    assert_eq!(result.rows, 1);
}

/// Scenario RFC0044.4 — `!=` does not silently drop mined records.
/// See `docs/rfcs/0044-template-aware-body-equality.md` §5.
#[tokio::test]
async fn rfc0044_4_ne_admits_mined_records_that_differ() {
    let bucket = tempfile::TempDir::new().expect("temp");
    seed(bucket.path());
    let result = run(bucket.path(), r#"body != "claude_code.api_request""#).await;
    // The two template-2 records and the retained body differ; the
    // matching template-1 record is excluded; the structured record is
    // excluded from both operators (§3.4).
    assert_eq!(
        result.rows, 3,
        "mined + retained differ; matching and structured excluded"
    );
}

/// Scenario RFC0044.5 — a string literal never matches a structured body,
/// even when the literal equals its canonical JSON bytes; the query
/// succeeds. See `docs/rfcs/0044-template-aware-body-equality.md` §5.
#[tokio::test]
async fn rfc0044_5_structured_bodies_are_excluded_not_errored() {
    let bucket = tempfile::TempDir::new().expect("temp");
    seed(bucket.path());
    let result = run(bucket.path(), r#"body == "{\"a\":1}""#).await;
    assert_eq!(result.rows, 0, "canonical-JSON bytes must not string-match");
}

/// An overflow-spilled record: the stored param is truncated, the true
/// body is retained. A literal crafted from the truncated value must NOT
/// match via the template arm — the retained body is the truth.
#[tokio::test]
async fn rfc0044_overflow_spill_cannot_false_match_a_crafted_literal() {
    let bucket = tempfile::TempDir::new().expect("temp");
    write_audit(bucket.path(), &[created("t", 5, "spill <*>", TS0)]);
    write_all(
        bucket.path(),
        &[MinedRecord {
            params: vec![Param {
                type_tag: ParamType::Overflow,
                value: "TRUNCATED".to_owned(),
            }],
            separators: vec![String::new(), " ".to_owned(), String::new()],
            body: Some("spill TRUNCATED-BUT-THE-REAL-LINE-WENT-ON".to_owned()),
            ..simple("t", 5, TS0 + 10)
        }],
    );
    let crafted = run(bucket.path(), r#"body == "spill TRUNCATED""#).await;
    assert_eq!(
        crafted.rows, 0,
        "the truncated stored param must not satisfy the crafted literal"
    );
    let truth = run(
        bucket.path(),
        r#"body == "spill TRUNCATED-BUT-THE-REAL-LINE-WENT-ON""#,
    )
    .await;
    assert_eq!(truth.rows, 1, "the retained body is the truth");
}

/// The empty-set half of RFC0044.8 — a literal matching no template and
/// no retained body returns empty (the full pruning assertion lands with
/// the partitioned fixtures in a later slice).
#[tokio::test]
async fn rfc0044_8_unmatched_literal_returns_empty() {
    let bucket = tempfile::TempDir::new().expect("temp");
    seed(bucket.path());
    let result = run(bucket.path(), r#"body == "no such body anywhere""#).await;
    assert_eq!(result.rows, 0);
}
