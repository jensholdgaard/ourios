//! RFC0059.2 — A replaced local root never re-issues a published id.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use crate::rfc0059_support::{Node, cut_and_reclaim, publish};

/// Scenario RFC0059.2 — an empty WAL root over an intact store allocates
/// above the high-water.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_2_an_empty_root_over_an_intact_store_allocates_above_n() {
    // Given a published store whose high-water a first restart seated.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = Node::rig(tmp.path());
    publish(
        &rig,
        "checkout",
        &["user alice logged in"],
        &["checkout.paid"],
    )
    .await;
    cut_and_reclaim(&rig).await;
    let node = Node::stop(rig, tmp.path());
    let first = node.restart().expect("first restart seats the store");
    let high_water = first.report.template_ids.high_water;
    drop(first);
    let issued = node.issued();

    // When the local root is replaced by an empty one.
    std::fs::remove_dir_all(&node.wal).expect("lose the root");
    std::fs::create_dir_all(&node.wal).expect("an empty root");
    let mut restarted = node.restart().expect("recover");

    // Then every id it allocates is above N and none is published.
    assert!(restarted.report.tenants.is_empty(), "nothing to restore");
    let minted = [
        restarted.mine("checkout", "order 7 shipped to berlin"),
        restarted.mine("checkout", "user alice logged in"),
        restarted.mine_structured("checkout", "checkout.paid"),
    ];
    for id in minted {
        assert!(id > high_water, "{id} is above {high_water}");
        assert!(!issued.contains(&id), "{id} is already published");
    }
}
