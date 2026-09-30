//! Each transition emits its registry event exactly once.

use std::sync::Arc;

use ourios_ingester::cadence;
use ourios_semconv as semconv;
use ourios_telemetry::live_check::Event;

use crate::drivers::{drive_floor_and_latch_edges, drive_rotation_edges, housekeeper_of, tick};
use crate::harness::{harness, serial};
use crate::rfc0052_barrier_support::BarrierRig;

/// Scenario RFC0052.7 — each transition emits its named event exactly once.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0052_7_each_transition_emits_its_registry_event_once() {
    let _serial = serial().await;
    let harness = harness();

    // When the rotation state is driven through the append path.
    drive_rotation_edges().await;
    let rotation: Vec<Event> = harness
        .events()
        .into_iter()
        .filter(|e| e.name.starts_with("ourios.receiver.wal.rotation."))
        .collect();

    // Then each edge is one event with its state, and entering terminal
    // is one event with no leave event after it.
    let observed: Vec<(&str, Option<&str>)> =
        rotation.iter().map(|e| (e.name, e.error_type())).collect();
    assert_eq!(
        observed,
        [
            (
                semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_RETRYING,
                Some("create")
            ),
            (semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_RECOVERED, None),
            (
                semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_RETRYING,
                Some("rename")
            ),
            (
                semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_TERMINAL,
                Some("rename")
            ),
        ],
    );

    // When the floor is pinned and lifted, and the latch is set.
    harness.reset_events();
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = Arc::new(BarrierRig::new(tmp.path()));
    let housekeeper = housekeeper_of(&rig);
    drive_floor_and_latch_edges(&rig, &housekeeper).await;

    // Then each emits exactly one event, in the order it happened.
    let names: Vec<&str> = harness
        .events()
        .into_iter()
        .map(|e| e.name)
        .filter(|name| {
            [
                semconv::EVENT_OURIOS_RECEIVER_WAL_RETAIN_FLOOR_PINNED,
                semconv::EVENT_OURIOS_RECEIVER_WAL_RETAIN_FLOOR_LIFTED,
                semconv::EVENT_OURIOS_RECEIVER_BARRIER_LATCHED,
            ]
            .contains(name)
        })
        .collect();
    assert_eq!(
        names,
        [
            semconv::EVENT_OURIOS_RECEIVER_WAL_RETAIN_FLOOR_PINNED,
            semconv::EVENT_OURIOS_RECEIVER_WAL_RETAIN_FLOOR_LIFTED,
            semconv::EVENT_OURIOS_RECEIVER_BARRIER_LATCHED,
        ],
    );
}

/// Scenario RFC0052.7 — a latch set at shutdown, after the timer is
/// joined, still emits its event exactly once.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §3.5.
///
/// The receiver's shutdown joins the housekeeping task and only then
/// reads the cadence joins, where a `JoinError` sets the latch, so the
/// timer's own tick never sees it; shutdown's last step is
/// `Housekeeper::observe_state`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0052_7_a_latch_set_at_shutdown_emits_its_event_once() {
    let _serial = serial().await;
    let harness = harness();
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = Arc::new(BarrierRig::new(tmp.path()));
    let housekeeper = housekeeper_of(&rig);
    rig.ingest("checkout", &["user 1 logged in"]).await;
    tick(&housekeeper).await;

    let joined = tokio::spawn(async { panic!("injected cadence-task panic") })
        .await
        .map(drop);
    assert!(cadence::read_join(&rig.epochs, "barrier", joined));
    let latched = |events: &[Event]| {
        events
            .iter()
            .filter(|e| e.name == semconv::EVENT_OURIOS_RECEIVER_BARRIER_LATCHED)
            .count()
    };
    assert_eq!(latched(&harness.events()), 0, "no tick has run since");

    housekeeper.observe_state();
    housekeeper.observe_state();
    assert_eq!(
        latched(&harness.events()),
        1,
        "the shutdown observation emits the latch once"
    );
}
