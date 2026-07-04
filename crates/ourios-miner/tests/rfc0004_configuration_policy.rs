//! RFC 0004 §5 — Acceptance criteria for the configuration-policy
//! boundary (tunables vs invariants). Each `#[test]` carries the
//! scenario id in its doc comment so `grep -R "RFC0004.1" .`
//! resolves bidirectionally between the RFC and the tests
//! (`docs/verification.md` §2.3).

/// Scenario RFC0004.1 — Every tunable validates at startup.
///
/// Covers the `PrefixDepthTooLarge` validation path the RFC adds;
/// the `ThresholdOutOfRange`, `FloorOutOfRange`, and
/// `ParamByteLimitTooLarge` variants are already pinned by
/// `tests/invariants.rs` and the in-crate config docstring
/// examples.
#[test]
fn rfc0004_1_prefix_depth_above_ceiling_rejected_at_startup() {
    use ourios_core::config::{MinerConfig, MinerConfigError, PREFIX_DEPTH_CEILING};

    // Arrange — one byte above the §6.1 ceiling.
    let over_ceiling: u8 = PREFIX_DEPTH_CEILING + 1;

    // Act
    let r = MinerConfig::default().with_prefix_depth(over_ceiling);

    // Assert — variant equality pins the failure mode and the
    // offending value.
    assert_eq!(r, Err(MinerConfigError::PrefixDepthTooLarge(over_ceiling)),);

    // The rendered message must cite the RFC 0001 §6.1 ceiling
    // so an operator inspecting the startup error sees the
    // source of the bound. Mirror of §3.2.2's
    // `param_limit_above_1_kib` test.
    let rendered = r.unwrap_err().to_string();
    assert!(
        rendered.contains("RFC 0001 §6.1"),
        "error must cite the §6.1 ceiling, got: {rendered}",
    );
    assert!(
        rendered.contains(&PREFIX_DEPTH_CEILING.to_string()),
        "error must cite the ceiling value {PREFIX_DEPTH_CEILING}, got: {rendered}",
    );

    // The ceiling value itself is permitted (boundary inclusive
    // per the docstring's `0..=PREFIX_DEPTH_CEILING` range).
    let r_at_ceiling = MinerConfig::default().with_prefix_depth(PREFIX_DEPTH_CEILING);
    assert!(r_at_ceiling.is_ok(), "ceiling value must be permitted");
}

/// Scenario RFC0004.1 (continued) — `param_byte_limit = 0` is
/// rejected at startup. Without this lower-bound check, every
/// non-empty `Param.value` would trip the §6.5 overflow marker
/// and force body retention on every record (a silent
/// degradation that violates the §3.2.2 "configuration rejected
/// values must refuse to serve the tenant" contract).
#[test]
fn rfc0004_1_param_byte_limit_zero_rejected_at_startup() {
    use ourios_core::config::{MinerConfig, MinerConfigError, PARAM_BYTE_LIMIT_CEILING};

    // Act
    let r = MinerConfig::try_new_full(0.7, 0.4, 0);

    // Assert — variant equality pins the failure mode.
    assert_eq!(r, Err(MinerConfigError::ParamByteLimitZero));

    // The rendered message names RFC 0001 §6.5 (the contract the
    // bound exists to defend) and the inclusive valid range so an
    // operator reading the startup error sees both.
    let rendered = r.unwrap_err().to_string();
    assert!(
        rendered.contains("§6.5"),
        "error must cite RFC 0001 §6.5, got: {rendered}",
    );
    assert!(
        rendered.contains("1..=") && rendered.contains(&PARAM_BYTE_LIMIT_CEILING.to_string()),
        "error must cite the 1..=ceiling range, got: {rendered}",
    );

    // Boundary inclusive: 1 is the smallest permitted value.
    let r_at_one = MinerConfig::try_new_full(0.7, 0.4, 1);
    assert!(r_at_one.is_ok(), "lower boundary (1) must be permitted");
}

