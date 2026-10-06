//! RFC0059.9's equivalence of two miners up to a renaming of ids.

use std::collections::{BTreeMap, BTreeSet};

use ourios_miner::cluster::MinerCluster;

/// RFC0059.9's contract: `recovered` equals `control` under an injective
/// renaming of ids that touches only ids first minted above `high_water`
/// (the tail replay's fresh block). Every other id, and every other field,
/// is exactly equal. Returns the ids the renaming moved.
pub fn assert_equivalent_up_to_renaming(
    recovered: &MinerCluster,
    control: &MinerCluster,
    high_water: u64,
) -> BTreeSet<u64> {
    assert_eq!(recovered.tenant_ids(), control.tenant_ids());
    let mut renamed = BTreeSet::new();
    for tenant in control.tenant_ids() {
        let (got, want) = (
            recovered.snapshot_state(&tenant),
            control.snapshot_state(&tenant),
        );
        let renaming = renaming(&want, &got);
        let mut targets = BTreeSet::new();
        for (from, to) in &renaming {
            assert!(
                targets.insert(*to),
                "{tenant:?}: the renaming is not injective at {to}"
            );
            if from != to {
                assert!(
                    *to > high_water,
                    "{tenant:?}: only ids minted above {high_water} may move ({from} -> {to})"
                );
                renamed.insert(*to);
            }
        }
        assert_eq!(
            got,
            renamed_state(&want, &renaming),
            "{tenant:?}: equal up to the renaming of tail-minted ids",
        );
    }
    renamed
}

/// Every template id `cluster` holds, per tenant.
pub fn ids_per_tenant(cluster: &MinerCluster) -> BTreeMap<String, BTreeSet<u64>> {
    cluster
        .tenant_ids()
        .into_iter()
        .map(|tenant| {
            let ids = ids_by_content(&cluster.snapshot_state(&tenant))
                .into_values()
                .collect();
            (tenant.as_str().to_owned(), ids)
        })
        .collect()
}

type State = ourios_miner::snapshot::SnapshotState;

/// Each template of `state`, keyed by its content.
fn ids_by_content(state: &State) -> BTreeMap<String, u64> {
    let leaves = state.leaves.iter().map(|l| {
        let key = (&l.template, l.severity_number, &l.scope_name);
        (format!("leaf {key:?}"), l.template_id)
    });
    let structured = state.structured_templates.iter().map(|s| {
        let key = (s.severity_number, &s.scope_name, &s.event_name);
        (format!("structured {key:?}"), s.template_id)
    });
    let adopted = state.adopted_templates.iter().map(|a| {
        let key = (&a.canonical, a.severity_number, &a.scope_name);
        (format!("adopted {key:?}"), a.template_id)
    });
    leaves.chain(structured).chain(adopted).collect()
}

/// Pair each template of `from` with the template of `to` that has the
/// same content, id to id.
fn renaming(from: &State, to: &State) -> BTreeMap<u64, u64> {
    let (from, to) = (ids_by_content(from), ids_by_content(to));
    assert_eq!(
        from.keys().collect::<Vec<_>>(),
        to.keys().collect::<Vec<_>>(),
        "the same templates on both sides"
    );
    from.into_iter().map(|(key, id)| (id, to[&key])).collect()
}

fn renamed_state(state: &State, renaming: &BTreeMap<u64, u64>) -> State {
    let mut state = state.clone();
    for leaf in &mut state.leaves {
        leaf.template_id = renaming[&leaf.template_id];
    }
    for structured in &mut state.structured_templates {
        structured.template_id = renaming[&structured.template_id];
    }
    for adopted in &mut state.adopted_templates {
        adopted.template_id = renaming[&adopted.template_id];
    }
    state.leaves.sort_by_key(|l| l.template_id);
    state.structured_templates.sort_by_key(|s| s.template_id);
    state
        .adopted_templates
        .sort_by(|a, b| (a.template_id, &a.canonical).cmp(&(b.template_id, &b.canonical)));
    state
}
