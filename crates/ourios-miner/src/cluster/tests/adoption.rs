use super::*;
use crate::upstream::LOG_RECORD_TEMPLATE_ATTR;

// ── RFC 0050 §3.2 upstream-template modes ────────────────────

/// Test helper — a string record carrying a
/// `log.record.template` attribute.
fn annotated_record(tenant: &TenantId, body: &str, template: &str) -> OtlpLogRecord {
    use ourios_core::otlp::KeyValue;
    let mut record = string_record(tenant, body);
    record.attributes.push(KeyValue {
        key: LOG_RECORD_TEMPLATE_ATTR.to_string(),
        value: Some(AnyValue {
            value: Some(AvValue::StringValue(template.to_string())),
        }),
        ..Default::default()
    });
    record
}

fn adopt_config() -> MinerConfig {
    MinerConfig::default().with_upstream_templates(UpstreamTemplates::Adopt)
}

#[test]
fn rfc0050_1_default_mines_as_if_unannotated_and_stores_verbatim() {
    let t = TenantId::new("tenant-x");
    let bodies = ["user alice logged in", "user bob logged in"];

    // The pre-RFC build had no upstream-template handling at all,
    // so its outcome on an annotated corpus is exactly what the
    // miner produces when the attribute does not participate:
    // mine the same bodies with and without the annotation under
    // the unset default and require identical miner output.
    let records = SharedRecordSink::new();
    let plain_records = SharedRecordSink::new();
    let mut annotated_run =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));
    let mut plain_run =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(plain_records.clone()));
    for body in bodies {
        let with_attr = annotated_run.ingest(&annotated_record(&t, body, "user <*> logged in"));
        let without = plain_run.ingest(&string_record(&t, body));
        assert_eq!(with_attr, without, "the annotation changed a template_id");
    }

    // Identical miner-derived state: same leaves, ids, versions;
    // no adoption surface was touched.
    // `templates_for` carries no ordering contract — compare as
    // a sorted set.
    let shape = |c: &MinerCluster| -> Vec<(String, u64, u32)> {
        let mut leaves: Vec<_> = c
            .templates_for(&t)
            .iter()
            .map(|l| {
                (
                    format_template(&l.template),
                    l.template_id,
                    l.template_version,
                )
            })
            .collect();
        leaves.sort();
        leaves
    };
    assert_eq!(shape(&annotated_run), shape(&plain_run));
    assert!(annotated_run.adopted_templates_for(&t).is_empty());

    // The attribute is stored as an ordinary attribute, verbatim
    // (RFC 0018 fidelity — nothing consumed, nothing rewritten),
    // and **every other emitted field** — params, body handling,
    // confidence, severity, the lot — is identical to the plain
    // run's record. The Parquet layout is a pure function of this
    // record stream and the (identical) config, so record-level
    // equality is the §5.1 "every miner-derived column and the
    // file layout" clause at its source.
    let stored = records.drain();
    let plain = plain_records.drain();
    assert_eq!(stored.len(), bodies.len());
    assert_eq!(plain.len(), bodies.len());
    for (record, plain_record) in stored.iter().zip(&plain) {
        let claim = record
            .attributes
            .iter()
            .find(|kv| kv.key == LOG_RECORD_TEMPLATE_ATTR)
            .expect("the annotation survives as an ordinary attribute");
        assert_eq!(
            claim.value.as_ref().and_then(|v| v.value.as_ref()),
            Some(&AvValue::StringValue("user <*> logged in".to_string())),
        );
        let mut stripped = record.clone();
        stripped
            .attributes
            .retain(|kv| kv.key != LOG_RECORD_TEMPLATE_ATTR);
        assert_eq!(
            &stripped, plain_record,
            "modulo the annotation itself, the emitted record is \
             field-for-field the pre-RFC one",
        );
    }
}

