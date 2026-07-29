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

/// Scenario RFC0044.6 — renames and reversions across deploys: the
/// registry folds every `(template_id, version)`'s tokens from the audit
/// stream, so unification finds every id/version under which
/// byte-identical records were written — a re-created (renamed) template
/// and a widened version both contribute. No alias-class expansion is involved: alias
/// classes group *different* shapes, which byte-equality must never
/// cross (§3.3 refinement).
/// See `docs/rfcs/0044-template-aware-body-equality.md` §5.
#[tokio::test]
async fn rfc0044_6_renames_and_reversions_contribute_every_matching_id() {
    let bucket = tempfile::TempDir::new().expect("temp");
    write_audit(
        bucket.path(),
        &[
            created("t", 2, "user <*> logged in from <*>", TS0),
            // The same shape re-created under a new id after a deploy —
            // the RFC 0010 drift scenario.
            created("t", 7, "user <*> logged in from <*>", TS0 + 2),
            // A real widening: v2 gains a trailing wildcard, so the
            // version's tokens differ from v1's and key separately.
            AuditEvent {
                tenant_id: TenantId::new("t"),
                timestamp: at(TS0 + 3),
                payload: AuditPayload::Template {
                    template_id: 2,
                    triggering_line_hash: hash_triggering_line(b"widen"),
                    triggering_line_sample: None,
                    change: TemplateChange::Widened {
                        old_version: 1,
                        new_version: 2,
                        old_template: "user <*> logged in from <*>".to_owned(),
                        new_template: "user <*> logged in from <*> <*>".to_owned(),
                        positions_widened: vec![6],
                    },
                },
            },
        ],
    );
    let sep6 = ["", " ", " ", " ", " ", " ", ""];
    let sep7 = ["", " ", " ", " ", " ", " ", " ", ""];
    write_all(
        bucket.path(),
        &[
            mined("t", 2, TS0 + 10, &["999", "1.2.3.4"], &sep6),
            mined("t", 7, TS0 + 20, &["999", "1.2.3.4"], &sep6),
            mined("t", 7, TS0 + 30, &["888", "1.2.3.4"], &sep6),
            // Written under the widened (2, v2).
            MinedRecord {
                template_version: 2,
                ..mined("t", 2, TS0 + 40, &["999", "1.2.3.4", "EXTRA"], &sep7)
            },
        ],
    );
    let renamed = run(
        bucket.path(),
        r#"body == "user 999 logged in from 1.2.3.4""#,
    )
    .await;
    assert_eq!(
        renamed.rows, 2,
        "both ids' byte-identical records return; the differing and v2 records do not"
    );
    let widened = run(
        bucket.path(),
        r#"body == "user 999 logged in from 1.2.3.4 EXTRA""#,
    )
    .await;
    assert_eq!(
        widened.rows, 1,
        "the widened version's tokens key separately and match their own render"
    );
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

/// Scenario RFC0044.7 — pruning still engages: two hour-partitioned
/// files, the matched template only in the first. The second file's row
/// group is skippable by both arms (its `template_id` statistics exclude
/// every candidate and its body column is all-NULL), so the scan touches
/// only the matching file — and the result is still complete.
/// See `docs/rfcs/0044-template-aware-body-equality.md` §5.
#[tokio::test]
async fn rfc0044_7_pruning_engages_across_partitions() {
    const HOUR: u64 = 3_600_000_000_000;
    let bucket = tempfile::TempDir::new().expect("temp");
    write_audit(
        bucket.path(),
        &[
            created("t", 1, "claude_code.api_request", TS0),
            created("t", 9, "claude_code.tool_result", TS0 + 1),
        ],
    );
    write_all(
        bucket.path(),
        &[
            mined("t", 1, TS0 + 10, &[], &["", ""]),
            // A different template a partition-hour later: its own file.
            mined("t", 9, TS0 + HOUR + 10, &[], &["", ""]),
        ],
    );
    let result = run(bucket.path(), r#"body == "claude_code.api_request""#).await;
    assert_eq!(result.rows, 1, "the matching record returns, completely");
    assert!(
        result.stats.row_groups_pruned >= 1,
        "the non-candidate partition is pruned: {:?}",
        result.stats,
    );
}

/// Scenario RFC0044.8 (full) — a literal matching no template and no
/// retained body returns empty with every row group pruned: correct
/// empties stay cheap.
/// See `docs/rfcs/0044-template-aware-body-equality.md` §5.
#[tokio::test]
async fn rfc0044_8_correct_empties_prune_everything() {
    const HOUR: u64 = 3_600_000_000_000;
    let bucket = tempfile::TempDir::new().expect("temp");
    write_audit(
        bucket.path(),
        &[created("t", 1, "claude_code.api_request", TS0)],
    );
    write_all(
        bucket.path(),
        &[
            mined("t", 1, TS0 + 10, &[], &["", ""]),
            mined("t", 1, TS0 + HOUR + 10, &[], &["", ""]),
        ],
    );
    let result = run(bucket.path(), r#"body == "claude_code.no_such_event""#).await;
    assert_eq!(result.rows, 0);
    assert_eq!(
        result.stats.row_groups_scanned, 0,
        "an unmatched literal must not scan anything: {:?}",
        result.stats,
    );
    assert_eq!(
        result.stats.row_groups_pruned, 2,
        "both partitions' row groups are pruned, not elided: {:?}",
        result.stats,
    );
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

// --- RFC0044.9: the reconstruction invariant, driven through the predicate ---

use ourios_config::MinerConfig;
use ourios_core::audit::SharedAuditSink;
use ourios_core::otlp::{Body as OtlpBody, OtlpLogRecord};
use ourios_miner::cluster::MinerCluster;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig {
        cases: ourios_testgen::proptest_cases(10),
        ..ProptestConfig::default()
    })]

    /// Scenario RFC0044.9 `[property]` — for every line of a mined corpus,
    /// `body == <the original line>` finds its record(s): the real miner
    /// mines, its real audit emissions build the registry, and equality
    /// through templates is exactly as faithful as reconstruction itself.
    /// See `docs/rfcs/0044-template-aware-body-equality.md` §5.
    #[test]
    fn rfc0044_9_every_mined_line_is_findable_by_its_own_body(
        lines in proptest::collection::vec("[a-z]{1,5}( [a-z0-9]{1,7}){0,4}", 1..10),
    ) {
        let tenant = TenantId::new("t");
        let audit = SharedAuditSink::new();
        let mut cluster =
            MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(audit.clone()));
        let mut mined_records = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let record = OtlpLogRecord {
                tenant_id: tenant.clone(),
                time_unix_nano: TS0 + u64::try_from(i).expect("small index") * 1_000,
                severity_number: 9,
                body: Some(OtlpBody::String(line.clone())),
                ..Default::default()
            };
            let (_, captured) = cluster.ingest_mined(&record);
            mined_records.push(captured.expect("string-bodied record captures"));
        }
        let bucket = tempfile::TempDir::new().expect("temp");
        write_all(bucket.path(), &mined_records);
        write_audit(bucket.path(), &audit.drain());

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let mut distinct: Vec<&String> = lines.iter().collect();
        distinct.sort();
        distinct.dedup();
        for line in distinct {
            let expected = lines.iter().filter(|l| *l == line).count();
            let result = runtime.block_on(run(
                bucket.path(),
                &format!(r#"body == "{line}""#),
            ));
            prop_assert_eq!(
                usize::try_from(result.rows).expect("row count fits usize"),
                expected,
                "`body == {:?}` must find exactly its own records",
                line,
            );
        }
    }
}
