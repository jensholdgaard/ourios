//! RFC0059.4 — An exhausted range fails fresh mints without blocking
//! ingest.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use std::time::{Duration, Instant};

use ourios_ingester::template_ids::{BLOCK, HIGH_WATER_KEY, mark_seated};

use crate::rfc0059_support::{Hooks, Node};

const TENANT: &str = "checkout";

/// Scenario RFC0059.4 — with every reserved block used and the store
/// down, fresh templates fail parse with their body while known ones
/// attach, and allocation resumes once the store is back. That the miner
/// never calls the store itself is the reserver's unit test
/// (`template_ids::reserver`).
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_4_an_exhausted_range_fails_fresh_mints_and_keeps_matches_flowing() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = Node::empty(tmp.path());
    node.put(HIGH_WATER_KEY, br#"{"reserved_through": 0}"#);
    mark_seated(&node.snapshots, 0).expect("seated");
    let hooks = Hooks::default();
    let mut running = node
        .restart_over(hooks.wrap(node.store()))
        .expect("recover");

    // Given startup holding a current block and two ready ones.
    assert_eq!(
        node.high_water_bytes().as_deref(),
        Some(format!(r#"{{"reserved_through":{}}}"#, 3 * BLOCK).as_bytes()),
        "startup reserves the current block and two ready blocks"
    );
    // And the store down, with all three blocks drained.
    hooks.set_down(true);
    let first = running.mine_structured(TENANT, "event.0");
    for i in 1..3 * BLOCK {
        assert_ne!(running.mine_structured(TENANT, &format!("event.{i}")), 0);
    }
    drop(running.records.drain());

    // When a fresh template and a known one arrive.
    let fresh = running.mine_structured(TENANT, "event.fresh");
    let known = running.mine_structured(TENANT, "event.0");

    // Then the fresh one has no id but keeps its body and counts, and the
    // known one attaches.
    assert_eq!(fresh, 0, "no id it cannot prove unique");
    assert_eq!(known, first, "existing templates keep flowing");
    let rows = running.records.drain();
    assert!(rows[0].body.is_some(), "the body is kept");
    assert_eq!(running.miner.parse_failures_total(), 1);

    // And once the store is back, the refiller lands and allocation
    // resumes.
    hooks.set_down(false);
    let deadline = Instant::now() + Duration::from_secs(30);
    let resumed = loop {
        match running.mine_structured(TENANT, "event.fresh") {
            0 if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            id => break id,
        }
    };
    assert!(resumed > 3 * BLOCK, "{resumed} comes from a new block");
}

/// Scenario RFC0059.4 — while the store is down, the refiller retries on
/// its own backoff however many fresh mints fail and ask it to refill:
/// the requests coalesce and never cut a backoff short.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_4_failed_mints_never_bypass_the_refill_backoff() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = Node::empty(tmp.path());
    node.put(HIGH_WATER_KEY, br#"{"reserved_through": 0}"#);
    mark_seated(&node.snapshots, 0).expect("seated");
    let hooks = Hooks::default();
    let mut running = node
        .restart_over(hooks.wrap(node.store()))
        .expect("recover");
    hooks.set_down(true);
    for i in 0..3 * BLOCK {
        assert_ne!(running.mine_structured(TENANT, &format!("event.{i}")), 0);
    }

    let reads = || {
        hooks
            .high_water_reads
            .load(std::sync::atomic::Ordering::Acquire)
    };
    let before = reads();
    let window = Instant::now() + Duration::from_millis(800);
    let mut failed = 0;
    while Instant::now() < window {
        assert_eq!(
            running.mine_structured(TENANT, &format!("fresh.{failed}")),
            0
        );
        failed += 1;
    }

    assert!(
        failed > 100,
        "{failed} failed mints each asked for a refill"
    );
    let retried = reads() - before;
    assert!(
        retried <= 6,
        "{retried} reservation attempts in 800 ms: the backoff was bypassed"
    );
}