#[test]
fn rfc0050_2_adoption_uses_the_upstream_string() {
    let t = TenantId::new("tenant-x");
    let records = SharedRecordSink::new();
    let audit = SharedAuditSink::new();
    let mut cluster = MinerCluster::with_audit_sink(adopt_config(), Box::new(audit.clone()))
        .with_record_sink(Box::new(records.clone()));

    let id_a = cluster.ingest(&annotated_record(
        &t,
        "user alice logged in",
        "user <*> logged in",
    ));
    let id_b = cluster.ingest(&annotated_record(
        &t,
        "user bob logged in",
        "user <name> logged in",
    ));

    // Two records sharing a canonical shape share one id; the
    // Drain tree gains no leaf for them (RFC0050.2).
    assert_ne!(id_a, NO_TEMPLATE);
    assert_eq!(id_a, id_b);
    assert!(cluster.templates_for(&t).is_empty(), "no tree leaf");

    let adopted = cluster.adopted_templates_for(&t);
    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0].canonical, "user <*> logged in");
    assert!(adopted[0].owned);
    assert_eq!(
        adopted[0].provenance,
        Some(ProvenanceSet::singleton(Provenance::UpstreamDerived)),
    );
    // Both raw spellings of the shape are associated.
    assert_eq!(
        adopted[0].upstream_associations,
        vec![
            "user <*> logged in".to_string(),
            "user <name> logged in".to_string()
        ],
    );

    // One provenance transition, one audit event (§3.3).
    let events = audit.drain();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0].payload,
        AuditPayload::Template {
            change: TemplateChange::Adopted {
                template_version: 1,
                ..
            },
            ..
        }
    ));

    // Emitted rows reconstruct byte for byte (§3.4 step 2).
    let rows = records.drain();
    assert_eq!(rows.len(), 2);
    let template = crate::tree::parse_template("user <*> logged in");
    for (row, original) in rows
        .iter()
        .zip(["user alice logged in", "user bob logged in"])
    {
        assert!((row.confidence - 1.0).abs() < f32::EPSILON);
        assert!(!row.lossy_flag);
        assert_eq!(
            crate::reconstruct::reconstruct(row, &template),
            original.as_bytes()
        );
    }
}

#[test]
fn rfc0050_3_mixed_stream_adopts_and_mines_side_by_side() {
    let t = TenantId::new("tenant-x");
    let mut cluster = MinerCluster::new(adopt_config());

    let adopted_id = cluster.ingest(&annotated_record(&t, "job 7 finished", "job <*> finished"));
    let mined_id = cluster.ingest(&string_record(&t, "cache warmed successfully"));

    assert_ne!(adopted_id, NO_TEMPLATE);
    assert_ne!(mined_id, NO_TEMPLATE);
    assert_ne!(adopted_id, mined_id);
    // Both provenances visible in the registry surfaces.
    assert_eq!(cluster.adopted_templates_for(&t).len(), 1);
    let leaves = cluster.templates_for(&t);
    assert_eq!(leaves.len(), 1);
    assert_eq!(leaves[0].template_id, mined_id);
    assert_eq!(
        leaves[0].provenance,
        ProvenanceSet::singleton(Provenance::Mined)
    );
    assert_eq!(cluster.template_count(&t), 2);
}

#[test]
fn rfc0050_4_grammar_and_alignment_gate_adoption() {
    let t = TenantId::new("tenant-x");
    let mut cluster = MinerCluster::new(adopt_config());

    // Foreign placeholder syntaxes and misaligned claims are
    // never adopted — the record is mined as if unannotated.
    for (body, template) in [
        ("User alice logged in", "User %s logged in"),
        ("count {} reached", "count {} reached"),
        ("path $HOME missing", "path $HOME missing"),
        ("user alice logged out", "user <*> logged in"),
        ("a b c", "a <*>"),
    ] {
        let id = cluster.ingest(&annotated_record(&t, body, template));
        assert_ne!(id, NO_TEMPLATE, "{template:?} must fall back to mining");
    }
    assert!(
        cluster.adopted_templates_for(&t).is_empty(),
        "no rejected claim may intern",
    );
    assert_eq!(
        cluster.templates_for(&t).len(),
        5,
        "each body mined normally"
    );
}

#[test]
fn rfc0050_5_ceiling_stops_adoption_interning() {
    let t = TenantId::new("tenant-x");
    let config = adopt_config()
        .with_max_templates(2)
        .expect("non-zero ceiling");
    let mut cluster = MinerCluster::new(config);

    let a = cluster.ingest(&annotated_record(&t, "alpha one", "alpha <*>"));
    let b = cluster.ingest(&annotated_record(&t, "beta two", "beta <*>"));
    assert_ne!(a, NO_TEMPLATE);
    assert_ne!(b, NO_TEMPLATE);

    // Third distinct shape: adoption refuses at the ceiling and
    // the documented fallback (mining) also diverts — the
    // record lands body-retained with NO_TEMPLATE.
    let c = cluster.ingest(&annotated_record(&t, "gamma three", "gamma <*>"));
    assert_eq!(c, NO_TEMPLATE);
    assert_eq!(cluster.adopted_templates_for(&t).len(), 2);
    assert_eq!(cluster.template_count(&t), 2);
    assert_eq!(cluster.parse_failures_total(), 1);
}

