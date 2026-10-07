//! RFC0059.14 — A seated root that finds the high-water gone fails
//! closed.
//! RFC0059.15 — A denied store call names the permission it needs.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use ourios_ingester::recovery::RecoveryDriverError;
use ourios_ingester::template_ids::{HIGH_WATER_KEY, TemplateIdsError};

use crate::rfc0059_support::{Hooks, Node};

/// Scenario RFC0059.14 — a seated root never bootstraps over a deleted
/// high-water.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_14_a_seated_root_never_bootstraps_a_deleted_high_water() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = Node::empty(tmp.path());
    node.put(HIGH_WATER_KEY, br#"{"reserved_through": 5000}"#);
    drop(node.restart().expect("the root seats"));
    std::fs::remove_file(node.store.join(HIGH_WATER_KEY)).expect("delete the object");

    // Authorised or not: a seated root never bootstraps.
    let Err(err) = node.restart_with(node.store(), true) else {
        panic!("a seated root must not bootstrap");
    };

    assert!(
        matches!(
            err,
            RecoveryDriverError::TemplateIds(TemplateIdsError::HighWaterDeleted)
        ),
        "{err}"
    );
    assert_eq!(node.high_water_bytes(), None, "nothing re-created it");
}

/// Scenario RFC0059.15 — a denied read fails startup with an error that
/// names the permission the receiver needs.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_15_a_denied_read_names_the_missing_permission() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = Node::empty(tmp.path());
    let hooks = Hooks::default();
    hooks
        .denied
        .store(true, std::sync::atomic::Ordering::Release);

    let Err(err) = node.restart_over(hooks.wrap(node.store())) else {
        panic!("a denied store must fail startup");
    };

    let message = err.to_string();
    assert!(message.contains("permission denied"), "{message}");
    assert!(message.contains("s3:GetObject"), "{message}");
}
