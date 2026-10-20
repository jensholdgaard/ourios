//! Scenario RFC0001.21 — snapshots from before the RFC0001.17/.19 masking
//! rules restore, and lines mined after the restore have one of four
//! audited outcomes with no id reused.
//! See `docs/rfcs/0001-template-miner.md` §5 and the §6.2 step 2 rollout.

use super::*;

/// What one ingested line did to a restored tenant.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Attached to an existing leaf with no event or version change.
    Matched(u64),
    /// Attached to an existing leaf through audited widening or type
    /// expansion; each event advanced the version by one.
    Mutated(u64),
    /// Started a new leaf with an id above every restored id.
    NewLeaf(u64),
    /// Rejected by a guard and counted as a parse failure.
    Rejected,
}

/// A literal-token leaf record, as a build without the new rules wrote it.
fn leaf(
    template_id: u64,
    tokens: &[Option<&str>],
    slot_types: Vec<Vec<ParamTypeRecord>>,
) -> LeafRecord {
    LeafRecord {
        template: tokens
            .iter()
            .map(|t| t.map_or(TokenRecord::Wildcard, |s| TokenRecord::Fixed(s.to_string())))
            .collect(),
        template_id,
        template_version: 1,
        severity_number: 0,
        scope_name: None,
        slot_types,
        provenance: vec![crate::snapshot::ProvenanceRecord::Mined],
        upstream_associations: vec![],
        upstream_association_overflow: 0,
        wildcard_routed: vec![],
    }
}

fn state(leaves: Vec<LeafRecord>) -> SnapshotState {
    SnapshotState {
        leaves,
        structured_templates: vec![],
        wal_high_water: None,
        adopted_templates: vec![],
    }
}

/// Restore `state` into a fresh cluster built from `config`, and check the
/// restore kept every leaf's id, template and version.
fn restored(
    config: MinerConfig,
    t: &TenantId,
    state: &SnapshotState,
) -> (MinerCluster, SharedAuditSink) {
    let sink = SharedAuditSink::new();
    let mut cluster = MinerCluster::with_audit_sink(config, Box::new(sink.clone()));
    cluster
        .restore_tenant(t, state)
        .expect("a pre-rules snapshot restores");
    assert_eq!(cluster.snapshot_state(t).leaves, state.leaves);
    (cluster, sink)
}

/// Ingest `line` and classify what it did, asserting the outcome's own
/// invariants: no silent attach, one version step per event, no id reuse.
fn ingest_line(
    cluster: &mut MinerCluster,
    sink: &SharedAuditSink,
    t: &TenantId,
    line: &str,
) -> Outcome {
    let before = cluster.snapshot_state(t);
    let max_restored = before
        .leaves
        .iter()
        .map(|l| l.template_id)
        .max()
        .unwrap_or(0);
    let failures = cluster.parse_failures_total();

    let id = cluster.ingest(&string_record(t, line));

    let events = sink.drain();
    if cluster.parse_failures_total() > failures {
        return Outcome::Rejected;
    }
    let Some(old) = before.leaves.iter().find(|l| l.template_id == id) else {
        assert!(id > max_restored, "new id {id} reuses a restored id");
        assert!(
            events.iter().all(|e| matches!(
                &e.payload,
                AuditPayload::Template {
                    change: TemplateChange::Created { .. },
                    ..
                }
            )),
            "a new leaf emits only template_created: {events:?}",
        );
        return Outcome::NewLeaf(id);
    };
    let now = cluster
        .templates_for(t)
        .into_iter()
        .find(|l| l.template_id == id)
        .expect("the attached leaf");
    if events.is_empty() {
        assert_eq!(
            now.template_version, old.template_version,
            "a match keeps the version"
        );
        return Outcome::Matched(id);
    }
    assert!(
        events.iter().all(|e| matches!(
            &e.payload,
            AuditPayload::Template {
                template_id,
                change: TemplateChange::Widened { .. } | TemplateChange::TypeExpanded { .. },
                ..
            } if *template_id == id
        )),
        "a mutation emits only widened / type-expanded events for its leaf: {events:?}",
    );
    assert_eq!(
        u64::from(now.template_version),
        u64::from(old.template_version) + events.len() as u64,
        "each event advances the version by one",
    );
    Outcome::Mutated(id)
}

fn fixed_head(cluster: &MinerCluster, t: &TenantId, id: u64) -> Vec<OwnedToken> {
    cluster
        .templates_for(t)
        .into_iter()
        .find(|l| l.template_id == id)
        .expect("leaf")
        .template
}

