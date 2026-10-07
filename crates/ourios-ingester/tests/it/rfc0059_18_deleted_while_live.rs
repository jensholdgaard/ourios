//! RFC0059.18 — A high-water deleted under a live receiver is never
//! re-created.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use ourios_ingester::recovery::RecoveryDriverError;
use ourios_ingester::template_ids::{
    BLOCK, HIGH_WATER_KEY, SEATED_MARKER, TemplateIdsError, mark_seated,
};

use crate::rfc0059_support::{Node, cut_and_reclaim, publish};

const TENANT: &str = "checkout";
/// Long enough for the refiller's backoff to retry several times.
const SETTLE: Duration = Duration::from_secs(2);

/// Scenario RFC0059.18 — after the object is deleted under a running
/// receiver, the refiller neither re-creates it nor reserves from one
/// that reappears; the blocks already held are spent without overlap,
/// then fresh mints fail while known templates keep attaching; and a
/// restart over the stale copy fails closed as a rollback.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_18_a_high_water_deleted_while_live_is_never_recreated() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = Node::empty(tmp.path());
    node.put(HIGH_WATER_KEY, br#"{"reserved_through": 0}"#);
    mark_seated(&node.snapshots, 0).expect("seated");
    let mut running = node.restart().expect("recover");
    let held = 3 * BLOCK;
    assert_eq!(
        node.high_water_bytes().as_deref(),
        Some(format!(r#"{{"reserved_through":{held}}}"#).as_bytes()),
        "startup holds a current block and two ready blocks"
    );

    // Given the object deleted while the node holds all three blocks.
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

    // And a restart over that stale copy fails closed, naming a rollback.
    drop(running);
    let Err(err) = node.restart() else {
        panic!("a restart over a rolled-back high-water must fail");
    };
    assert!(
        matches!(
            err,
            RecoveryDriverError::TemplateIds(TemplateIdsError::HighWaterRolledBack {
                seen,
                found: 0
            }) if seen == held
        ),
        "{err}"
    );
    assert!(err.to_string().contains("rolled back"), "{err}");
    assert_eq!(
        node.high_water_bytes().as_deref(),
        Some(br#"{"reserved_through": 0}"#.as_slice()),
        "the failed restart writes nothing"
    );
}

/// Scenario RFC0059.18 — the documented recovery from a rolled-back
/// high-water (RFC 0059 §3.1): with every receiver stopped, remove the
/// object and every root's seated marker, then start one replica
/// authorised to bootstrap. It seats above every published id, and every
/// id it mints is fresh.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_18_the_documented_recovery_reseats_above_every_published_id() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = Node::rig(tmp.path());
    publish(
        &rig,
        TENANT,
        &["user alice logged in", "disk sda1 is full"],
        &[],
    )
    .await;
    cut_and_reclaim(&rig).await;
    let node = Node::stop(rig, tmp.path());
    drop(node.restart().expect("the upgrade bootstraps and seats"));
    let published = node.issued();
    node.put(HIGH_WATER_KEY, br#"{"reserved_through": 0}"#);
    assert!(node.restart().is_err(), "the stale copy is refused");

    // When the operator removes the object and the marker, then starts
    // one replica authorised to bootstrap.
    std::fs::remove_file(node.store.join(HIGH_WATER_KEY)).expect("remove the object");
    std::fs::remove_file(node.snapshots.join(SEATED_MARKER)).expect("remove the marker");
    let mut restarted = node
        .restart_with(node.store(), true)
        .expect("the authorised re-bootstrap");

    // Then it seats above every published id, and mints only fresh ones.
    let seated = restarted.report.template_ids.high_water;
    let highest = published.iter().max().copied().expect("ids were published");
    assert!(
        seated >= highest,
        "seated at {seated}, below published {highest}"
    );
    let fresh = restarted.mine(TENANT, "cache warmed in 12 ms");
    assert!(fresh > highest && !published.contains(&fresh));
}

/// Scenario RFC0059.18 — a high-water deleted between a reservation's read
/// and its compare-and-swap, on a backend that answers that write
/// not-found, stops the refiller for good: an older copy restored during
/// what would have been its backoff is never read or reserved from, and no
/// block is handed out once the held ones are gone.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_18_a_deletion_between_read_and_write_stops_the_refiller() {
    use std::sync::atomic::Ordering;

    use ourios_config::MinerConfig;
    use ourios_ingester::template_ids::{self, SnapshotTrust, TemplateIds};
    use ourios_miner::cluster::MinerCluster;
    use ourios_parquet::Store;

    use crate::rfc0059_support::Hooks;

    // Given a running receiver whose refiller is about to reserve.
    let backend = Store::in_memory();
    backend
        .put_blocking(HIGH_WATER_KEY, br#"{"reserved_through": 5000}"#.to_vec())
        .expect("seed");
    let hooks = Hooks::default();
    let ids = TemplateIds::new(hooks.wrap(backend.clone()));
    let mut miner = MinerCluster::new(MinerConfig::default()).with_id_reserver(ids.reserver());
    ids.start(
        &mut miner,
        SnapshotTrust::Seated {
            max_reserved_seen: 0,
        },
    )
    .expect("start");
    ids.finish_replay().expect("the refiller");
    let present = || template_ids::read(&backend).expect("read").is_some();
    let mut reserver = ids.reserver();

    // When the object is deleted between the refill's read and its write.
    hooks.delete_before_next_put.store(true, Ordering::Release);
    let held = reserver.reserve(0).expect("a held block");
    let deadline = Instant::now() + Duration::from_secs(10);
    while present() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!present(), "the refill reached its write");

    // And an older copy is restored inside the first backoff.
    backend
        .put_blocking(HIGH_WATER_KEY, br#"{"reserved_through": 1000}"#.to_vec())
        .expect("restore a stale copy");
    let reads = || hooks.high_water_reads.load(Ordering::Acquire);
    let reads_after_restore = reads();
    let last = reserver
        .reserve(held.through())
        .expect("the other held block");
    std::thread::sleep(SETTLE);

    // Then the refiller never read or wrote the restored object again,
    // and no further block is handed out.
    assert_eq!(reads(), reads_after_restore, "the refiller stopped");
    assert_eq!(
        template_ids::read(&backend)
            .expect("read")
            .expect("restored")
            .reserved_through,
        1000,
        "nothing reserved from the stale copy"
    );
    assert!(held.after() >= 5000 && last.after() >= 5000);
    assert!(
        reserver.reserve(last.through()).is_err(),
        "no block below another replica's comes from the stale copy"
    );
}
