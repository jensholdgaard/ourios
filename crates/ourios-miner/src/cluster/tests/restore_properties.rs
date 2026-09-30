use super::*;
use crate::snapshot::{load_snapshot, snapshot};
use crate::upstream::LOG_RECORD_TEMPLATE_ATTR;

// ---------- §6.9 restore of any reachable state (issue #892) ----------

const WORDS: [&str; 16] = [
    "user",
    "login",
    "logout",
    "zone",
    "Freeport",
    "says",
    "42",
    "7",
    "3.5",
    "10.0.0.1",
    "550e8400-e29b-41d4-a716-446655440000",
    "ok",
    "fail",
    "GET",
    "/api",
    "x=1",
];

/// One generated input record: a tenant index plus what it carries.
#[derive(Debug, Clone)]
enum Op {
    Line {
        tenant: usize,
        words: Vec<usize>,
        severity: u8,
    },
    /// A line annotated with an RFC 0050 upstream template that
    /// wildcards the chosen positions.
    Annotated {
        tenant: usize,
        words: Vec<usize>,
        wildcards: Vec<bool>,
    },
    Event {
        tenant: usize,
        severity: u8,
        scope: Option<&'static str>,
        event: Option<&'static str>,
    },
}

fn op() -> impl Strategy<Value = Op> {
    let words = || prop::collection::vec(0..WORDS.len(), 1..6);
    prop_oneof![
        4 => (0..2usize, words(), prop::sample::select(vec![0u8, 9, 13])).prop_map(
            |(tenant, words, severity)| Op::Line { tenant, words, severity }
        ),
        1 => (0..2usize, words(), prop::collection::vec(any::<bool>(), 6)).prop_map(
            |(tenant, words, wildcards)| Op::Annotated { tenant, words, wildcards }
        ),
        2 => (
            0..2usize,
            prop::sample::select(vec![9u8, 13, 17]),
            prop::option::of(prop::sample::select(vec!["eq.chat", "eq.combat"])),
            prop::option::of(prop::sample::select(vec![
                "everquest.character.profile",
                "everquest.zone.entered",
            ])),
        )
            .prop_map(|(tenant, severity, scope, event)| Op::Event {
                tenant,
                severity,
                scope,
                event,
            }),
    ]
}

fn config() -> impl Strategy<Value = MinerConfig> {
    (0u8..4, prop::sample::select(vec![1u16, 2, 3, 100])).prop_map(|(depth, children)| {
        MinerConfig::default()
            .with_upstream_templates(UpstreamTemplates::Adopt)
            .with_prefix_depth(depth)
            .and_then(|c| c.with_max_node_children(children))
            .expect("in-range generated config")
    })
}

fn tenants() -> [TenantId; 2] {
    [TenantId::new("eq-perses"), TenantId::new("nocturnal")]
}

fn record(op: &Op) -> OtlpLogRecord {
    let tenants = tenants();
    match op {
        Op::Line {
            tenant,
            words,
            severity,
        } => {
            let mut rec = string_record(&tenants[*tenant], &line(words));
            rec.severity_number = *severity;
            rec
        }
        Op::Annotated {
            tenant,
            words,
            wildcards,
        } => {
            let template: Vec<&str> = words
                .iter()
                .zip(wildcards)
                .map(|(&w, &wild)| if wild { "<*>" } else { WORDS[w] })
                .collect();
            let mut rec = string_record(&tenants[*tenant], &line(words));
            rec.attributes.push(ourios_core::otlp::KeyValue {
                key: LOG_RECORD_TEMPLATE_ATTR.to_string(),
                value: Some(AnyValue {
                    value: Some(AvValue::StringValue(template.join(" "))),
                }),
                ..Default::default()
            });
            rec
        }
        Op::Event {
            tenant,
            severity,
            scope,
            event,
        } => {
            let mut rec = structured_record(&tenants[*tenant], *severity, *scope);
            rec.event_name = event.map(str::to_string);
            rec
        }
    }
}