#[test]
fn rfc0001_21_timestamp_in_the_prefix_at_an_unsaturated_node_starts_a_new_leaf() {
    let t = TenantId::new("tenant-x");
    let a = leaf(
        7,
        &[
            Some("2026/10/17"),
            Some("traces"),
            Some("export:"),
            Some("rpc"),
            Some("error"),
        ],
        vec![],
    );
    let (mut cluster, sink) = restored(MinerConfig::default(), &t, &state(vec![a.clone()]));

    let outcome = ingest_line(
        &mut cluster,
        &sink,
        &t,
        "2026/10/18 traces export: rpc error",
    );

    assert!(
        matches!(outcome, Outcome::NewLeaf(id) if id > 7),
        "{outcome:?}"
    );
    let kept = cluster.snapshot_state(&t);
    assert!(kept.leaves.contains(&a), "the restored leaf is untouched");
}

#[test]
fn rfc0001_21_timestamp_in_the_prefix_at_a_saturated_node_follows_the_overflow_child() {
    // `max_node_children = 1`: `alpha` fills the root, so the pre-rules
    // build routed `A`'s date through the `<*>` overflow child.
    let t = TenantId::new("tenant-x");
    let config = MinerConfig::default()
        .with_max_node_children(1)
        .expect("in range");
    let b = leaf(
        3,
        &[
            Some("alpha"),
            Some("traces"),
            Some("export:"),
            Some("rpc"),
            Some("error"),
        ],
        vec![],
    );
    let a = LeafRecord {
        wildcard_routed: vec![0],
        ..leaf(
            7,
            &[
                Some("2026/10/17"),
                Some("traces"),
                Some("export:"),
                Some("rpc"),
                Some("error"),
            ],
            vec![],
        )
    };
    let (mut cluster, sink) = restored(config, &t, &state(vec![b, a]));

    let outcome = ingest_line(
        &mut cluster,
        &sink,
        &t,
        "2026/10/18 traces export: rpc error",
    );

    // The unseen `<TS>` follows the overflow child to `A`, where §6.2
    // steps 4–5 widen the date (simSeq 4/5) under the same id.
    assert_eq!(outcome, Outcome::Mutated(7));
    assert_eq!(fixed_head(&cluster, &t, 7)[0], OwnedToken::Wildcard);
}

#[test]
fn rfc0001_21_timestamp_outside_the_prefix_reaches_the_restored_leaf() {
    let t = TenantId::new("tenant-x");
    let a = leaf(
        7,
        &[
            Some("worker"),
            Some("started"),
            Some("at"),
            Some("16:55:02"),
        ],
        vec![],
    );
    let (mut cluster, sink) = restored(MinerConfig::default(), &t, &state(vec![a]));

    let outcome = ingest_line(&mut cluster, &sink, &t, "worker started at 16:55:09");

    assert_eq!(
        outcome,
        Outcome::Mutated(7),
        "steps 4–5 widen the literal time"
    );
    assert_eq!(fixed_head(&cluster, &t, 7)[3], OwnedToken::Wildcard);
}

#[test]
fn rfc0001_21_decimal_reaching_a_restored_num_leaf_is_an_ordinary_match() {
    let t = TenantId::new("tenant-x");
    let a = leaf(
        7,
        &[
            Some("backing"),
            Some("off"),
            Some("for"),
            None,
            Some("seconds"),
        ],
        vec![vec![ParamTypeRecord::Num]],
    );
    let (mut cluster, sink) = restored(MinerConfig::default(), &t, &state(vec![a.clone()]));

    let outcome = ingest_line(&mut cluster, &sink, &t, "backing off for 0.3786463 seconds");

    assert_eq!(outcome, Outcome::Matched(7));
    assert!(
        cluster.snapshot_state(&t).leaves.contains(&a),
        "same id and version"
    );
}

#[test]
fn rfc0001_21_a_post_rules_ts_path_restores_to_the_same_leaf() {
    let t = TenantId::new("tenant-x");
    let mut original = MinerCluster::new(MinerConfig::default());
    let id = original.ingest(&string_record(
        &t,
        "2026/10/17 16:55:02 traces export: rpc error",
    ));
    let snapshot = original.snapshot_state(&t);
    assert!(
        matches!(snapshot.leaves[0].template[0], TokenRecord::Wildcard),
        "fixture: a `<TS>` wildcard on the descend path",
    );

    let (mut cluster, sink) = restored(MinerConfig::default(), &t, &snapshot);

    let outcome = ingest_line(
        &mut cluster,
        &sink,
        &t,
        "2026/10/17 16:55:09 traces export: rpc error",
    );
    assert_eq!(outcome, Outcome::Matched(id));
}
