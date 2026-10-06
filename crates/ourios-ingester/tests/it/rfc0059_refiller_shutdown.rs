//! RFC 0059 §3.3 — a receiver's shutdown waits out a reservation in
//! flight, so the next receiver over the same roots never races a stale
//! write that could lower the high-water or the seated marker beneath it
//! (the §3.1 rollback check).
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §3.3.

use std::time::Duration;

use ourios_config::MinerConfig;
use ourios_ingester::recovery;
use ourios_ingester::template_ids::{self, SEATED_MARKER, TemplateIds};
use ourios_miner::cluster::MinerCluster;
use ourios_wal::{Wal, WalConfig};

use crate::rfc0052_barrier_support::wal_config;
use crate::rfc0059_support::{Hooks, Node};

/// The refiller parks in a reservation's write, after its read. Shutdown
/// must not return until that reservation is done, and must leave the
/// marker as it was: the next receiver then starts over roots no old
/// worker can still write.
#[test]
fn shutdown_waits_for_a_reservation_in_flight_before_the_roots_are_free() {
    // Given a started receiver whose refiller is parked mid-reservation.
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = Node::empty(tmp.path());
    let hooks = Hooks::default();
    let ids = TemplateIds::new(hooks.wrap(node.store()));
    let mut miner = MinerCluster::new(MinerConfig::default()).with_id_reserver(ids.reserver());
    let mut wal = Wal::open(WalConfig {
        segment_age_secs: 1,
        ..wal_config(&node.wal)
    })
    .expect("open the WAL");
    recovery::recover(&mut wal, &node.snapshots, &mut miner, &ids).expect("start");
    let high_water = || {
        template_ids::read(&node.store())
            .expect("read")
            .expect("present")
            .reserved_through
    };
    let marker = || std::fs::read(node.snapshots.join(SEATED_MARKER)).expect("the marker");
    let marker_at_start = marker();
    hooks.high_water_put_gate.arm();
    ids.reserver().reserve(0).expect("a ready block");
    hooks.high_water_put_gate.wait_parked();

    // When the receiver shuts down.
    let shutting_down = std::thread::spawn(move || {
        let stopped = ids.shutdown();
        (ids, stopped)
    });
    std::thread::sleep(Duration::from_millis(300));

    // Then shutdown waits for the reservation in flight.
    assert!(
        !shutting_down.is_finished(),
        "shutdown returned while a reservation was still writing"
    );
    hooks.high_water_put_gate.release();
    let (ids, outcome) = shutting_down.join().expect("the shutdown thread");
    outcome.expect("the refiller joined");
    let after_shutdown = high_water();

    // And the stopped refiller recorded nothing in the marker.
    assert_eq!(
        marker(),
        marker_at_start,
        "no block recorded after shutdown"
    );

    // And the next receiver over the same roots seats, and nothing lowers
    // the high-water beneath it.
    drop((ids, miner, wal));
    let restarted = node.restart().expect("the next start seats");
    assert!(restarted.report.template_ids.high_water >= after_shutdown);
    assert!(high_water() >= after_shutdown);
}
