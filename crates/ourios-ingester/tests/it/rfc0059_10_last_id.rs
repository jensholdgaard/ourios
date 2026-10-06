//! RFC0059.10 — `i64::MAX` is the last id, issued from a shortened final
//! block.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use ourios_ingester::template_ids::{BLOCK, HIGH_WATER_KEY, mark_seated};
use ourios_miner::cluster::MAX_TEMPLATE_ID;

use crate::rfc0059_support::Node;

const TENANT: &str = "checkout";

/// Scenario RFC0059.10 — from a high-water whose distance to `i64::MAX`
/// is not a multiple of the block size, every id up to and including
/// `i64::MAX` is issued, and only then do fresh mints fail.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_10_an_unaligned_high_water_still_issues_i64_max_last() {
    let left = BLOCK + 234;
    assert_ne!(left % BLOCK, 0, "the remainder is unaligned");
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = Node::empty(tmp.path());
    let start = MAX_TEMPLATE_ID - left;
    node.put(
        HIGH_WATER_KEY,
        format!(r#"{{"reserved_through": {start}}}"#).as_bytes(),
    );
    mark_seated(&node.snapshots, 0).expect("seated");
    let mut running = node.restart().expect("recover");

    let ids: Vec<u64> = (0..left)
        .map(|i| running.mine_structured(TENANT, &format!("event.{i}")))
        .collect();

    assert_eq!(ids, ((start + 1)..=MAX_TEMPLATE_ID).collect::<Vec<_>>());
    assert_eq!(
        running.mine_structured(TENANT, "event.past"),
        0,
        "nothing is left above i64::MAX"
    );
    assert_eq!(
        node.high_water_bytes().as_deref(),
        Some(format!(r#"{{"reserved_through":{MAX_TEMPLATE_ID}}}"#).as_bytes()),
        "the final block ends exactly at i64::MAX"
    );
}
