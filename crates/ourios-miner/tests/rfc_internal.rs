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
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn rfc0001_1_fresh_leaf_creation_does_not_emit_audit_event() {
    todo!("RFC 0001 §6.2");
}

/// Scenario RFC0001.2 — Degenerate-template guard rejects fully-wildcard widening.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn rfc0001_2_degenerate_template_guard_rejects_fully_wildcard_widening() {
    todo!("RFC 0001 §6.4");
}

/// Scenario RFC0001.3 — Tokenizer is Unicode whitespace only; punctuation stays in tokens.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn rfc0001_3_tokenizer_is_unicode_whitespace_only() {
    todo!("RFC 0001 §6.2");
}

/// Scenario RFC0001.4 — Confidence ratio = simSeq / threshold; decision boundary at 1.0.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn rfc0001_4_confidence_ratio_decision_boundary_at_one() {
    todo!("RFC 0001 §6.3");
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
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn rfc0001_7_combined_widening_and_type_expansion_emits_two_events_in_order() {
    todo!("RFC 0001 §6.2, §6.4");
}

/// Scenario RFC0001.8 — `confidence_p50` and `confidence_p01` are emitted as gauges.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn rfc0001_8_confidence_p50_and_p01_are_emitted_as_gauges() {
    todo!("RFC 0001 §6.8");
}
