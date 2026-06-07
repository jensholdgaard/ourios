//! RFC 0001 §5.3 — RFC-internal design commitments. Acceptance
//! criteria stubs for `RFC0001.x`. Each `#[test]` carries the
//! scenario id in its doc comment so `grep -R "RFC0001.1" .`
//! resolves bidirectionally between the RFC and the tests
//! (`docs/verification.md` §2.3).
//!
//! Stubs are tagged `#[ignore]` so the default `cargo test`
//! invocation skips them (outer loop / CI stays green). The Red
//! signal lives at the inner loop: an implementor working on a
//! stub runs `cargo test <name> -- --ignored` and watches the
//! `todo!()` panic. See `docs/verification.md` §3.

/// Scenario RFC0001.1 — Fresh-leaf creation does not emit an audit event.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn rfc0001_1_fresh_leaf_creation_does_not_emit_audit_event() {
    use ourios_core::audit::SharedAuditSink;
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — ingest two structurally distinct lines, both
    // creating fresh leaves. Per §6.2 step 4, fresh-leaf
    // creation does not emit an audit event; the audit stream
    // is reserved for widening events.
    let sink = SharedAuditSink::new();
    let mut cluster = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(sink.clone()));
    let t = TenantId::new("tenant-x");
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    // Act
    let _ = cluster.ingest(&make("user 42 logged in"));
    let _ = cluster.ingest(&make("GET /home 200"));

    // Assert — both lines created fresh leaves; the sink
    // remains empty.
    assert_eq!(cluster.template_count(&t), 2);
    assert_eq!(cluster.merges_total(), 0);
    assert!(
        sink.is_empty(),
        "fresh-leaf creation must not emit audit events",
    );
}

/// Scenario RFC0001.2 — Degenerate-template guard rejects fully-wildcard widening.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn rfc0001_2_degenerate_template_guard_rejects_fully_wildcard_widening() {
    use ourios_core::audit::{AuditPayload, SharedAuditSink, TemplateChange};
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::{MinerCluster, NO_TEMPLATE};

    // Arrange — force a degenerate-widening attempt.
    //
    // The default prefix-tree shape (`prefix_depth = 2`) makes
    // this scenario structurally unreachable: the first two
    // tokens of any line are baked into the descend path, so
    // any leaf reachable as a widening target has those two
    // positions Fixed by construction. To test the §6.4 guard
    // we drop `prefix_depth` to 0 (all length-N lines share one
    // leaf list) so widenings can reach every position. The
    // tunable is exposed precisely for this kind of guard
    // exercise.
    //
    // Threshold of 0.3 so a 1/3-similar attach triggers widening
    // rather than fresh-leaf creation.
    //   L1 = ["alpha", "beta", "gamma"]  — v=1, all-Fixed.
    //   L2 = ["alpha", "xxx", "yyy"]     — sim 1/3 → widens
    //                                       positions 1, 2 →
    //                                       template
    //                                       ["alpha", <*>, <*>],
    //                                       v=2. NOT degenerate.
    //   L3 = ["zzz", "qqq", "rrr"]       — sim 2/3 (two wildcard
    //                                       matches) → would
    //                                       widen position 0 →
    //                                       fully degenerate →
    //                                       rejected.
    let config = MinerConfig::try_new(0.3, 256).expect("valid config");
    let sink = SharedAuditSink::new();
    let mut cluster =
        MinerCluster::with_audit_sink(config, Box::new(sink.clone())).with_prefix_depth(0);
    let t = TenantId::new("tenant-x");
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    // Act
    let l1 = cluster.ingest(&make("alpha beta gamma"));
    let _l2 = cluster.ingest(&make("alpha xxx yyy"));
    let l3 = cluster.ingest(&make("zzz qqq rrr"));

    // Assert — L3 was rejected (NO_TEMPLATE), the rejection
    // was audited but did not increment `merges_total`, and
    // `parse_failures_total` ticked.
    assert_ne!(l1, NO_TEMPLATE, "L1 created the leaf");
    assert_eq!(
        l3, NO_TEMPLATE,
        "L3's widening would be fully wildcard → rejected"
    );
    assert_eq!(
        cluster.merges_total(),
        1,
        "only L2's (non-degenerate) widening counts toward merges_total",
    );
    assert_eq!(
        cluster.parse_failures_total(),
        1,
        "L3's rejection is a parse failure",
    );

    let events = sink.drain();
    assert_eq!(events.len(), 2);
    assert!(
        matches!(
            events[0].payload,
            AuditPayload::Template {
                change: TemplateChange::Widened { .. },
                ..
            }
        ),
        "event 0: expected Template/Widened, got {:?}",
        events[0].payload,
    );
    // Rejection audit's `would_be_template` records what the
    // widening *would* have produced, so an operator inspecting
    // the event sees the degenerate shape that was avoided.
    let AuditPayload::Template {
        change: TemplateChange::RejectedDegenerate {
            would_be_template, ..
        },
        ..
    } = &events[1].payload
    else {
        panic!(
            "event 1: expected Template/RejectedDegenerate, got {:?}",
            events[1].payload,
        );
    };
    assert_eq!(would_be_template, "<*> <*> <*>");
}

