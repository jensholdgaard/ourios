//! RFC0059.12 — Snapshots written before a root's first seat are never
//! restored.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use std::collections::BTreeSet;

use ourios_config::MinerConfig;
use ourios_ingester::recovery::{RecoveryDriverError, SnapshotFate};
use ourios_ingester::template_ids::{
    HIGH_WATER_KEY, SEATED_MARKER, SnapshotTrust, TemplateIds, TemplateIdsError,
};
use ourios_miner::cluster::MinerCluster;

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

/// Every template id the node's snapshot artefacts hold.
fn snapshot_ids(node: &Node) -> BTreeSet<u64> {
    artefacts(node)
        .iter()
        .flat_map(|path| {
            let bytes = std::fs::read(path).expect("artefact");
            let state = ourios_miner::snapshot::load_snapshot(&bytes).expect("decodes");
            state
                .leaves
                .iter()
                .map(|l| l.template_id)
                .chain(state.structured_templates.iter().map(|s| s.template_id))
                .collect::<Vec<_>>()
        })
        .collect()
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
    let b_old_ids = snapshot_ids(&b);
    assert!(!b_old_ids.is_empty(), "B snapshotted templates");
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

    // And none of the discarded snapshot's leaves is live: they were never
    // restored, so a shape the snapshot held mints a fresh id above the
    // high-water rather than reviving its old one.
    let tenant = ourios_core::tenant::TenantId::new("checkout");
    assert!(
        b_running.miner.snapshot_state(&tenant).leaves.is_empty(),
        "no discarded leaf is in the miner"
    );
    let high_water = b_running.report.template_ids.high_water;
    let revived = b_running.mine("checkout", "disk sda1 is 91 percent full");
    assert!(
        revived > high_water,
        "{revived} is fresh, above {high_water}"
    );
    assert!(!b_old_ids.contains(&revived), "not its pre-RFC id");

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

    // A directory where the marker's temp file belongs makes its write
    // fail: the start dies after the removal and before the marker.
    let marker = b.snapshots.join(SEATED_MARKER);
    let marker_tmp = b.snapshots.join(format!("{SEATED_MARKER}.tmp"));
    std::fs::create_dir(&marker_tmp).expect("block the marker");
    std::fs::write(marker_tmp.join("occupied"), b"x").expect("occupy it");
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

    assert!(!marker.exists(), "no marker was written");
    std::fs::remove_dir_all(&marker_tmp).expect("unblock");
    let restarted = b.restart().expect("the next start");
    assert!(restarted.report.tenants.is_empty(), "nothing left to trust");
    assert!(marker.is_file());
}

/// Scenario RFC0059.12 — a marker that does not parse, or claims more
/// than the high-water holds, fails startup closed and never trusts the
/// snapshots.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_12_an_unusable_marker_fails_startup_closed() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (a, b) = two_pre_rfc_receivers(tmp.path()).await;
    drop(a.restart().expect("A bootstraps"));
    let snapshots_before = artefacts(&b);
    for body in [
        &b""[..],
        br#"{"version": 1, "seated_ab"#,
        b"not json",
        br#"{"version": 1, "seated_above": 99999999}"#,
    ] {
        std::fs::write(b.snapshots.join(SEATED_MARKER), body).expect("marker");
        let Err(err) = b.restart() else {
            panic!("{body:?} must fail startup");
        };
        assert!(
            matches!(
                err,
                RecoveryDriverError::TemplateIds(TemplateIdsError::MarkerInvalid { .. })
            ),
            "{body:?}: {err}"
        );
        assert_eq!(artefacts(&b), snapshots_before, "nothing is touched");
    }
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

/// Scenario RFC0059.12 — a start that saw no high-water, restored its
/// snapshots, and then finds that another start created the object before
/// its seat, fails startup: it certifies nothing and writes no marker, and
/// its restart takes the discard path.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_12_a_high_water_created_after_the_trust_read_fails_the_start() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (_, b) = two_pre_rfc_receivers(tmp.path()).await;
    let snapshots_before = artefacts(&b);
    let hooks = Hooks::default();
    hooks
        .create_after_absent_read
        .store(true, std::sync::atomic::Ordering::Release);

    let Err(err) = b.restart_over(hooks.wrap(b.store())) else {
        panic!("a start whose trust read is stale must fail");
    };
    assert!(
        matches!(
            err,
            RecoveryDriverError::TemplateIds(TemplateIdsError::BootstrapRaceLost)
        ),
        "{err}"
    );
    assert!(!b.snapshots.join(SEATED_MARKER).exists(), "no marker");
    assert_eq!(artefacts(&b), snapshots_before, "nothing is removed");

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

/// Scenario RFC0059.12 — a markerless start that saw the high-water, and
/// so discards its snapshots, never bootstraps if the object then
/// vanishes: it fails closed with nothing written.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_12_a_start_that_saw_the_high_water_never_bootstraps() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (a, b) = two_pre_rfc_receivers(tmp.path()).await;
    drop(a.restart().expect("A bootstraps"));
    let trust = SnapshotTrust::of(&b.snapshots, &b.store()).expect("trust");
    assert_eq!(trust, SnapshotTrust::PredatesHighWater);
    std::fs::remove_file(b.store.join(HIGH_WATER_KEY)).expect("the object vanishes");

    let ids = TemplateIds::new(b.store()).with_bootstrap_allowed(true);
    let mut miner = MinerCluster::new(MinerConfig::default()).with_id_reserver(ids.reserver());
    let Err(err) = ids.start(&mut miner, trust) else {
        panic!("a start that saw the object must not bootstrap");
    };
    assert!(matches!(err, TemplateIdsError::HighWaterDeleted), "{err}");
    assert_eq!(b.high_water_bytes(), None, "nothing is created");
}
