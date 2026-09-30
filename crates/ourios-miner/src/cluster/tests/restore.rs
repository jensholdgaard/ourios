use super::*;

// ---------- §6.9 restore (RFC 0001 v2 amendment) ----------

#[test]
fn restore_round_trips_snapshot_state() {
    let mut original = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");
    // Varied shapes: typed wildcards (NUM, UUID), a widened slot
    // ("in"/"out" → Str wildcard past the prefix path), and a
    // no-wildcard leaf.
    for line in [
        "user 42 logged in",
        "user 17 logged out",
        "GET /home 200",
        "request 550e8400-e29b-41d4-a716-446655440000 accepted",
    ] {
        let _ = original.ingest(&string_record(&t, line));
    }
    for (severity, scope) in [(9, Some("lib.a")), (13, None)] {
        let _ = original.ingest(&structured_record(&t, severity, scope));
    }
    let s1 = original.snapshot_state(&t);

    let mut restored = MinerCluster::new(MinerConfig::default());
    restored
        .restore_tenant(&t, &s1)
        .expect("restore succeeds on a live-produced snapshot");

    assert_eq!(restored.snapshot_state(&t), s1);
    assert_eq!(restored.template_count(&t), original.template_count(&t));
}

#[test]
fn restored_tree_continues_identically() {
    let t = TenantId::new("tenant-x");
    let mut original = MinerCluster::new(MinerConfig::default());
    for line in ["user 42 logged in", "GET /home 200"] {
        let _ = original.ingest(&string_record(&t, line));
    }
    let mut restored = MinerCluster::new(MinerConfig::default());
    restored
        .restore_tenant(&t, &original.snapshot_state(&t))
        .expect("restore succeeds");

    // §3.5.3 equivalence at the miner level: the same follow-up
    // lines must match the same templates AND allocate the same
    // fresh ids in both clusters.
    for line in [
        "user 17 logged in",    // attaches to the restored leaf
        "cache evicted 5 keys", // allocates a fresh id
    ] {
        let rec = string_record(&t, line);
        assert_eq!(
            original.ingest(&rec),
            restored.ingest(&rec),
            "line {line:?}"
        );
    }
    assert_eq!(restored.snapshot_state(&t), original.snapshot_state(&t));
}

#[test]
fn restore_with_wildcard_in_prefix_path() {
    // The first token masks (IPv4) → the leaf carries Wildcard
    // at path position 0; restore must rebuild the descend path
    // from the slot's mask tag.
    let t = TenantId::new("tenant-x");
    let mut original = MinerCluster::new(MinerConfig::default());
    let id = original.ingest(&string_record(&t, "10.0.0.1 connection accepted"));
    let s1 = original.snapshot_state(&t);
    assert!(
        matches!(s1.leaves[0].template[0], TokenRecord::Wildcard),
        "precondition: the leaf must carry a wildcard at path position 0",
    );

    let mut restored = MinerCluster::new(MinerConfig::default());
    restored.restore_tenant(&t, &s1).expect("restore succeeds");
    assert_eq!(restored.snapshot_state(&t), s1);

    // A new matching line attaches to the restored leaf: same
    // id, no new template, version unchanged.
    let id2 = restored.ingest(&string_record(&t, "10.0.0.2 connection accepted"));
    assert_eq!(id2, id);
    assert_eq!(restored.template_count(&t), 1);
    assert_eq!(restored.templates_for(&t)[0].template_version, 1);
}

#[test]
fn restore_rejects_live_tenant() {
    let t = TenantId::new("tenant-x");
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let _ = cluster.ingest(&string_record(&t, "hello world"));
    let snapshot = cluster.snapshot_state(&t);

    let err = cluster
        .restore_tenant(&t, &snapshot)
        .expect_err("restoring over a live tenant must fail");
    assert!(matches!(err, RestoreError::TenantAlreadyLive));
}

#[test]
fn mined_leaf_carries_mined_provenance() {
    // RFC 0050 §3.3: every leaf minted by the Drain walk starts
    // as `{Mined}` with no upstream associations.
    let t = TenantId::new("tenant-x");
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let _ = cluster.ingest(&string_record(&t, "hello world"));

    let leaves = cluster.templates_for(&t);
    assert_eq!(leaves.len(), 1);
    assert_eq!(
        leaves[0].provenance,
        ProvenanceSet::singleton(Provenance::Mined),
    );
    assert!(leaves[0].upstream_associations.is_empty());
    assert_eq!(leaves[0].upstream_association_overflow, 0);
}

