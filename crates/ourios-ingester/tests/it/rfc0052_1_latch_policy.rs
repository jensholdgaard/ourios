//! RFC0052.1 — What a latched node refuses, and what it settles on the
//! way out.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! The checkpoint half of the criterion is
//! [`crate::rfc0052_1_checkpoint_policy`]; these legs are about the node
//! that can no longer stamp at all. Stubs are `#[ignore]`d so the
//! default run stays green while the RFC is red; each names the green
//! slice that discharges it.

use std::time::Duration;

use ourios_ingester::barrier::CutOutcome;

use crate::rfc0052_barrier_support::{BarrierRig, wal_config};

/// Scenario RFC0052.1 — the `cadence_failed` latch refuses every barrier until restart.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_1_latched_epoch_refuses_checkpoint_and_snapshot_until_restart() {
    // Given an acknowledged frame and a latch holding the epoch the next
    // cut will take.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    rig.ingest("checkout", &["user 1 logged in"]).await;
    // Quiesced here rather than left to the capture's own quiesce: with
    // the latch checked before the cut there is no capture to do it, and
    // the buffered-records assertion below has to be about a record that
    // really is in the sink's buffers.
    rig.pipeline.quiesce_encodes();
    assert_eq!(rig.sink.buffered_records(), 1, "the record is buffered");
    rig.epochs.report(rig.epochs.current());
    let latched_at = rig.epochs.current();

    // When however many timer passes run.
    for _ in 0..3 {
        assert_eq!(
            rig.barrier.tick(&rig.pipeline, false),
            CutOutcome::Latched,
            "a latch at or below the cut's epoch refuses it",
        );
    }

    // Then the barrier neither checkpoints nor snapshots.
    assert_eq!(rig.commits.last_checkpoint(), None, "no checkpoint");
    assert!(rig.snapshots().is_empty(), "and no snapshot was installed");
    assert_eq!(
        rig.sink.buffered_records(),
        1,
        "the records stay where a later cut — or a restart's replay — finds them",
    );
    assert_eq!(
        rig.epochs.current(),
        latched_at,
        "and no cut was taken at all: a latched node stands still rather than \
         quiescing, rotating and draining once per tick for a cut that cannot stamp",
    );

    // And a *failed publish* refuses only cuts captured before its
    // requeue: the same latch word carries the scope, so a publish
    // registered after a cut fails no cut at or below it.
    let fresh = tempfile::TempDir::new().expect("temp");
    let later = BarrierRig::new(fresh.path());
    let mark = later.ingest("checkout", &["user 1 logged in"]).await;
    let registered = later.epochs.current();
    let cut = later.epochs.open_cut();
    assert_eq!(cut, registered, "the guard was registered before the cut");
    // A publish registered *after* that cut: its frames are above the
    // cut's mark by construction.
    let after = later.epochs.current();
    later.sink.note_resettled(after);
    assert_eq!(
        later.barrier.tick(&later.pipeline, false),
        CutOutcome::Stamped
    );
    assert_eq!(
        later.commits.last_checkpoint(),
        Some(mark),
        "a post-cut settlement fails no cut at or below the current one",
    );
}

/// The latch is checked before the cut, but the rotation hook does not
/// consult it at all: it runs on the request path, where a cut it cannot
/// take is still a drain it must not lose. So a latched node keeps
/// acquiring pending cuts, and the tick's early return is the only place
/// left that can settle them — left in the slot, their `Drained` batches
/// hold the sink's in-flight publish guards, and every
/// `quiesce_publishes` (shutdown's included) then waits on them for the
/// life of the process.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_latched_tick_settles_the_cut_the_rotation_hook_left_pending() {
    // Given a latched node with one acknowledged batch buffered.
    let tmp = tempfile::TempDir::new().expect("temp");
    let wal_root = tmp.path().join("wal");
    let rig = BarrierRig::with_rotation_capture(
        tmp.path(),
        ourios_wal::WalConfig {
            segment_age_secs: 1, // the WAL's floor; slept past below
            ..wal_config(&wal_root)
        },
        usize::MAX,
    );
    rig.ingest("checkout", &["user 1 logged in"]).await;
    rig.epochs.report(rig.epochs.current());

    // When the segment ages out and the next append observes the change,
    // so the hook captures despite the latch.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    rig.ingest("checkout", &["payment 9 settled"]).await;
    rig.pipeline.quiesce_encodes();
    assert!(
        rig.barrier.pending_mark().is_some(),
        "the request path filled the slot while the node was latched",
    );

    // Then the tick that refuses the cut settles what it found.
    assert_eq!(rig.barrier.tick(&rig.pipeline, false), CutOutcome::Latched);
    assert_eq!(rig.barrier.pending_mark(), None, "the slot is empty");
    assert_parked_rather_than_published(&rig, 2);

    // And the publish guards those batches held are released, so a
    // quiesce returns rather than waiting out the process.
    let sink = rig.sink.clone();
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || sink.quiesce_publishes()),
    )
    .await
    .expect("the parked batches released their publish guards")
    .expect("the quiesce did not panic");
}