/// Scenario RFC0001.3 — Tokenizer is Unicode whitespace only; punctuation stays in tokens.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn rfc0001_3_tokenizer_is_unicode_whitespace_only() {
    use ourios_miner::tokenize::tokenize;

    let r = tokenize("key=value, other=42").expect("nul-free test input");
    assert_eq!(r.tokens, vec!["key=value,", "other=42"]);
    assert_eq!(
        r.separators.len(),
        r.tokens.len() + 1,
        "separators contract: len == tokens.len() + 1",
    );

    // Each listed punctuation must stay inside a single token AND
    // the token must equal the input verbatim — asserting only
    // tokens.len() == 1 would miss a buggy tokenizer that *drops*
    // the punctuation char (PR #8 review, comment 3178317220).
    for line in ["a=b", "a:b", "a,b", "a;b", "a[b", "a]b", "a(b", "a)b"] {
        let r = tokenize(line).expect("nul-free test input");
        assert_eq!(
            r.tokens.len(),
            1,
            "punctuation in {line:?} introduced a token boundary",
        );
        assert_eq!(
            r.tokens[0], line,
            "punctuation char dropped or altered in {line:?}",
        );
    }

    let r = tokenize("hello world").expect("nul-free test input");
    assert_eq!(r.tokens, vec!["hello", "world"]);

    let r = tokenize("hello\u{00A0}world").expect("nul-free test input");
    assert_eq!(
        r.tokens,
        vec!["hello", "world"],
        "U+00A0 (non-breaking space) is Unicode whitespace and must split",
    );
}

/// Regression for RFC0001.3 — the full ASCII whitespace set (every
/// byte `char::is_whitespace` recognises) must split tokens. The
/// scenario only names "Unicode whitespace"; this test locks in the
/// VT (U+000B) and FF (U+000C) cases that the doc comment originally
/// omitted (PR #8 review, comment 3178317199).
#[test]
fn rfc0001_3_regression_vt_and_ff_split_tokens() {
    use ourios_miner::tokenize::tokenize;

    let r = tokenize("hello\u{000B}world").expect("nul-free test input");
    assert_eq!(
        r.tokens,
        vec!["hello", "world"],
        "U+000B (vertical tab) must split",
    );

    let r = tokenize("hello\u{000C}world").expect("nul-free test input");
    assert_eq!(
        r.tokens,
        vec!["hello", "world"],
        "U+000C (form feed) must split",
    );
}

/// Regression for `Tokenized<'a>` — every separator must be a
/// borrowed slice of the input, including the empty trailing
/// separator after a line ending in a token. This locks in the
/// "borrows from the input" guarantee against an implementation
/// that pushes a literal `""` (which is `&'static str`) and would
/// silently violate the lifetime story (PR #8 review, comment
/// 3178317213).
#[test]
fn rfc0001_3_regression_separators_always_borrow_from_input() {
    use ourios_miner::tokenize::tokenize;

    for line in ["", "hello", "  hello  world  ", "hello\nworld\n"] {
        let r = tokenize(line).expect("nul-free test input");
        let bounds = line.as_bytes().as_ptr_range();
        for (idx, sep) in r.separators.iter().enumerate() {
            let sep_ptr = sep.as_ptr();
            assert!(
                sep_ptr >= bounds.start && sep_ptr <= bounds.end,
                "separator[{idx}] = {sep:?} in line {line:?} \
                 does not borrow from the input ({sep_ptr:p} \
                 outside [{:p}, {:p}])",
                bounds.start,
                bounds.end,
            );
        }
    }
}