fn line(words: &[usize]) -> String {
    words
        .iter()
        .map(|&w| WORDS[w])
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every tenant of `original`, through the snapshot codec, into a
/// fresh cluster — the recovery driver's restore.
fn restored(original: &MinerCluster, config: MinerConfig) -> Result<MinerCluster, TestCaseError> {
    let mut fresh = MinerCluster::new(config);
    for tenant in original.tenant_ids() {
        let bytes = snapshot(&original.snapshot_state(&tenant))
            .map_err(|e| TestCaseError::fail(e.to_string()))?;
        let state = load_snapshot(&bytes).map_err(|e| TestCaseError::fail(e.to_string()))?;
        fresh
            .restore_tenant(&tenant, &state)
            .map_err(|e| TestCaseError::fail(format!("tenant {tenant:?}: {e}")))?;
    }
    Ok(fresh)
}

fn assert_same_state(left: &MinerCluster, right: &MinerCluster) -> Result<(), TestCaseError> {
    prop_assert_eq!(left.tenant_ids(), right.tenant_ids());
    for tenant in right.tenant_ids() {
        prop_assert_eq!(left.snapshot_state(&tenant), right.snapshot_state(&tenant));
        prop_assert_eq!(left.template_count(&tenant), right.template_count(&tenant));
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// RFC 0001 §6.9 / §3.5.3 (property) — any miner state that live
    /// ingest reaches, over string lines, adopted upstream templates and
    /// structured events for two tenants, snapshots and restores, and the
    /// restored cluster continues exactly as the original: the same ids
    /// for the same follow-up records and the same state after them.
    #[test]
    fn any_reachable_state_snapshots_restores_and_continues(
        config in config(),
        before in prop::collection::vec(op(), 1..200),
        after in prop::collection::vec(op(), 0..60),
    ) {
        let mut original = MinerCluster::new(config);
        for op in &before {
            let _ = original.ingest(&record(op));
        }
        let mut fresh = restored(&original, config)?;
        assert_same_state(&fresh, &original)?;

        for op in &after {
            let rec = record(op);
            prop_assert_eq!(fresh.ingest(&rec), original.ingest(&rec), "{:?}", op);
        }
        assert_same_state(&fresh, &original)?;
    }
}

/// The #892 shape: a full node routes new first tokens through the
/// wildcard child, where lines differing at that path position widen
/// the leaf there. The snapshot must record the route, or restore
/// reads the widened slot as a malformed path tag.
#[test]
fn a_leaf_widened_under_the_wildcard_child_restores() {
    let t = TenantId::new("tenant-x");
    let config = MinerConfig::default()
        .with_max_node_children(1)
        .expect("in range");
    let mut original = MinerCluster::new(config);
    for line in [
        "alpha zone was entered",
        "Freeport zone was entered",
        "Qeynos zone was entered",
    ] {
        let _ = original.ingest(&string_record(&t, line));
    }
    let state = original.snapshot_state(&t);
    let widened = state
        .leaves
        .iter()
        .find(|leaf| matches!(leaf.template[0], TokenRecord::Wildcard))
        .expect("fixture: a leaf widened at path position 0");
    assert_eq!(widened.wildcard_routed, vec![0]);

    let mut fresh = MinerCluster::new(config);
    fresh
        .restore_tenant(&t, &state)
        .expect("a widened wildcard-routed leaf restores");
    assert_eq!(fresh.snapshot_state(&t), state);
    let rec = string_record(&t, "Halas zone was entered");
    assert_eq!(fresh.ingest(&rec), original.ingest(&rec));
}

#[test]
fn restore_rejects_a_wildcard_route_past_the_walk() {
    let state = SnapshotState {
        leaves: vec![LeafRecord {
            template: vec![
                TokenRecord::Fixed("disk".to_string()),
                TokenRecord::Fixed("full".to_string()),
                TokenRecord::Fixed("again".to_string()),
            ],
            template_id: 4,
            template_version: 1,
            severity_number: 0,
            scope_name: None,
            slot_types: vec![],
            provenance: vec![],
            upstream_associations: vec![],
            upstream_association_overflow: 0,
            wildcard_routed: vec![2],
        }],
        structured_templates: vec![],
        wal_high_water: None,
        adopted_templates: vec![],
    };
    let mut cluster = MinerCluster::new(MinerConfig::default());

    let err = cluster
        .restore_tenant(&TenantId::new("tenant-x"), &state)
        .expect_err("a route below position 2 cannot exist at prefix depth 2");
    match err {
        RestoreError::Inconsistent { detail } => {
            assert!(detail.contains("template_id 4"), "{detail}");
        }
        other => panic!("expected Inconsistent, got {other:?}"),
    }
}