#[test]
fn rfc0050_5_byte_limit_rejects_before_parsing() {
    let t = TenantId::new("tenant-x");
    let config = adopt_config().with_upstream_template_byte_limit(8);
    let mut cluster = MinerCluster::new(config);

    let id = cluster.ingest(&annotated_record(
        &t,
        "user alice logged in",
        "user <*> logged in",
    ));
    assert_ne!(id, NO_TEMPLATE);
    assert!(
        cluster.adopted_templates_for(&t).is_empty(),
        "over-cap value never interns"
    );
    assert_eq!(
        cluster.templates_for(&t).len(),
        1,
        "the record mined instead"
    );
}

#[test]
fn rfc0050_6_mined_first_adopted_second_converges() {
    let t = TenantId::new("tenant-x");
    let audit = SharedAuditSink::new();
    let mut cluster = MinerCluster::with_audit_sink(adopt_config(), Box::new(audit.clone()));

    // Mining first: masking makes 42 a wildcard, so the mined
    // canonical is exactly the upstream shape.
    let mined_id = cluster.ingest(&string_record(&t, "user 42 logged in"));
    let adopted_id = cluster.ingest(&annotated_record(
        &t,
        "user 43 logged in",
        "user <*> logged in",
    ));

    assert_eq!(mined_id, adopted_id, "one template_id (RFC0050.6)");
    let leaves = cluster.templates_for(&t);
    assert_eq!(leaves.len(), 1);
    assert!(leaves[0].provenance.contains(Provenance::Mined));
    assert!(leaves[0].provenance.contains(Provenance::UpstreamDerived));
    // The map entry rides the leaf — nothing owned, count unchanged.
    let adopted = cluster.adopted_templates_for(&t);
    assert_eq!(adopted.len(), 1);
    assert!(!adopted[0].owned);
    assert_eq!(cluster.template_count(&t), 1);
    // Created (mined) + Adopted (first adoption) — exactly two.
    let kinds: Vec<String> = audit
        .drain()
        .iter()
        .map(|e| e.payload.event_type().to_string())
        .collect();
    assert_eq!(kinds, vec!["template_created", "template_adopted"]);
}

/// RFC0050.6 when two mined leaves share a canonical: one widening away
/// must not hide the other from the convergence guard, or the adoption
/// interns an owned id beside a mined leaf of exactly its shape.
#[test]
fn rfc0050_6_converges_on_a_leaf_whose_twin_widened_away() {
    let t = TenantId::new("tenant-x");
    let mut cluster = MinerCluster::new(adopt_config().with_prefix_depth(1).expect("in range"));
    // Different masked first tokens (IP, NUM): two leaves, one shape.
    let _ = cluster.ingest(&string_record(&t, "10.0.0.1 did a b"));
    let twin = cluster.ingest(&string_record(&t, "42 did a b"));
    let _ = cluster.ingest(&string_record(&t, "10.0.0.2 did a c"));

    let adopted = cluster.ingest(&annotated_record(&t, "7 did a b", "<*> did a b"));

    assert_eq!(adopted, twin, "the adoption rides the remaining mined leaf");
    assert!(cluster.adopted_templates_for(&t).iter().all(|a| !a.owned));
}

/// RFC0050.6 when two mined leaves share a canonical: the adoption
/// lands on the lower id, whatever order the tree's maps iterate in, so
/// a restored tenant converges exactly as the live one did.
#[test]
fn rfc0050_6_converges_on_the_lowest_id_of_a_shared_shape() {
    let t = TenantId::new("tenant-x");
    let mut cluster = MinerCluster::new(adopt_config());
    let lower = cluster.ingest(&string_record(&t, "10.0.0.1"));
    let higher = cluster.ingest(&string_record(&t, "550e8400-e29b-41d4-a716-446655440000"));
    assert!(lower < higher, "fixture: two leaves of shape <*>");

    let adopted = cluster.ingest(&annotated_record(&t, "logout", "<*>"));

    assert_eq!(adopted, lower);
}

