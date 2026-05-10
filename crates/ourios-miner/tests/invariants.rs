//! RFC 0001 §5.2 — Invariants. Acceptance criteria stubs for
//! CLAUDE.md §3.1, §3.2, §3.3, §3.5, §3.7. Each `#[test]` carries
//! the scenario id in its doc comment so `grep -R "§3.1.1" .`
//! resolves bidirectionally between the RFC and the tests
//! (`docs/verification.md` §2.3).
//!
//! Stubs are tagged `#[ignore]` so the default `cargo test`
//! invocation skips them (outer loop / CI stays green). The Red
//! signal lives at the inner loop: an implementor working on a
//! stub runs `cargo test <name> -- --ignored` and watches the
//! `todo!()` panic. See `docs/verification.md` §3.

/// Scenario §3.1.1 — Default similarity threshold is 0.7.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn invariant_3_1_1_default_threshold_is_0_7() {
    use ourios_core::config::MinerConfig;

    // Arrange — no override; tenant config is left at defaults.

    // Act
    let cfg = MinerConfig::default();

    // Assert
    assert!(
        (cfg.similarity_threshold - 0.7_f32).abs() < f32::EPSILON,
        "default similarity_threshold must be 0.7, got {}",
        cfg.similarity_threshold,
    );
}

/// Scenario §3.1.2 — Mandatory metric set is exposed.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn invariant_3_1_2_mandatory_metric_set_is_exposed() {
    todo!("RFC 0001 §6.8");
}

/// Scenario §3.2.1 — Default per-parameter byte limit is 256.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn invariant_3_2_1_default_param_byte_limit_is_256() {
    use ourios_core::config::MinerConfig;

    // Arrange — no override; tenant config is left at defaults.

    // Act
    let cfg = MinerConfig::default();

    // Assert
    assert_eq!(cfg.param_byte_limit, 256);
}

/// Scenario §3.2.2 — Configured limit above 1 KiB is rejected at startup.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn invariant_3_2_2_param_limit_above_1_kib_rejected_at_startup() {
    use ourios_core::config::{MinerConfig, MinerConfigError};

    // Arrange — explicit attempt to set the limit one byte above
    // the §3.2 ceiling (1024 B).
    let over_ceiling: u32 = 1025;

    // Act
    let r = MinerConfig::try_new(0.7, over_ceiling);

    // Assert
    assert_eq!(
        r,
        Err(MinerConfigError::ParamByteLimitTooLarge(over_ceiling)),
    );
    // §3.2.2's "the process refuses to start serving that tenant"
    // is the consequence of try_new returning Err; refusal is the
    // call site's responsibility (future ingester PR), not a
    // property of MinerConfig itself.
}

/// Scenario §3.3.1 — Separators array captured on every successful tokenization.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn invariant_3_3_1_separators_captured_on_every_tokenization() {
    todo!("RFC 0001 §6.6");
}

/// Scenario §3.5.1 — Snapshot format carries a leading version byte.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn invariant_3_5_1_snapshot_format_carries_leading_version_byte() {
    todo!("RFC 0001 §6.9");
}

/// Scenario §3.5.2 — Unknown snapshot version triggers full WAL replay.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn invariant_3_5_2_unknown_snapshot_version_triggers_wal_replay() {
    todo!("RFC 0001 §6.9");
}

/// Scenario §3.7.1 — Tenants' template trees never cross-pollinate.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn invariant_3_7_1_tenant_trees_never_cross_pollinate() {
    todo!("RFC 0001 §6.1, §6.2");
}

/// Scenario §3.7.2 — Same structural template in two tenants gets distinct `template_id`s.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
#[ignore = "RFC 0001 Red gate — implementation pending"]
fn invariant_3_7_2_same_template_two_tenants_distinct_template_ids() {
    todo!("RFC 0001 §6.1");
}
