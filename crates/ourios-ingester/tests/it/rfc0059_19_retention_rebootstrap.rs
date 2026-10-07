//! RFC0059.19 — Retention or erasure followed by a re-bootstrap never
//! reissues an id still bound by stored data or audit.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use ourios_ingester::template_ids::{HIGH_WATER_KEY, SEATED_MARKER};

use crate::rfc0059_support::{Node, cut_and_reclaim, publish};

const KEPT: &str = "search";
const EXPIRED: &str = "checkout";

/// Scenario RFC0059.19 — after one tenant's data and audit age out and
/// the documented re-bootstrap runs, the floor may drop below the
/// expired ids, but no id a stored row or event still binds is issued
/// again.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_19_a_rebootstrap_after_retention_never_reissues_a_bound_id() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = Node::rig(tmp.path());
    publish(&rig, KEPT, &["query 7 served"], &["search.hit"]).await;
    publish(
        &rig,
        EXPIRED,
        &["user alice logged in", "disk sda1 is 91 percent full"],
        &["checkout.paid"],
    )
    .await;
    cut_and_reclaim(&rig).await;
    let node = Node::stop(rig, tmp.path());
    drop(node.restart().expect("the upgrade bootstraps and seats"));
    let issued = node.issued();

    // Given retention removes one tenant's data and audit.
    for stream in ["data", "audit"] {
        std::fs::remove_dir_all(node.store.join(stream).join(format!("tenant_id={EXPIRED}")))
            .expect("the tenant ages out");
    }
    let bound = node.issued();
    assert!(
        bound.len() < issued.len(),
        "some ids are no longer bound: {bound:?} of {issued:?}"
    );

    // When the documented recovery re-bootstraps.
    std::fs::remove_file(node.store.join(HIGH_WATER_KEY)).expect("remove the object");
    std::fs::remove_file(node.snapshots.join(SEATED_MARKER)).expect("remove the marker");
    let mut restarted = node
        .restart_with(node.store(), true)
        .expect("the authorised re-bootstrap");

    // Then the floor covers every bound id, and no new id is one of them.
    let seated = restarted.report.template_ids.high_water;
    let highest = bound.iter().max().copied().expect("bound ids");
    assert!(
        seated >= highest,
        "seated at {seated}, below bound {highest}"
    );
    for line in [
        "cache warmed in 12 ms",
        "user bob logged out",
        "queue drained",
    ] {
        let id = restarted.mine(KEPT, line);
        assert!(!bound.contains(&id), "{id} is still bound by stored data");
    }
}