#[test]
fn readoption_after_widening_emits_no_second_audit() {
    // §3.3 "once per provenance transition": a leaf adopted at
    // one canonical, widened to a new canonical, then adopted
    // again under the new shape already carries
    // `upstream_derived` — the second adoption is a cache fill,
    // not a transition, and must not add audit noise.
    let t = TenantId::new("tenant-x");
    let audit = SharedAuditSink::new();
    let mut cluster = MinerCluster::with_audit_sink(adopt_config(), Box::new(audit.clone()));

    let _ = cluster.ingest(&string_record(&t, "user 42 logged in"));
    let first = cluster.ingest(&annotated_record(
        &t,
        "user 43 logged in",
        "user <*> logged in",
    ));
    // Widen position 3: "in" vs "out" under a clean-zone match.
    let _ = cluster.ingest(&string_record(&t, "user 44 logged out"));
    let second = cluster.ingest(&annotated_record(
        &t,
        "user 45 logged off",
        "user <*> logged <*>",
    ));
    assert_eq!(first, second, "both adoptions ride the one widened leaf");

    let kinds: Vec<String> = audit
        .drain()
        .iter()
        .map(|e| e.payload.event_type().to_string())
        .collect();
    assert_eq!(
        kinds,
        vec!["template_created", "template_adopted", "template_widened"],
        "exactly one template_adopted — the re-adoption is silent",
    );
}

#[test]
fn rfc0050_6_adopted_first_mined_second_converges() {
    let t = TenantId::new("tenant-x");
    let mut cluster = MinerCluster::new(adopt_config());

    let adopted_id = cluster.ingest(&annotated_record(
        &t,
        "user 43 logged in",
        "user <*> logged in",
    ));
    let mined_id = cluster.ingest(&string_record(&t, "user 42 logged in"));

    assert_eq!(adopted_id, mined_id, "one template_id (RFC0050.6)");
    let leaves = cluster.templates_for(&t);
    assert_eq!(leaves.len(), 1, "the mined leaf took over the identity");
    assert!(leaves[0].provenance.contains(Provenance::Mined));
    assert!(leaves[0].provenance.contains(Provenance::UpstreamDerived));
    let adopted = cluster.adopted_templates_for(&t);
    assert_eq!(adopted.len(), 1);
    assert!(!adopted[0].owned, "the entry flipped to tree-backed");
    assert_eq!(cluster.template_count(&t), 1, "the identity counted once");
}

#[test]
fn rfc0050_6_alias_binds_a_mined_template_to_an_adopted_one() {
    use std::time::SystemTime;

    use ourios_core::alias::{ActorId, AliasMap, Operator};

    let t = TenantId::new("tenant-x");
    let audit = SharedAuditSink::new();
    let mut cluster = MinerCluster::with_audit_sink(adopt_config(), Box::new(audit.clone()));

    let adopted_id = cluster.ingest(&annotated_record(&t, "job 7 finished", "job <*> finished"));
    let mined_id = cluster.ingest(&string_record(&t, "task 9 done"));
    assert_ne!(adopted_id, mined_id);

    // The RFC 0007 alias surface takes both ids like any pair —
    // adoption-interned ids live in the same id space as tree
    // leaves, so an operator can bind across provenance.
    let mut aliases = AliasMap::new();
    let mut sink = audit.clone();
    aliases
        .assert(
            &mut sink,
            &t,
            mined_id,
            vec![adopted_id],
            Operator {
                actor: ActorId::new("ops@example.com").expect("actor"),
                reason: "same job-completion shape".to_string(),
                timestamp: SystemTime::UNIX_EPOCH,
            },
        )
        .expect("alias binds a mined id to an adopted id");
    let class = aliases.resolves(&t, adopted_id);
    assert!(class.contains(&mined_id) && class.contains(&adopted_id));
}

