//! RFC0059.16 — Bootstrapping over existing data needs authorisation.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use ourios_ingester::recovery::RecoveryDriverError;
use ourios_ingester::template_ids::{SEATED_MARKER, TemplateIdsError};

use crate::rfc0059_support::{Node, cut_and_reclaim, publish};

/// A store with published data and no high-water, and a root that never
/// seated: what a replaced root finds after the object was deleted, or a
/// pre-RFC store at its upgrade.
async fn data_without_high_water(tmp: &std::path::Path) -> Node {
    let rig = Node::rig(tmp);
    publish(&rig, "checkout", &["user alice logged in"], &[]).await;
    cut_and_reclaim(&rig).await;
    let node = Node::stop(rig, tmp);
    std::fs::remove_dir_all(&node.wal).expect("a markerless root");
    std::fs::create_dir_all(&node.wal).expect("an empty root");
    node
}

/// Scenario RFC0059.16 — a genuinely new store bootstraps without
/// authorisation.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_16_a_new_store_bootstraps_without_authorisation() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = Node::empty(tmp.path());

    let restarted = node
        .restart_with(node.store(), false)
        .expect("a new store needs no authorisation");

    assert!(restarted.report.template_ids.bootstrapped);
    assert_eq!(restarted.report.template_ids.high_water, 0);
}

/// Scenario RFC0059.16 — a store that holds data fails closed without
/// authorisation, explains the upgrade step, and writes nothing.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_16_data_without_authorisation_fails_closed() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = data_without_high_water(tmp.path()).await;

    let Err(err) = node.restart_with(node.store(), false) else {
        panic!("an unauthorised bootstrap over data must fail");
    };

    assert!(
        matches!(
            err,
            RecoveryDriverError::TemplateIds(TemplateIdsError::BootstrapNotAuthorized)
        ),
        "{err}"
    );
    assert!(
        err.to_string()
            .contains("OURIOS_TEMPLATE_IDS_ALLOW_BOOTSTRAP"),
        "{err}"
    );
    assert_eq!(node.high_water_bytes(), None, "nothing is written");
    assert!(!node.snapshots.join(SEATED_MARKER).exists());
}

/// Scenario RFC0059.16 — with authorisation, a markerless root bootstraps
/// over the data.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_16_an_authorised_markerless_root_bootstraps_over_data() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = data_without_high_water(tmp.path()).await;
    let issued = node.issued();

    let restarted = node
        .restart_with(node.store(), true)
        .expect("the authorised upgrade start");

    assert!(restarted.report.template_ids.bootstrapped);
    let floor = restarted.report.template_ids.high_water;
    assert!(issued.iter().all(|id| *id <= floor), "{issued:?} ≤ {floor}");
    assert!(node.snapshots.join(SEATED_MARKER).is_file());
}