#[test]
fn restore_maps_pre_rfc0050_empty_provenance_to_mined() {
    // A snapshot written before RFC 0050 carries no provenance
    // list; restore must read that as `{Mined}` (those leaves
    // were minted by the Drain walk) while restoring the
    // association fields verbatim when present.
    let state = SnapshotState {
        leaves: vec![LeafRecord {
            template: vec![
                TokenRecord::Fixed("disk".to_string()),
                TokenRecord::Fixed("full".to_string()),
            ],
            template_id: 7,
            template_version: 1,
            severity_number: 0,
            scope_name: None,
            slot_types: vec![],
            provenance: vec![],
            upstream_associations: vec!["disk <*>".to_string()],
            upstream_association_overflow: 3,
        }],
        structured_templates: vec![],
        wal_high_water: None,
        adopted_templates: vec![],
    };
    let t = TenantId::new("tenant-x");
    let mut cluster = MinerCluster::new(MinerConfig::default());
    cluster
        .restore_tenant(&t, &state)
        .expect("pre-RFC0050 snapshot restores");

    let leaves = cluster.templates_for(&t);
    assert_eq!(leaves.len(), 1);
    assert_eq!(
        leaves[0].provenance,
        ProvenanceSet::singleton(Provenance::Mined),
    );
    assert_eq!(
        leaves[0].upstream_associations,
        vec!["disk <*>".to_string()]
    );
    assert_eq!(leaves[0].upstream_association_overflow, 3);

    // And a re-snapshot now records the provenance explicitly —
    // the migration happens once, on read.
    let resnap = cluster.snapshot_state(&t);
    assert_eq!(
        resnap.leaves[0].provenance,
        vec![crate::snapshot::ProvenanceRecord::Mined],
    );
}

#[test]
fn restore_rejects_inconsistent_slot() {
    // A path-position wildcard can only arise from mask
    // emission, so its recorded slot set must be a singleton
    // mask-emitted type; `[Str]` at position 0 cannot come
    // from a live tree.
    let state = SnapshotState {
        leaves: vec![LeafRecord {
            template: vec![
                TokenRecord::Wildcard,
                TokenRecord::Fixed("connection".to_string()),
                TokenRecord::Fixed("accepted".to_string()),
            ],
            template_id: 1,
            template_version: 1,
            severity_number: 0,
            scope_name: None,
            slot_types: vec![vec![ParamTypeRecord::Str]],
            provenance: vec![],
            upstream_associations: vec![],
            upstream_association_overflow: 0,
        }],
        structured_templates: vec![],
        wal_high_water: None,
        adopted_templates: vec![],
    };
    let mut cluster = MinerCluster::new(MinerConfig::default());

    let err = cluster
        .restore_tenant(&TenantId::new("tenant-x"), &state)
        .expect_err("a Str slot at a path position must be inconsistent");
    assert!(matches!(err, RestoreError::Inconsistent { .. }));
}

#[test]
fn restore_rejects_duplicate_template_id() {
    // Ids are unique cluster-wide; the same id on a leaf and a
    // structured template could not come from a live tree.
    let state = SnapshotState {
        leaves: vec![LeafRecord {
            template: vec![
                TokenRecord::Fixed("disk".to_string()),
                TokenRecord::Fixed("full".to_string()),
            ],
            template_id: 7,
            template_version: 1,
            severity_number: 0,
            scope_name: None,
            slot_types: vec![],
            provenance: vec![],
            upstream_associations: vec![],
            upstream_association_overflow: 0,
        }],
        structured_templates: vec![StructuredTemplateRecord {
            severity_number: 9,
            scope_name: None,
            event_name: None,
            template_id: 7,
        }],
        wal_high_water: None,
        adopted_templates: vec![],
    };
    let mut cluster = MinerCluster::new(MinerConfig::default());

    let err = cluster
        .restore_tenant(&TenantId::new("tenant-x"), &state)
        .expect_err("a duplicate template_id must be inconsistent");
    match err {
        RestoreError::Inconsistent { detail } => {
            assert!(detail.contains('7'), "detail names the id, got {detail:?}");
        }
        other => panic!("expected Inconsistent, got {other:?}"),
    }
}

