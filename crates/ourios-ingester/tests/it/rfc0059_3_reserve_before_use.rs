//! RFC0059.3 — Ids are reserved before they are used; a crash only skips.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use ourios_ingester::template_ids::{self, HIGH_WATER_KEY};

use crate::rfc0059_support::Node;

const N: u64 = 100;

fn seeded(tmp: &std::path::Path) -> Node {
    let node = Node::empty(tmp);
    node.put(
        HIGH_WATER_KEY,
        format!(r#"{{"reserved_through": {N}}}"#).as_bytes(),
    );
    node
}

fn high_water(node: &Node) -> u64 {
    template_ids::read(&node.store())
        .expect("read")
        .expect("present")
        .reserved_through
}

/// Scenario RFC0059.3 — the high-water covers an id before it is
/// allocated.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_3_the_high_water_covers_an_id_before_it_is_allocated() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = seeded(tmp.path());
    let mut restarted = node.restart().expect("recover");

    let id = restarted.mine("checkout", "user alice logged in");

    assert!(id > N, "{id} is above the high-water read at start");
    assert!(
        high_water(&node) >= id,
        "the store already covered {id} when it was allocated"
    );
}

/// Scenario RFC0059.3 — a crash after a reservation and before any use
/// only skips the reserved ids.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_3_a_kill_after_reservation_skips_the_block() {
    // Given a start that reserved blocks and used none: dropping it is
    // the state a kill leaves, since every reservation is durable before
    // the start returns.
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = seeded(tmp.path());
    drop(node.restart().expect("first start"));
    let reserved = high_water(&node);
    assert!(reserved > N, "the start reserved ahead");

    // When the node restarts and allocates.
    let mut restarted = node.restart().expect("recover");
    let id = restarted.mine("checkout", "user alice logged in");

    // Then the first id is above every block the first start reserved.
    assert!(
        id > reserved,
        "{id} skips the unused block below {reserved}"
    );
}