#[test]
fn rfc0050_9_observe_associates_without_touching_the_clustering() {
    let t = TenantId::new("tenant-x");
    let bodies = ["user 1 logged in", "user 2 logged in", "user 3 logged in"];
    let observe_config = MinerConfig::default()
        .with_upstream_templates(UpstreamTemplates::Observe)
        .with_upstream_association_limit(1);

    let mut ignored = MinerCluster::new(MinerConfig::default());
    let mut observing = MinerCluster::new(observe_config);
    for (i, body) in bodies.iter().enumerate() {
        let _ = ignored.ingest(&string_record(&t, body));
        // Two distinct upstream spellings map onto the one
        // mined template — the coarser/finer disagreement made
        // visible (§3.2), with the second one overflowing the
        // bound of 1.
        let template = if i == 0 {
            "user <*> logged in"
        } else {
            "user <id> logged in"
        };
        let _ = observing.ingest(&annotated_record(&t, body, template));
    }

    // The clustering is untouched: identical templates and ids.
    let base: Vec<(String, u64)> = ignored
        .templates_for(&t)
        .iter()
        .map(|l| (format_template(&l.template), l.template_id))
        .collect();
    let observed: Vec<(String, u64)> = observing
        .templates_for(&t)
        .iter()
        .map(|l| (format_template(&l.template), l.template_id))
        .collect();
    assert_eq!(base, observed);

    // The mined entry carries the association, bounded, with
    // the overflow counted (bound 1: the second distinct
    // spelling overflows on both its observations).
    let leaves = observing.templates_for(&t);
    assert_eq!(leaves.len(), 1);
    assert_eq!(
        leaves[0].upstream_associations,
        vec!["user <*> logged in".to_string()]
    );
    assert_eq!(leaves[0].upstream_association_overflow, 2);
    assert!(
        observing.adopted_templates_for(&t).is_empty(),
        "observe never interns"
    );
}

#[test]
fn rfc0050_adopted_state_round_trips_through_snapshot() {
    let t = TenantId::new("tenant-x");
    let mut cluster = MinerCluster::new(adopt_config());
    let id = cluster.ingest(&annotated_record(&t, "job 7 finished", "job <*> finished"));
    let snapshot = cluster.snapshot_state(&t);

    let mut restored = MinerCluster::new(adopt_config());
    restored.restore_tenant(&t, &snapshot).expect("restores");
    assert_eq!(
        restored.adopted_templates_for(&t),
        cluster.adopted_templates_for(&t)
    );

    // The restored cache resolves a new record of the same
    // shape to the same id without re-interning.
    let again = restored.ingest(&annotated_record(&t, "job 9 finished", "job <*> finished"));
    assert_eq!(again, id);
    assert_eq!(restored.template_count(&t), 1);
}

#[test]
fn restore_rejects_dangling_tree_backed_adoption() {
    let state = SnapshotState {
        leaves: vec![],
        structured_templates: vec![],
        wal_high_water: None,
        adopted_templates: vec![crate::snapshot::AdoptedTemplateRecord {
            canonical: "job <*> done".to_string(),
            severity_number: 0,
            scope_name: None,
            template_id: 5,
            template_version: 1,
            owned: false,
            provenance: vec![],
            upstream_associations: vec![],
            upstream_association_overflow: 0,
        }],
    };
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let err = cluster
        .restore_tenant(&TenantId::new("tenant-x"), &state)
        .expect_err("a tree-backed entry with no leaf must be rejected");
    assert!(matches!(err, RestoreError::Inconsistent { .. }));
}

#[test]
fn restore_rejects_mismatched_tree_backed_adoption() {
    // The leaf exists but its current-version tokens disagree
    // with the recorded canonical.
    let state = SnapshotState {
        leaves: vec![LeafRecord {
            template: vec![
                TokenRecord::Fixed("disk".to_string()),
                TokenRecord::Fixed("full".to_string()),
            ],
            template_id: 5,
            template_version: 1,
            severity_number: 0,
            scope_name: None,
            slot_types: vec![],
            provenance: vec![],
            upstream_associations: vec![],
            upstream_association_overflow: 0,
            wildcard_routed: vec![],
        }],
        structured_templates: vec![],
        wal_high_water: None,
        adopted_templates: vec![crate::snapshot::AdoptedTemplateRecord {
            canonical: "job <*> done".to_string(),
            severity_number: 0,
            scope_name: None,
            template_id: 5,
            template_version: 1,
            owned: false,
            provenance: vec![],
            upstream_associations: vec![],
            upstream_association_overflow: 0,
        }],
    };
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let err = cluster
        .restore_tenant(&TenantId::new("tenant-x"), &state)
        .expect_err("a canonical that disagrees with the leaf tokens must be rejected");
    assert!(matches!(err, RestoreError::Inconsistent { .. }));
}