/// Scenario RFC0001.4 — Confidence ratio = simSeq / threshold; decision boundary at 1.0.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn rfc0001_4_confidence_ratio_decision_boundary_at_one() {
    use ourios_core::confidence::ConfidenceZone;
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;
    use ourios_miner::sim_seq::confidence_ratio;

    // The math: confidence = simSeq / threshold.
    assert!(
        (confidence_ratio(0.7, 0.7) - 1.0).abs() < f32::EPSILON,
        "simSeq == threshold → confidence == 1.0",
    );
    assert!(
        (confidence_ratio(0.7, 0.5) - 1.4).abs() < f32::EPSILON,
        "same simSeq under a lower threshold reframes confidence \
         scale-invariantly (sim 0.7 / thresh 0.5 = 1.4)",
    );

    // The decision boundary at confidence == 1.0 maps to the
    // Clean zone (inclusive); confidence < 1.0 (i.e.
    // simSeq < threshold) drops to Lossy or below.
    assert_eq!(
        ConfidenceZone::classify(0.7, 0.7, 0.5),
        ConfidenceZone::Clean,
        "sim == threshold (confidence == 1.0) is on the clean-attach side",
    );
    assert_eq!(
        ConfidenceZone::classify(0.6999, 0.7, 0.5),
        ConfidenceZone::Lossy,
        "sim just below threshold (confidence < 1.0) is lossy",
    );

    // Behavioural check: a line whose sim_seq against the
    // candidate is exactly the threshold takes the clean-attach
    // branch (widening if positions differ; body NOT retained).
    //
    // Construction: two 10-token lines sharing the first 7
    // post-mask tokens. Single-letter tokens dodge every mask
    // rule, so masked == raw and sim_seq = 7/10 = 0.7 exactly.
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };
    cluster.ingest(&make("a b c d e f g h i j"));
    cluster.ingest(&make("a b c d e f g x y z"));

    assert_eq!(
        cluster.template_count(&t),
        1,
        "sim == threshold is clean → reuses the existing leaf",
    );
    assert_eq!(
        cluster.merges_total(),
        1,
        "clean attach widens the 3 mismatched positions → one merge event",
    );
    assert_eq!(
        cluster.body_retentions_total(),
        0,
        "clean zone does not retain body (ConfidenceZone::Clean.retains_body() == false)",
    );
    assert_eq!(cluster.parse_failures_total(), 0);
}

/// Scenario RFC0001.5 — Bare `template_id = X` spans all versions of leaf X.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn rfc0001_5_bare_template_id_spans_all_versions_of_leaf() {
    todo!("RFC 0001 §6.7");
}

/// Scenario RFC0001.6 — Bare `template_id = X` does NOT follow alias chains.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn rfc0001_6_bare_template_id_does_not_follow_alias_chains() {
    todo!("RFC 0001 §6.7");
}

/// Scenario RFC0001.7 — Combined widening + type-expansion increments version twice and emits two events in order.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn rfc0001_7_combined_widening_and_type_expansion_emits_two_events_in_order() {
    use ourios_core::audit::{AuditPayload, ParamType, SharedAuditSink, TemplateChange};
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — a leaf with one pre-existing wildcard slot whose
    // slot_types = {Str} (seeded via a literal widening at
    // position 4). The third line then triggers a Fixed mismatch
    // at position 3 (literal "hour" → "minute" → widening) AND
    // brings a `<NUM>` token at position 4 (the existing
    // wildcard, whose slot_types[0] = {Str}, doesn't contain
    // Num → type expansion).
    //
    // Per RFC §6.2 the third line's attach must emit
    // `TemplateWidened` first, then `TemplateTypeExpanded`,
    // bumping `template_version` twice.
    let sink = SharedAuditSink::new();
    let mut cluster = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(sink.clone()));
    let t = TenantId::new("tenant-x");
    let make = |text: &str| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    // L1: fresh 5-token leaf, v=1. Shared prefix ["user","logged"].
    let _ = cluster.ingest(&make("user logged at hour NOW"));
    // L2: literal widening at position 4 ("NOW" → "LATER").
    //     sim_seq = 4/5 ≥ default threshold 0.7. v=2, slot_types
    //     = [{Str}].
    let _ = cluster.ingest(&make("user logged at hour LATER"));
    // Discard the L2 event so we can assert exactly the L3 trail.
    let _ = sink.drain();

    // Act — L3: Fixed mismatch at position 3 ("hour" → "minute")
    // AND `<NUM>` (from "13") at position 4's pre-existing
    // wildcard. sim_seq = 4/5 → Clean. Combined attach.
    let _ = cluster.ingest(&make("user logged at minute 13"));

    // Assert — exactly two events in widening-then-expansion
    // order, with the version pair carrying the two-bump trail
    // (2 → 3 from widening, 3 → 4 from expansion).
    let events = sink.drain();
    assert_eq!(
        events.len(),
        2,
        "combined attach must emit exactly two audit events",
    );

    let AuditPayload::Template {
        change:
            TemplateChange::Widened {
                old_version: w_old,
                new_version: w_new,
                positions_widened,
                ..
            },
        ..
    } = &events[0].payload
    else {
        panic!(
            "event 0 must be Template/Widened (widening fires before expansion), got {:?}",
            events[0].payload,
        );
    };
    assert_eq!((*w_old, *w_new), (2, 3), "widening bumps version 2 → 3");
    assert_eq!(*positions_widened, vec![3]);

    let AuditPayload::Template {
        change:
            TemplateChange::TypeExpanded {
                old_version: e_old,
                new_version: e_new,
                slots_expanded,
                ..
            },
        ..
    } = &events[1].payload
    else {
        panic!(
            "event 1 must be Template/TypeExpanded, got {:?}",
            events[1].payload,
        );
    };
    assert_eq!(
        (*e_old, *e_new),
        (3, 4),
        "expansion bumps version 3 → 4 in the same attach",
    );
    // The existing wildcard at template position 4 is ordinal 1
    // in the post-widen template (the freshly-widened slot at
    // position 3 takes ordinal 0). Expansion adds Num.
    assert_eq!(slots_expanded.len(), 1);
    assert_eq!(slots_expanded[0].slot_index, 1);
    assert_eq!(slots_expanded[0].added_types, vec![ParamType::Num]);

    // RFC §6.4: both events count toward merges_total via
    // `TemplateChange::counts_as_merge`, so the counter
    // increments by exactly 2 across the combined attach.
    assert_eq!(
        cluster.merges_total(),
        3,
        "merges_total: 1 (L2 widen) + 1 (L3 widen) + 1 (L3 expand)",
    );
}

