//! RFC0059.1 — A discarded snapshot never re-issues a published id.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use std::collections::BTreeSet;

use ourios_core::tenant::TenantId;
use ourios_ingester::recovery::SnapshotFate;
use ourios_ingester::snapshot_store;
use ourios_miner::snapshot::{SNAPSHOT_VERSION, load_snapshot};

use crate::rfc0059_support::{Node, Restarted, cut_and_reclaim, publish};

const DISCARDED: &str = "checkout";
const KEPT: &str = "search";

/// What a test does to the discarded tenant's artefact, named by the
/// `error.type` recovery reports for it.
#[derive(Clone, Copy, Debug)]
enum Discard {
    UnknownVersion,
    Corrupt,
    Empty,
    NoHorizon,
    RestoreFailed,
}

impl Discard {
    const ALL: [Self; 5] = [
        Self::UnknownVersion,
        Self::Corrupt,
        Self::Empty,
        Self::NoHorizon,
        Self::RestoreFailed,
    ];

    fn error_type(self) -> &'static str {
        match self {
            Self::UnknownVersion => "unknown_version",
            Self::Corrupt => "corrupt",
            Self::Empty => "empty",
            Self::NoHorizon => "no_horizon",
            Self::RestoreFailed => "restore_failed",
        }
    }

    fn apply(self, node: &Node) {
        let artefact = node.snapshots.join(format!("{DISCARDED}.snap"));
        let bytes = std::fs::read(&artefact).expect("artefact");
        match self {
            Self::UnknownVersion => node.overwrite_snapshot(DISCARDED, &[SNAPSHOT_VERSION + 9]),
            Self::Corrupt => node.overwrite_snapshot(DISCARDED, &[SNAPSHOT_VERSION, 0xff, 0]),
            Self::Empty => node.overwrite_snapshot(DISCARDED, &[]),
            Self::NoHorizon => {
                let mut state = load_snapshot(&bytes).expect("decodes");
                state.wal_high_water = None;
                snapshot_store::write(&node.snapshots, &TenantId::new(DISCARDED), &state)
                    .expect("rewrite");
            }
            Self::RestoreFailed => {
                let mut state = load_snapshot(&bytes).expect("decodes");
                let leaf = state.leaves.first().cloned().expect("a mined leaf");
                state.leaves.push(leaf);
                snapshot_store::write(&node.snapshots, &TenantId::new(DISCARDED), &state)
                    .expect("rewrite");
            }
        }
    }
}

/// Scenario RFC0059.1 — no new id equals a published one, for each
/// discard class, over string and structured templates.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_1_no_new_id_equals_a_published_one_for_each_discard_class() {
    for discard in Discard::ALL {
        let tmp = tempfile::TempDir::new().expect("temp");
        let node = published_node(tmp.path()).await;
        let issued = node.issued();
        discard.apply(&node);

        let mut restarted = node.restart().expect("recover");
        assert_discarded(&restarted, discard);
        let kept = restored_ids(&restarted);
        let minted = mint_after_restart(&mut restarted);

        let reissued: BTreeSet<&u64> = minted
            .iter()
            .filter(|id| issued.contains(id) && !kept.contains(id))
            .collect();
        assert!(
            reissued.is_empty(),
            "{discard:?}: re-issued {reissued:?} (published {issued:?})",
        );
    }
}

/// Scenario RFC0059.1 — an old shape first seen in a reclaimed frame
/// re-mints under a fresh id above the high-water, never another
/// template's.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_1_an_old_shape_re_mints_under_a_fresh_id() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = published_node(tmp.path()).await;
    let issued = node.issued();
    Discard::Corrupt.apply(&node);

    let mut restarted = node.restart().expect("recover");
    let high_water = restarted.report.template_ids.high_water;
    let old_shape = restarted.mine(DISCARDED, "user alice logged in");

    assert!(old_shape > high_water, "{old_shape} is above {high_water}");
    assert!(!issued.contains(&old_shape), "drift, not a collision");
}

/// A node whose two tenants minted string and structured templates, cut,
/// published them and reclaimed every frame.
async fn published_node(tmp: &std::path::Path) -> Node {
    let rig = Node::rig(tmp);
    publish(&rig, KEPT, &["query 7 served"], &[]).await;
    publish(
        &rig,
        DISCARDED,
        &["user alice logged in", "disk sda1 is 91 percent full"],
        &["checkout.paid"],
    )
    .await;
    cut_and_reclaim(&rig).await;
    Node::stop(rig, tmp)
}

fn assert_discarded(restarted: &Restarted, discard: Discard) {
    let fate = restarted
        .report
        .tenants
        .iter()
        .find(|t| t.tenant_id.as_str() == DISCARDED)
        .map(|t| t.fate.clone());
    match fate {
        Some(SnapshotFate::Discarded(reason)) => {
            assert_eq!(reason.error_type(), discard.error_type());
        }
        other => panic!("{discard:?}: the snapshot was not discarded: {other:?}"),
    }
}

/// The ids the kept tenant's snapshot restored: a later line may attach
/// to one of them.
fn restored_ids(restarted: &Restarted) -> BTreeSet<u64> {
    let state = restarted.miner.snapshot_state(&TenantId::new(KEPT));
    state
        .leaves
        .iter()
        .map(|l| l.template_id)
        .chain(state.structured_templates.iter().map(|s| s.template_id))
        .collect()
}

/// New and old shapes, string and structured, across both tenants.
fn mint_after_restart(restarted: &mut Restarted) -> BTreeSet<u64> {
    [
        restarted.mine(DISCARDED, "order 7 shipped to berlin"),
        restarted.mine(DISCARDED, "user alice logged in"),
        restarted.mine(DISCARDED, "disk sda1 is 91 percent full"),
        restarted.mine_structured(DISCARDED, "checkout.paid"),
        restarted.mine_structured(DISCARDED, "checkout.refunded"),
        restarted.mine(KEPT, "cache evicted 5 keys"),
        restarted.mine(KEPT, "query 7 served"),
    ]
    .into_iter()
    .collect()
}
