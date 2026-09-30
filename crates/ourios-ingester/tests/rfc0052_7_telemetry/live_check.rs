//! Every RFC 0052 event is emitted and live-checked.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use ourios_ingester::barrier::CutOutcome;
use ourios_ingester::cadence;
use ourios_ingester::housekeeping::HousekeepingTick;
use ourios_semconv as semconv;
use ourios_telemetry::live_check::{self, Checked, EventSpec};

use crate::drivers::{
    cut, drive_floor_and_latch_edges, drive_rotation_edges, housekeeper_of, rig_with, tick,
};
use crate::harness::{harness, serial};
use crate::rfc0052_barrier_support::JournalFaults;

/// The RFC 0052 log events as the registry declares them, every one of
/// which the live-check leg must see emitted.
const RFC0052_EVENTS: [EventSpec; 11] = [
    failure(semconv::EVENT_OURIOS_RECEIVER_WAL_CHECKPOINT_ERROR),
    failure(semconv::EVENT_OURIOS_RECEIVER_WAL_IDLE_ROTATION_ERROR),
    failure(semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_RETRYING),
    plain(semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_RECOVERED),
    failure(semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_TERMINAL),
    plain(semconv::EVENT_OURIOS_RECEIVER_WAL_RETAIN_FLOOR_PINNED),
    plain(semconv::EVENT_OURIOS_RECEIVER_WAL_RETAIN_FLOOR_LIFTED),
    plain(semconv::EVENT_OURIOS_RECEIVER_BARRIER_LATCHED),
    failure(semconv::EVENT_OURIOS_RECEIVER_WAL_HOUSEKEEPING_ERROR),
    failure(semconv::EVENT_OURIOS_RECEIVER_CADENCE_JOIN_ERROR),
    EventSpec {
        name: semconv::EVENT_OURIOS_RECEIVER_PUBLISH_HELD,
        required: &[],
        optional: &[semconv::OURIOS_SINK_FLUSH_TRIGGER],
    },
];

const fn failure(name: &'static str) -> EventSpec {
    EventSpec {
        name,
        required: &["error.type"],
        optional: &[],
    }
}

const fn plain(name: &'static str) -> EventSpec {
    EventSpec {
        name,
        required: &[],
        optional: &[],
    }
}

/// Every RFC 0052 event, emitted: the rotation, floor and latch edges,
/// a failed idle rotation and a failed checkpoint write.
async fn emit_every_event() {
    drive_rotation_edges().await;

    let tmp = tempfile::TempDir::new().expect("temp");
    let faults = Arc::new(JournalFaults::default());
    let rig = rig_with(tmp.path(), &faults);
    let housekeeper = housekeeper_of(&rig);
    drive_floor_and_latch_edges(&rig, &housekeeper).await;

    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = rig_with(tmp.path(), &faults);
    rig.ingest("checkout", &["user 1 logged in"]).await;
    faults.fail_idle_rotation.store(true, Ordering::Release);
    rig.sabotage_checkpoint();
    assert_eq!(
        cut(&rig, true).await,
        CutOutcome::Stamped,
        "a failed sidecar write keeps the previous mark usable (RFC0052.1)"
    );

    // A housekeeping pass that unwinds after the WAL took its plan.
    faults.panic_after_prepare.store(true, Ordering::Release);
    assert!(matches!(
        tick(&housekeeper_of(&rig)).await,
        HousekeepingTick::Panicked
    ));

    // A cadence task whose join is a panic, read at shutdown.
    let joined = tokio::spawn(async { panic!("injected cadence-task panic") })
        .await
        .map(drop);
    assert!(cadence::read_join(&rig.epochs, "barrier", joined));

    // A later drain held behind an earlier one whose template events are
    // still unwritten.
    rig.ingest("checkout", &["cache warmed in 5 ms"]).await;
    let earlier = rig.publish.drain_all();
    rig.ingest("checkout", &["cache warmed in 5 ms"]).await;
    let later = rig.publish.drain_all();
    assert!(
        !rig.publish.write_ordered(later, "age"),
        "the later drain is held"
    );
    assert!(rig.publish.write_ordered(earlier, "age"));
}

/// Scenario RFC0052.7 — `weaver registry live-check` sees every new event emitted.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
///
/// Every emitted RFC 0052 event is checked against its registry name and
/// the attributes the registry declares for it — so a tracing field left
/// over from before the naming (`error = %e`) fails here as weaver would
/// fail it — and, where CI's `live-check` job configures weaver
/// (`OURIOS_LIVE_CHECK_WEAVER`, `OURIOS_LIVE_CHECK_REGISTRY`), through
/// `weaver registry live-check` itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0052_7_live_check_covers_every_new_log_event() {
    let _serial = serial().await;
    emit_every_event().await;
    let events = harness().events();
    let checked = live_check::live_check(&events, &RFC0052_EVENTS)
        .expect("every RFC 0052 event is emitted and registry-conformant");

    // The failure events carry the error.type values the registry lists.
    for (name, class) in [
        (semconv::EVENT_OURIOS_RECEIVER_WAL_IDLE_ROTATION_ERROR, "io"),
        (semconv::EVENT_OURIOS_RECEIVER_WAL_CHECKPOINT_ERROR, "io"),
        (
            semconv::EVENT_OURIOS_RECEIVER_WAL_HOUSEKEEPING_ERROR,
            "cadence_panic",
        ),
        (semconv::EVENT_OURIOS_RECEIVER_CADENCE_JOIN_ERROR, "panic"),
    ] {
        assert!(
            events
                .iter()
                .any(|e| e.name == name && e.error_type() == Some(class)),
            "{name} with error.type {class}"
        );
    }
    if checked == Checked::SpecOnly {
        eprintln!(
            "RFC0052.7: weaver is not configured here; checked each event against its spec only"
        );
    }
}