/// Scenario RFC0001.8 — `confidence_p50` and `confidence_p01` are emitted as gauges.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn rfc0001_8_confidence_p50_and_p01_are_emitted_as_gauges() {
    todo!("RFC 0001 §6.8");
}

/// Scenario RFC0001.9 — `body_kind = Structured` short-circuits to a structured-template id.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn rfc0001_9_structured_body_short_circuits_to_structured_template_id() {
    todo!("RFC 0001 §6.1 (Body representation), §6.2 (step 0)");
}

/// Scenario RFC0001.10 — `time_unix_nano` is preserved verbatim from the wire.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn rfc0001_10_time_unix_nano_preserved_verbatim_from_wire() {
    todo!("RFC 0001 §6.1 (record schema)");
}

/// Scenario RFC0001.11 — `severity_number = 0` and `scope_name = None` are distinct key buckets.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn rfc0001_11_severity_zero_and_scope_none_are_distinct_key_buckets() {
    use std::collections::HashSet;

    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — four records with identical body text, varying only
    // across the full `(severity_number, scope_name)` cross-product
    // of the two edge-case key positions the RFC locks: severity `0`
    // (`UNSPECIFIED`, a valid OTLP value) and scope `None` (absent).
    // Per RFC §6.1, `0` is its own severity bucket and `None` is its
    // own scope bucket — never coalesced with a specified severity
    // or named scope. The four `(severity, scope)` combinations must
    // therefore yield four distinct `template_id`s.
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");
    let make = |severity: u8, scope: Option<&str>| OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String("user logged in".to_string())),
        severity_number: severity,
        scope_name: scope.map(str::to_string),
        ..Default::default()
    };

    // Act — (0, None), (0, Some), (9, None), (9, Some).
    let id_0_none = cluster.ingest(&make(0, None));
    let id_0_some = cluster.ingest(&make(0, Some("lib.x")));
    let id_9_none = cluster.ingest(&make(9, None));
    let id_9_some = cluster.ingest(&make(9, Some("lib.x")));

    // Assert — four distinct ids (one per key bucket), four
    // templates, no widening coalescing any pair of buckets.
    let ids = HashSet::from([id_0_none, id_0_some, id_9_none, id_9_some]);
    assert_eq!(
        ids.len(),
        4,
        "each (severity, scope) bucket — including (0, None) — must get its own template_id; got {ids:?}",
    );
    assert_eq!(cluster.template_count(&t), 4);
    assert_eq!(
        cluster.merges_total(),
        0,
        "the UNSPECIFIED-severity / None-scope buckets must never coalesce with specified ones",
    );
}
