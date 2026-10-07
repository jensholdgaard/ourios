//! RFC0059.6 — The bootstrap floor is provable and written once.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use ourios_ingester::recovery::RecoveryDriverError;
use ourios_ingester::template_ids::{SEATED_MARKER, TemplateIdsError};

use crate::rfc0059_support::{Fixture, Node, audit_bindings, cut_and_reclaim, publish, rows};

/// A published store whose highest ids are structured templates, which
/// emit no audit event, with one tenant's audit stream lost entirely,
/// and no local root: only the data rows can bound the ids.
async fn store_only_rows_can_bound(tmp: &std::path::Path) -> (Node, u64, u64) {
    let rig = Node::rig(tmp);
    publish(&rig, "search", &["query 7 served"], &[]).await;
    publish(
        &rig,
        "checkout",
        &["user alice logged in"],
        &["checkout.paid"],
    )
    .await;
    cut_and_reclaim(&rig).await;
    let node = Node::stop(rig, tmp);
    std::fs::remove_dir_all(node.store.join("audit").join("tenant_id=checkout"))
        .expect("drop one tenant's audit events");
    std::fs::remove_dir_all(&node.wal).expect("no local root");
    std::fs::create_dir_all(&node.wal).expect("an empty root");
    let data_max = rows(&node.store.join("data"))
        .iter()
        .map(|r| r.template_id)
        .max()
        .expect("rows");
    let audit_max = audit_bindings(&node.store.join("audit"))
        .keys()
        .map(|(id, _)| *id)
        .max()
        .unwrap_or(0);
    assert!(
        data_max > audit_max,
        "the audit stream cannot bound the ids"
    );
    (node, data_max, audit_max)
}

/// Scenario RFC0059.6 — the floor is the max over data, audit and
/// restored snapshots, with no margin.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_6_the_floor_is_the_max_over_data_audit_and_snapshots() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (node, data_max, _) = store_only_rows_can_bound(tmp.path()).await;

    let mut restarted = node.restart().expect("recover");

    assert!(restarted.report.template_ids.bootstrapped);
    assert_eq!(
        restarted.report.template_ids.high_water, data_max,
        "no margin"
    );
    let fresh = restarted.mine("checkout", "order 7 shipped to berlin");
    assert!(fresh > data_max, "{fresh} is above the floor {data_max}");
}

/// Scenario RFC0059.6 — the bootstrap happens at most once per creation of the
/// high-water: a later start reads the object it wrote.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_6_the_bootstrap_is_logged_once() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (node, _, _) = store_only_rows_can_bound(tmp.path()).await;

    let first = node.restart().expect("bootstrap");
    assert!(
        first.report.template_ids.bootstrapped,
        "the first start bootstraps"
    );
    drop(first);
    let second = node.restart().expect("restart");

    assert!(
        !second.report.template_ids.bootstrapped,
        "the bootstrapped event is the first start's alone"
    );
}

/// Scenario RFC0059.6 — a start that fails mid-scan writes nothing, and
/// the next start redoes the scan and writes once.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_6_a_kill_mid_scan_writes_nothing_and_the_next_start_redoes_it() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (node, data_max, _) = store_only_rows_can_bound(tmp.path()).await;
    let torn = node
        .store
        .join("data")
        .join("tenant_id=search")
        .join("torn.parquet");
    std::fs::write(&torn, b"not parquet").expect("a file the scan cannot read");

    let Err(err) = node.restart() else {
        panic!("the scan must fail");
    };
    assert!(
        matches!(
            err,
            RecoveryDriverError::TemplateIds(TemplateIdsError::Scan(_))
        ),
        "{err}"
    );
    assert!(err.to_string().contains("bootstrap scan"), "{err}");
    assert_eq!(node.high_water_bytes(), None, "nothing is written mid-scan");
    assert!(
        !node.snapshots.join(SEATED_MARKER).exists(),
        "nor the marker"
    );

    std::fs::remove_file(&torn).expect("the next start can read it");
    let restarted = node.restart().expect("recover");
    assert!(restarted.report.template_ids.bootstrapped);
    assert_eq!(restarted.report.template_ids.high_water, data_max);
}

/// Scenario RFC0059.6 — a receiver process killed by `SIGKILL` mid-scan leaves
/// neither the object nor the marker, and the next start redoes the scan
/// and writes once.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_6_a_sigkill_mid_scan_writes_nothing_and_the_next_start_redoes_it() {
    // Given a receiver process parked inside the bootstrap scan, in a
    // data file's footer read.
    let tmp = tempfile::TempDir::new().expect("temp");
    let (node, data_max, _) = store_only_rows_can_bound(tmp.path()).await;
    let mut fixture = Fixture::spawn("scan", &node);
    let scanning = fixture.reached("SCANNING");

    // When it is SIGKILLed there.
    fixture.kill();

    // Then nothing was written.
    assert_eq!(
        node.high_water_bytes(),
        None,
        "no object after a kill at {scanning:?}"
    );
    assert!(
        !node.snapshots.join(SEATED_MARKER).exists(),
        "nor the marker"
    );

    // And the next start redoes the scan and writes once.
    let restarted = node.restart().expect("recover");
    assert!(restarted.report.template_ids.bootstrapped);
    assert_eq!(restarted.report.template_ids.high_water, data_max);
}

/// Scenario RFC0059.6 — a listing the scan cannot complete fails startup
/// closed as a scan failure, and writes neither the object nor the
/// marker.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_6_a_failed_listing_fails_the_scan_closed() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::TempDir::new().expect("temp");
    let (node, _, _) = store_only_rows_can_bound(tmp.path()).await;
    let partition = node.store.join("data").join("tenant_id=search");
    let set = |mode| {
        std::fs::set_permissions(&partition, std::fs::Permissions::from_mode(mode)).expect("chmod");
    };
    set(0o000);

    let result = node.restart();
    set(0o755);

    let Err(err) = result else {
        panic!("an unlistable partition must fail the scan");
    };
    assert!(
        matches!(
            &err,
            RecoveryDriverError::TemplateIds(e @ TemplateIdsError::Scan(_))
                if e.error_type() == "scan"
        ),
        "{err}"
    );
    assert_eq!(node.high_water_bytes(), None, "no object is written");
    assert!(
        !node.snapshots.join(SEATED_MARKER).exists(),
        "nor the marker"
    );
}
