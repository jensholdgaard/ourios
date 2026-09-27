use super::*;

// ---------- existing String-body behaviour preserved ----------

#[test]
fn ingest_returns_same_template_id_for_repeat_shape() {
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");

    let id1 = cluster.ingest(&string_record(&t, "user 42 logged in"));
    let id2 = cluster.ingest(&string_record(&t, "user 17 logged in"));

    // Both lines mask to "user <NUM> logged in" → exact
    // sim_seq match on the existing leaf, no widening, no
    // audit, same template_id.
    assert_eq!(id1, id2);
    assert_eq!(cluster.template_count(&t), 1);
    assert_eq!(cluster.merges_total(), 0);
}

#[test]
fn ingest_returns_distinct_template_ids_for_distinct_shapes() {
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");

    let id1 = cluster.ingest(&string_record(&t, "user 42 logged in"));
    let id2 = cluster.ingest(&string_record(&t, "GET /home 200"));

    // Distinct masked shapes land in different `(length,
    // prefix)` buckets — no candidate selection happens at
    // all, both create fresh leaves.
    assert_ne!(id1, id2);
    assert_eq!(cluster.template_count(&t), 2);
    assert_eq!(cluster.merges_total(), 0);
}

#[test]
fn snapshot_state_orders_records_by_template_id() {
    // The tree and the structured-template map both iterate in
    // `HashMap` order; `snapshot_state` sorts by the cluster-unique
    // `template_id` so the serialized snapshot is byte-deterministic.
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");
    for line in [
        "GET /home 200",
        "user 42 logged in",
        "cache evicted 5 keys",
        "disk usage high",
    ] {
        let _ = cluster.ingest(&string_record(&t, line));
    }
    // Distinct (severity, scope) keys populate the structured-template
    // map so its ordering is exercised too.
    for (severity, scope) in [(9, Some("lib.a")), (5, Some("lib.b")), (13, None)] {
        let _ = cluster.ingest(&structured_record(&t, severity, scope));
    }

    let state = cluster.snapshot_state(&t);

    assert!(state.leaves.len() >= 2, "needs multiple leaves to order");
    assert!(
        state
            .leaves
            .windows(2)
            .all(|w| w[0].template_id <= w[1].template_id),
        "snapshot leaves must be sorted by template_id, got {:?}",
        state
            .leaves
            .iter()
            .map(|l| l.template_id)
            .collect::<Vec<_>>(),
    );
    assert!(
        state.structured_templates.len() >= 2,
        "needs multiple structured templates to order",
    );
    assert!(
        state
            .structured_templates
            .windows(2)
            .all(|w| w[0].template_id <= w[1].template_id),
        "structured templates must be sorted by template_id, got {:?}",
        state
            .structured_templates
            .iter()
            .map(|s| s.template_id)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn tenant_ids_returns_sorted_tenants() {
    let mut cluster = MinerCluster::new(MinerConfig::default());
    for name in ["tenant-b", "tenant-a", "tenant-c"] {
        let _ = cluster.ingest(&string_record(&TenantId::new(name), "hello world"));
    }

    assert_eq!(
        cluster.tenant_ids(),
        vec![
            TenantId::new("tenant-a"),
            TenantId::new("tenant-b"),
            TenantId::new("tenant-c"),
        ],
    );
}

#[test]
fn template_count_is_zero_for_unseen_tenant() {
    let cluster = MinerCluster::new(MinerConfig::default());
    let unseen = TenantId::new("never-ingested");

    assert_eq!(cluster.template_count(&unseen), 0);
    assert!(cluster.templates_for(&unseen).is_empty());
}

#[test]
fn ingest_lazily_allocates_per_tenant_state() {
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");
    assert_eq!(cluster.template_count(&t), 0);

    let _ = cluster.ingest(&string_record(&t, "hello world"));

    assert_eq!(cluster.template_count(&t), 1);
}
