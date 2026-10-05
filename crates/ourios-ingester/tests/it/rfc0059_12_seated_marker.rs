//! RFC0059.12 — Snapshots written before a root's first seat are never
//! restored.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use std::collections::BTreeSet;

use ourios_ingester::recovery::{RecoveryDriverError, SnapshotFate};
use ourios_ingester::template_ids::{SEATED_MARKER, TemplateIdsError};

use crate::rfc0059_support::{Hooks, Node, cut_and_reclaim, publish};

/// Two pre-RFC receivers on one store: each counted from 1, so their
/// snapshots hold ids the other also used.
async fn two_pre_rfc_receivers(tmp: &std::path::Path) -> (Node, Node) {
    let store = tmp.join("store");
    let rig = Node::rig(&tmp.join("a"));
    publish(
        &rig,
        "checkout",
        &["user alice logged in"],
        &["checkout.paid"],
    )
    .await;
    cut_and_reclaim(&rig).await;
    let a = Node::stop_into(rig, &tmp.join("a"), &store);
    let rig = Node::rig(&tmp.join("b"));
    publish(
        &rig,
        "checkout",
        &["disk sda1 is 91 percent full", "order 7 shipped"],
        &[],
    )
    .await;
    cut_and_reclaim(&rig).await;
    let b = Node::stop_into(rig, &tmp.join("b"), &store);
    (a, b)
}

fn artefacts(node: &Node) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(&node.snapshots)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "snap"))
                .collect()
        })
        .unwrap_or_default()
}

/// Scenario RFC0059.12 — a markerless root over a seated store discards
/// every artefact, and allocates nothing the seated replica issued.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_12_a_markerless_root_over_a_seated_store_discards_its_snapshots() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (a, b) = two_pre_rfc_receivers(tmp.path()).await;

    // Given A bootstrapped the high-water and allocated from it.
    let mut a_running = a.restart().expect("A bootstraps");
    assert!(a_running.report.template_ids.bootstrapped);
    let a_issued: BTreeSet<u64> = [
        a_running.mine("checkout", "cache evicted 5 keys"),
        a_running.mine("checkout", "payment 9 settled"),
    ]
    .into_iter()
    .collect();

    // When B starts with no seated marker.
    let mut b_running = b.restart().expect("B recovers");

    // Then every artefact of B's is discarded as predating the seat,
    // removed, and B's marker written.
    assert!(!b_running.report.tenants.is_empty());
    for tenant in &b_running.report.tenants {
        match &tenant.fate {
            SnapshotFate::Discarded(reason) => {
                assert_eq!(reason.error_type(), "predates_high_water");
            }
            other @ SnapshotFate::Restored(_) => panic!("{other:?}"),
        }
    }
    assert!(artefacts(&b).is_empty(), "the untrusted artefacts are gone");
    assert!(b.snapshots.join(SEATED_MARKER).exists());

    // And nothing B allocates equals an id A issued since the bootstrap.
    let b_minted: BTreeSet<u64> = [
        b_running.mine("checkout", "disk sda1 is 91 percent full"),
        b_running.mine("checkout", "cache evicted 5 keys"),
        b_running.mine("checkout", "user alice logged in"),
    ]
    .into_iter()
    .collect();
    assert!(
        b_minted.is_disjoint(&a_issued),
        "{b_minted:?} vs {a_issued:?}"
    );
}

/// Scenario RFC0059.12 — a start that fails before its marker leaves the
/// next start the same decision, with no untrusted artefact left behind.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_12_the_marker_is_written_only_after_the_artefacts_are_gone() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (a, b) = two_pre_rfc_receivers(tmp.path()).await;
    drop(a.restart().expect("A bootstraps"));

    // A directory where the marker belongs makes its rename fail: the
    // start dies after the removal and before the marker.
    let marker = b.snapshots.join(SEATED_MARKER);
    std::fs::create_dir(&marker).expect("block the marker");
    std::fs::write(marker.join("occupied"), b"x").expect("occupy it");
    let Err(err) = b.restart() else {
        panic!("the marker write must fail");
    };
    assert!(
        matches!(
            err,
            RecoveryDriverError::TemplateIds(TemplateIdsError::Marker { .. })
        ),
        "{err}"
    );
    assert!(artefacts(&b).is_empty(), "removed before the marker");

    std::fs::remove_dir_all(&marker).expect("unblock");
    let restarted = b.restart().expect("the next start");
    assert!(restarted.report.tenants.is_empty(), "nothing left to trust");
    assert!(marker.is_file());
}

/// Scenario RFC0059.12 — when two markerless receivers race the
/// bootstrap, the loser fails startup and its restart discards.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_12_the_loser_of_the_bootstrap_race_fails_and_its_restart_discards() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (_, b) = two_pre_rfc_receivers(tmp.path()).await;
    let hooks = Hooks::default();
    hooks
        .race_the_create
        .store(true, std::sync::atomic::Ordering::Release);

    let Err(err) = b.restart_over(hooks.wrap(b.store())) else {
        panic!("the loser must fail startup");
    };
    assert!(
        matches!(
            err,
            RecoveryDriverError::TemplateIds(TemplateIdsError::BootstrapRaceLost)
        ),
        "{err}"
    );
    assert!(!b.snapshots.join(SEATED_MARKER).exists());

    let restarted = b.restart().expect("the restart");
    assert!(
        restarted
            .report
            .tenants
            .iter()
            .all(|t| matches!(&t.fate, SnapshotFate::Discarded(r) if r.error_type() == "predates_high_water")),
        "{:?}",
        restarted.report.tenants
    );
}
