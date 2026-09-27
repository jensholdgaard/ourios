use super::*;

// ---------- three-zone confidence (RFC §6.3) ----------

#[test]
fn lossy_zone_creates_new_leaf_and_bumps_body_retention() {
    // L1 = "alpha beta gamma delta epsilon"  (length 5,
    //                                         prefix "alpha beta").
    // L2 = "alpha beta gamma rho sigma"      → sim with L1 = 3/5 = 0.6
    //                                         → lossy zone
    //                                         (0.4 ≤ 0.6 < 0.7 under
    //                                         the RFC §6.3 defaults).
    //
    // Lossy attach: new leaf in the same parent (not widening),
    // body_retentions_total bumps by one, no audit event, no
    // merges_total bump.
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    let id1 = cluster.ingest(&string_record(&t, "alpha beta gamma delta epsilon"));
    let id2 = cluster.ingest(&string_record(&t, "alpha beta gamma rho sigma"));

    assert_ne!(id1, id2, "lossy attach creates a distinct leaf");
    assert_eq!(cluster.template_count(&t), 2);
    assert_eq!(cluster.body_retentions_total(), 1);
    assert_eq!(cluster.merges_total(), 0);
    assert_eq!(cluster.parse_failures_total(), 0);
    assert!(
        drain_changes(&sink).is_empty(),
        "lossy attach emits no audit event"
    );
}

#[test]
fn parse_failure_zone_returns_no_template_and_bumps_counters() {
    // L1 = "alpha beta gamma delta epsilon zeta"  (length 6).
    // L2 = "alpha beta phi rho sigma omega"        → sim with L1 = 2/6
    //                                              ≈ 0.333 →
    //                                              parse-failure zone
    //                                              (< 0.4 RFC §6.3 floor).
    //
    // Pre-§6.3 PR draft used length-5 lines with sim 0.4 and
    // a 0.5 floor — that boundary collapsed once the floor
    // was corrected to the RFC-pinned 0.4. Lengthening L2 by
    // one token (sim 2/6 instead of 2/5) lands the line
    // unambiguously below the floor without re-introducing a
    // boundary-dependent assertion.
    //
    // Parse failure: no leaf created, NO_TEMPLATE returned,
    // parse_failures_total AND body_retentions_total both
    // bump (RFC §6.3 says parse failure also retains body).
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    let id1 = cluster.ingest(&string_record(&t, "alpha beta gamma delta epsilon zeta"));
    let id2 = cluster.ingest(&string_record(&t, "alpha beta phi rho sigma omega"));

    assert_ne!(id1, NO_TEMPLATE, "L1 created the only leaf");
    assert_eq!(
        id2, NO_TEMPLATE,
        "below-floor similarity → parse failure, not new leaf",
    );
    assert_eq!(
        cluster.template_count(&t),
        1,
        "parse failure must not allocate a leaf",
    );
    assert_eq!(cluster.parse_failures_total(), 1);
    assert_eq!(
        cluster.body_retentions_total(),
        1,
        "RFC §6.3: parse failure retains body too",
    );
    assert_eq!(cluster.merges_total(), 0);
    assert!(
        drain_changes(&sink).is_empty(),
        "parse failure emits no audit event"
    );
}

#[test]
fn clean_attach_does_not_bump_body_retentions() {
    // sim ≥ threshold → ConfidenceZone::Clean →
    // retains_body() == false → counter unchanged.
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");

    // Two structurally identical (post-mask) lines: sim == 1.0,
    // clean attach to the existing leaf.
    let _ = cluster.ingest(&string_record(&t, "user 42 logged in"));
    let _ = cluster.ingest(&string_record(&t, "user 17 logged in"));

    assert_eq!(cluster.body_retentions_total(), 0);
    assert_eq!(cluster.parse_failures_total(), 0);
    assert_eq!(cluster.template_count(&t), 1);
}

#[test]
fn floor_at_threshold_collapses_lossy_zone() {
    // With floor == threshold, the lossy zone is empty. Every
    // below-threshold attach goes straight to parse failure.
    // Pin the corner case so a future config refactor that
    // accidentally re-introduces the lossy zone is caught.
    let config = MinerConfig::try_new_full(0.7, 0.7, 256).expect("valid config");
    let mut cluster = MinerCluster::new(config);
    let t = TenantId::new("tenant-x");

    // L1 establishes the leaf; L2 has sim 3/5 = 0.6 — under
    // the collapsed-zone config this is < floor, so parse
    // failure (not lossy, since lossy zone is empty).
    let _id1 = cluster.ingest(&string_record(&t, "alpha beta gamma delta epsilon"));
    let id2 = cluster.ingest(&string_record(&t, "alpha beta gamma rho sigma"));

    assert_eq!(id2, NO_TEMPLATE);
    assert_eq!(cluster.template_count(&t), 1);
    assert_eq!(cluster.parse_failures_total(), 1);
    assert_eq!(cluster.body_retentions_total(), 1);
}
