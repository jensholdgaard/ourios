//! RFC 0001 §5.1 — Hazards. Acceptance criteria stubs for H1, H2,
//! H5, H7. Each `#[test]` carries the scenario id in its doc
//! comment so `grep -R "H1.1" .` resolves bidirectionally between
//! the RFC and the tests (`docs/verification.md` §2.3).
//!
//! Stubs are tagged `#[ignore]` so the default `cargo test`
//! invocation skips them (outer loop / CI stays green). The Red
//! signal lives at the inner loop: an implementor working on a
//! stub runs `cargo test <name> -- --ignored` and watches the
//! `todo!()` panic. See `docs/verification.md` §3.

/// Scenario H1.1 — Semantically distinct templates do not silently merge.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h1_1_login_and_logout_remain_distinct_at_default_threshold() {
    todo!("RFC 0001 §6.4");
}

/// Scenario H1.2 — Lossy-zone match retains body.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h1_2_lossy_zone_match_retains_body() {
    todo!("RFC 0001 §6.6");
}

/// Scenario H1.3 — Every widening emits an audit event.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h1_3_every_widening_emits_an_audit_event() {
    todo!("RFC 0001 §6.4");
}

/// Scenario H2.1 — Oversized parameter triggers OVERFLOW marker and forced body retention.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h2_1_oversized_parameter_triggers_overflow_marker() {
    todo!("RFC 0001 §6.5");
}

/// Scenario H2.2 — Per-service overflow rate above 1% raises an alert.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h2_2_per_service_overflow_rate_above_one_percent_alerts() {
    todo!("RFC 0001 §6.5");
}

/// Scenario H5.1 — Wildcard widening increments `template_version` and emits `template_widened`.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h5_1_wildcard_widening_increments_version_and_emits_template_widened() {
    todo!("RFC 0001 §6.4");
}

/// Scenario H5.2 — Type expansion increments `template_version` and emits `template_type_expanded`.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h5_2_type_expansion_increments_version_and_emits_template_type_expanded() {
    todo!("RFC 0001 §6.4");
}

/// Scenario H5.3 — Drift query returns templates that gained a version in window.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h5_3_drift_query_returns_templates_that_gained_a_version() {
    todo!("RFC 0001 §6.7");
}

/// Scenario H7.1 — Reconstruction property holds across the corpus.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h7_1_reconstruction_property_holds_across_corpus() {
    todo!("RFC 0001 §6.6");
}

/// Scenario H7.2 — Tokenizer failure sets `lossy_flag = true` and retains body.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h7_2_tokenizer_failure_sets_lossy_flag_and_retains_body() {
    todo!("RFC 0001 §6.6");
}

/// Scenario H7.3 — Reader emits body verbatim when `lossy_flag` is true.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h7_3_reader_emits_body_verbatim_when_lossy_flag_is_true() {
    todo!("RFC 0001 §6.6");
}

/// Scenario H7.4 — Widened literal slot reconstructs via STR fallback.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn h7_4_widened_literal_slot_reconstructs_via_str_fallback() {
    todo!("RFC 0001 §6.2");
}