#[test]
fn restore_rejects_duplicate_structured_key() {
    // The structured map keys on (severity, scope, event_name); a
    // duplicate key would silently drop one entry while
    // template_count counted both. Both records share the same
    // (9, "lib.a", None) key, so restore must reject them.
    let state = SnapshotState {
        leaves: vec![],
        structured_templates: vec![
            StructuredTemplateRecord {
                severity_number: 9,
                scope_name: Some("lib.a".to_string()),
                event_name: None,
                template_id: 1,
            },
            StructuredTemplateRecord {
                severity_number: 9,
                scope_name: Some("lib.a".to_string()),
                event_name: None,
                template_id: 2,
            },
        ],
        wal_high_water: None,
        adopted_templates: vec![],
    };
    let mut cluster = MinerCluster::new(MinerConfig::default());

    let err = cluster
        .restore_tenant(&TenantId::new("tenant-x"), &state)
        .expect_err("a duplicate structured key must be inconsistent");
    match err {
        RestoreError::Inconsistent { detail } => {
            assert!(
                detail.contains('9') && detail.contains("lib.a"),
                "detail names the key, got {detail:?}",
            );
        }
        other => panic!("expected Inconsistent, got {other:?}"),
    }
}

#[test]
fn restore_bumps_the_id_allocator() {
    // The allocator is cluster-wide; a restored id must never
    // be re-minted for a new template.
    let state = SnapshotState {
        leaves: vec![LeafRecord {
            template: vec![
                TokenRecord::Fixed("disk".to_string()),
                TokenRecord::Fixed("usage".to_string()),
                TokenRecord::Fixed("high".to_string()),
            ],
            template_id: 7,
            template_version: 1,
            severity_number: 0,
            scope_name: None,
            slot_types: vec![],
            provenance: vec![],
            upstream_associations: vec![],
            upstream_association_overflow: 0,
        }],
        structured_templates: vec![],
        wal_high_water: None,
        adopted_templates: vec![],
    };
    let t = TenantId::new("tenant-x");
    let mut cluster = MinerCluster::new(MinerConfig::default());
    cluster
        .restore_tenant(&t, &state)
        .expect("restore succeeds");

    let new_id = cluster.ingest(&string_record(&t, "cache evicted 5 keys"));
    assert!(
        new_id >= 8,
        "new template must not collide with restored id 7, got {new_id}",
    );
}

fn high_water(byte: u64) -> crate::snapshot::WalHighWater {
    crate::snapshot::WalHighWater {
        segment: "0190b3c8-1a2b-7c3d-9e4f-50607080a0b0".to_string(),
        byte,
    }
}

/// RFC 0052 §3.1: each tenant carries its own folded horizon, and
/// recording one tenant's frame moves no other tenant's.
#[test]
fn fold_through_moves_only_the_folding_tenants_horizon() {
    let (busy, idle) = (TenantId::new("busy"), TenantId::new("idle"));
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let _ = cluster.ingest(&string_record(&idle, "user 1 logged in"));
    cluster.fold_through(&idle, high_water(10));

    let _ = cluster.ingest(&string_record(&busy, "user 2 logged in"));
    cluster.fold_through(&busy, high_water(20));

    assert_eq!(cluster.folded_horizon(&idle), Some(&high_water(10)));
    assert_eq!(cluster.folded_horizon(&busy), Some(&high_water(20)));
    assert_eq!(
        cluster.snapshot_state(&busy).wal_high_water,
        None,
        "the snapshot writer stamps the horizon, not the cluster",
    );
}

/// A restored tenant keeps its snapshot's horizon until a later
/// frame folds, so a restart with nothing to replay does not lose it.
#[test]
fn restore_carries_the_snapshot_horizon_as_the_folded_horizon() {
    let t = TenantId::new("tenant-x");
    let mut original = MinerCluster::new(MinerConfig::default());
    let _ = original.ingest(&string_record(&t, "user 42 logged in"));
    let mut state = original.snapshot_state(&t);
    state.wal_high_water = Some(high_water(7));

    let mut restored = MinerCluster::new(MinerConfig::default());
    restored.restore_tenant(&t, &state).expect("restore");

    assert_eq!(restored.folded_horizon(&t), Some(&high_water(7)));
}

/// A tenant whose records carried no body allocates no templates, but
/// its frames were folded all the same: without state it would have no
/// snapshot, and its frames would pin reclamation for good.
#[test]
fn fold_through_allocates_a_tenant_whose_records_minted_nothing() {
    let t = TenantId::new("bodyless");
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let _ = cluster.ingest(&OtlpLogRecord {
        tenant_id: t.clone(),
        body: None,
        ..Default::default()
    });
    assert!(cluster.tenant_ids().is_empty(), "fixture: no state yet");

    cluster.fold_through(&t, high_water(3));

    assert_eq!(cluster.tenant_ids(), vec![t.clone()]);
    assert_eq!(cluster.folded_horizon(&t), Some(&high_water(3)));
    assert_eq!(cluster.template_count(&t), 0);
}
