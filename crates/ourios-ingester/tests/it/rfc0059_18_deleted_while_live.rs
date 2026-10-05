//! RFC0059.18 — A high-water deleted under a live receiver is never
//! re-created.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use ourios_ingester::template_ids::{BLOCK, HIGH_WATER_KEY, mark_seated};

use crate::rfc0059_support::Node;

const TENANT: &str = "checkout";
/// Long enough for the refiller's backoff to retry several times.
const SETTLE: Duration = Duration::from_secs(2);

/// Scenario RFC0059.18 — after the object is deleted under a running
/// receiver, the refiller neither re-creates it nor reserves from one
/// that reappears; the blocks already held are spent without overlap,
/// then fresh mints fail while known templates keep attaching.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_18_a_high_water_deleted_while_live_is_never_recreated() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = Node::empty(tmp.path());
    node.put(HIGH_WATER_KEY, br#"{"reserved_through": 0}"#);
    mark_seated(&node.snapshots, 0).expect("seated");
    let mut running = node.restart().expect("recover");
    let held = 2 * BLOCK;
    assert_eq!(
        node.high_water_bytes().as_deref(),
        Some(format!(r#"{{"reserved_through":{held}}}"#).as_bytes()),
        "startup readies two blocks"
    );

    // Given the object deleted while the node holds both blocks.
    std::fs::remove_file(node.store.join(HIGH_WATER_KEY)).expect("delete the object");

    // When it mints through every held id, asking the refiller each time.
    let minted: BTreeSet<u64> = (0..held)
        .map(|i| running.mine_structured(TENANT, &format!("event.{i}")))
        .collect();
    std::thread::sleep(SETTLE);

    // Then every held id was used once and none lies past the reservation.
    assert_eq!(
        minted,
        (1..=held).collect(),
        "held blocks are spent without overlap"
    );
    // And nothing re-created the object.
    assert_eq!(
        node.high_water_bytes(),
        None,
        "a live reservation never creates it"
    );
    // And fresh mints fail while known templates attach.
    assert_eq!(running.mine_structured(TENANT, "event.fresh"), 0);
    assert_eq!(running.mine_structured(TENANT, "event.0"), 1);

    // And an object that reappears is not reserved from.
    node.put(HIGH_WATER_KEY, br#"{"reserved_through": 0}"#);
    let deadline = Instant::now() + SETTLE;
    while Instant::now() < deadline {
        assert_eq!(
            running.mine_structured(TENANT, "event.fresh"),
            0,
            "the refiller stopped for good"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        node.high_water_bytes().as_deref(),
        Some(br#"{"reserved_through": 0}"#.as_slice()),
        "the reappeared object is left alone"
    );
}