/// Scenario RFC0004.2 — Per-tenant override is honoured.
///
/// Two tenants, same line shape, different `similarity_threshold`
/// → different leaf-allocation outcomes. Tenant A on the project
/// default (0.7) drops a 0.6-similarity attach into the Lossy
/// zone and creates a fresh leaf; tenant B on an explicit 0.5
/// override stays in the Clean zone and widens.
#[test]
fn rfc0004_2_per_tenant_override_is_honoured_at_decision_boundary() {
    use ourios_core::config::MinerConfig;
    use ourios_core::otlp::{Body, OtlpLogRecord};
    use ourios_core::tenant::TenantId;
    use ourios_miner::cluster::MinerCluster;

    // Arrange — cluster default = 0.7 threshold; tenant B
    // overridden to 0.5. L1 creates a leaf in each tenant
    // independently. L2 produces sim_seq = 3/5 = 0.6 against L1
    // — straddles the two thresholds.
    let tenant_a = TenantId::new("tenant-a");
    let tenant_b = TenantId::new("tenant-b");
    let lenient = MinerConfig::try_new_full(0.5, 0.4, 256).expect("valid config");
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_tenant_config(tenant_b.clone(), lenient);
    let make = |tenant: &TenantId, text: &str| OtlpLogRecord {
        tenant_id: tenant.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    };

    // Act — L1 in each tenant (fresh leaves), then L2 in each.
    let _ = cluster.ingest(&make(&tenant_a, "user logged at hour NOW"));
    let _ = cluster.ingest(&make(&tenant_b, "user logged at hour NOW"));
    // L2 differs from L1 at positions 3 and 4: sim = 3/5 = 0.6.
    let _ = cluster.ingest(&make(&tenant_a, "user logged at later DAY"));
    let _ = cluster.ingest(&make(&tenant_b, "user logged at later DAY"));

    // Assert — tenant A's L2 sim=0.6 < threshold 0.7 → Lossy
    // zone → fresh leaf, so two templates. Tenant B's L2 sim=0.6
    // ≥ threshold 0.5 → Clean → widens the existing leaf, so
    // one template + a `merges_total` bump.
    assert_eq!(
        cluster.template_count(&tenant_a),
        2,
        "tenant A on default (threshold=0.7) drops L2 into Lossy zone → fresh leaf",
    );
    assert_eq!(
        cluster.template_count(&tenant_b),
        1,
        "tenant B on override (threshold=0.5) keeps L2 in Clean zone → widening",
    );
    assert_eq!(
        cluster.merges_total(),
        1,
        "only tenant B's attach widened — exactly one structural merge",
    );
}

/// Scenario RFC0004.3 — No invariant-breaking field exists on
/// `MinerConfig`.
///
/// Compile-time pin: exhaustive destructure of `MinerConfig`'s
/// fields. Adding a new field forces this test to fail to compile
/// until the test author updates the destructure — at which point
/// the PR reviewer sees the addition and has to classify it on
/// the RFC 0004 boundary (tunable inside an invariant vs.
/// invariant-breaking knob requiring a `meta:` RFC).
///
/// The forbidden field names listed in the assertion message are
/// RFC 0004 §3.3's enumeration of *named* invariant-breaking
/// toggles; the test fails to compile rather than fails at
/// runtime, so the names are a comment to the test author, not a
/// runtime check.
#[test]
fn rfc0004_3_no_invariant_breaking_field_exists_on_miner_config() {
    use ourios_core::config::MinerConfig;

    // Adding a new field forces this destructure pattern to be
    // updated (compile error: missing field `<new_field>`).
    // Forbidden field names per RFC 0004 §3.3:
    //   - allow_widening
    //   - respect_severity
    //   - lossy_mode
    //   - enable_cross_tenant_dedup
    //   - accept_lossy_reconstruction
    //
    // A PR proposing any of those triggers a `meta:` RFC against
    // CLAUDE.md §3, not a runtime toggle.
    let cfg = MinerConfig::default();
    let MinerConfig {
        similarity_threshold: _,
        similarity_floor: _,
        param_byte_limit: _,
        prefix_depth: _,
        // RFC 0023 §3.1 bounds — classified as tunables *inside*
        // the invariants: overflow at any of the three diverts to
        // the §6.3 parse-failure path (body retained, counted),
        // never force-merges (§3.1) and never drops data.
        max_node_children: _,
        max_templates: _,
        max_line_tokens: _,
    } = cfg;
}