/// Scenario RFC0052.1 — a latched node settles the cuts it can no longer run.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_1_a_latched_node_does_not_accumulate_the_cuts_it_refuses() {
    // Given a latched node that keeps acquiring cuts: the rotation hook
    // does not consult the latch, so captures keep filling the slot and
    // `tick` keeps parking them. Each park dates a settlement, and the
    // `settle_cut` that would retire it is on `run_pending`'s path —
    // the one a latched tick returns before reaching.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    rig.ingest("checkout", &["user 0 logged in"]).await;
    let guard = rig.sink.begin_publish();
    let panicking = std::thread::spawn(move || {
        let _held = guard;
        panic!("injected publish panic");
    });
    assert!(panicking.join().is_err(), "the publish panicked");
    assert!(
        rig.epochs.capture().failed_epoch().is_some(),
        "which latched the node",
    );

    // When it keeps ticking, for as long as a latched process would,
    // with each round's capture holding real drained records.
    let mut round = 0u32;
    let few = latched_rounds(&rig, &mut round, 4).await;
    let many = latched_rounds(&rig, &mut round, 32).await;

    // Then the settlement list does not grow with the tick count: a
    // latched node stands still rather than leaking a record per cut for
    // the life of the process. The unwind's own settlement stays —
    // `Unwound` is never spent, because the records exist only in the
    // WAL until a restart re-mines them.
    assert_eq!(
        many, few,
        "a latched node settles each refused cut instead of accumulating it",
    );
    assert!(
        !rig.sink.quiesce_publishes().all_ok(rig.epochs.current()),
        "and the unwind it is latched on is still recorded",
    );
}

/// Ingest, capture and tick `rounds` times against a latched rig, and
/// report how many settlements the sink is left carrying.
///
/// The capture is the **hook's**, not the timer's, because only the
/// hook's order dates a settlement: it drains before opening the cut, so
/// the batches are registered one epoch behind the one they are parked
/// at, which is what `note_resettled` records. It is also the path
/// Copilot's finding names — a latched node keeps taking rotation
/// captures, because the hook does not consult the latch.
async fn latched_rounds(rig: &BarrierRig, round: &mut u32, rounds: usize) -> usize {
    for _ in 0..rounds {
        *round += 1;
        let body = format!("user {round} logged in");
        let mark = rig.ingest("checkout", &[&body]).await;
        rig.pipeline.quiesce_encodes();
        let _captured = rig
            .pipeline
            .with_miner(|miner| rig.barrier.capture_rotation(miner, mark));
        assert_eq!(rig.barrier.tick(&rig.pipeline, false), CutOutcome::Latched);
    }
    rig.sink.quiesce_publishes().recorded()
}

/// A refused cut's batches are back in the sink, dated, with nothing put
/// to the store.
fn assert_parked_rather_than_published(rig: &BarrierRig, records: usize) {
    assert_eq!(
        rig.sink.buffered_records(),
        records,
        "the cut's batch is parked back beside the appends around it",
    );
    assert!(rig.data_files().is_empty(), "and nothing was published");
}

/// Scenario RFC0052.1 — a panic in a cadence tick itself, and a `JoinError` at shutdown.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.1 stub — implemented in the timer green slice E (barrier-tick panic lowers failed_epoch; housekeeping-tick panic counts cadence_panic)"]
fn rfc0052_1_cadence_tick_panic_and_join_error_read_as_a_failed_cut() {
    todo!(
        "RFC0052.1 — a panic in the barrier tick outside any batch guard \
         lowers failed_epoch to that tick's epoch, invalidates the \
         pending slot, and the barrier task takes the next tick; a panic \
         in a housekeeping tick counts with error.type = cadence_panic, \
         leaves the checkpoint untouched, and the next pass re-plans the \
         uncommitted plan; a JoinError from either task at shutdown is \
         logged and read as a failed cut"
    );
}
