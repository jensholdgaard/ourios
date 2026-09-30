//! RFC0052.1 — What a latched node refuses, and what it settles on the
//! way out.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! The checkpoint half of the criterion is
//! [`crate::rfc0052_1_checkpoint_policy`]; these legs are about the node
//! that can no longer stamp at all. Stubs are `#[ignore]`d so the
//! default run stays green while the RFC is red; each names the green
//! slice that discharges it.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ourios_ingester::barrier::CutOutcome;
use ourios_ingester::cadence;
use ourios_ingester::housekeeping::{Housekeeper, HousekeepingTick};

use crate::rfc0052_barrier_support::{BarrierRig, JournalFaults, RigSpec, wal_config};

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
/// at, which is what `note_resettled` records. It is also the path a
/// latched node keeps taking cuts on, because the hook does not consult
/// the latch.
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_1_cadence_tick_panic_and_join_error_read_as_a_failed_cut() {
    barrier_tick_panic_lowers_the_latch_and_invalidates_the_pending_slot().await;
    housekeeping_tick_panic_leaves_the_checkpoint_and_the_next_pass_replans().await;
    a_join_error_at_shutdown_is_read_as_a_failed_cut().await;
}

async fn barrier_tick_panic_lowers_the_latch_and_invalidates_the_pending_slot() {
    // Given a pending cut the request path left in the slot, and a
    // barrier tick armed to panic in its own capture, before any batch
    // guard exists.
    let tmp = tempfile::TempDir::new().expect("temp");
    let faults = Arc::new(JournalFaults::default());
    let rig = BarrierRig::build(
        tmp.path(),
        RigSpec {
            rotation_capture: true,
            journal_faults: Some(Arc::clone(&faults)),
            ..RigSpec::new(aging_wal(tmp.path()))
        },
    );
    rig.ingest("checkout", &["user 1 logged in"]).await;
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    rig.ingest("checkout", &["payment 9 settled"]).await;
    rig.pipeline.quiesce_encodes();
    assert!(rig.barrier.pending_mark().is_some(), "the slot is filled");
    faults.panic_on_age_check.store(true, Ordering::Release);

    // When the barrier tick panics.
    let epoch = rig.epochs.current();
    let outcome = rig.barrier.tick(&rig.pipeline, true);

    // Then the latch is lowered to that tick's epoch, the pending slot is
    // invalidated with its batches parked rather than dropped, and
    // nothing was stamped.
    assert_eq!(outcome, CutOutcome::Latched);
    assert_eq!(
        rig.epochs.capture().failed_epoch(),
        Some(epoch),
        "the tick's own epoch is the failed one",
    );
    assert_eq!(rig.barrier.pending_mark(), None, "the pending cut is gone");
    assert_parked_rather_than_published(&rig, 2);
    assert_eq!(rig.commits.last_checkpoint(), None, "no checkpoint");
    assert!(rig.snapshots().is_empty(), "no snapshot");

    // And the barrier takes the next tick, which refuses rather than
    // stamping over the batches the panic may have stranded.
    rig.ingest("checkout", &["user 2 logged in"]).await;
    assert_eq!(rig.barrier.tick(&rig.pipeline, true), CutOutcome::Latched);
    assert_eq!(rig.commits.last_checkpoint(), None, "still no checkpoint");
}

async fn housekeeping_tick_panic_leaves_the_checkpoint_and_the_next_pass_replans() {
    // Given a closed segment the barrier has stamped past and
    // snapshotted, so housekeeping has something to reclaim.
    let tmp = tempfile::TempDir::new().expect("temp");
    let faults = Arc::new(JournalFaults::default());
    let rig = BarrierRig::build(
        tmp.path(),
        RigSpec {
            journal_faults: Some(Arc::clone(&faults)),
            ..RigSpec::new(aging_wal(tmp.path()))
        },
    );
    let mark = rig.ingest("checkout", &["user 1 logged in"]).await;
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(rig.barrier.tick(&rig.pipeline, true), CutOutcome::Stamped);
    assert_eq!(rig.commits.last_checkpoint(), Some(mark));
    let sealed = rig.wal_root.join(format!("{}.wal", mark.segment));
    assert!(sealed.exists(), "the stamped segment is still on disk");
    let housekeeper = Housekeeper::new(
        Arc::clone(&rig.commits),
        Arc::clone(&rig.barrier),
        rig.publish.clone(),
        usize::try_from(ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS).expect("the cap fits"),
    );

    // When a housekeeping tick panics after the WAL took its plan.
    faults.panic_after_prepare.store(true, Ordering::Release);
    let panicked = housekeeper.tick();

    // Then the tick reports the panic (counted as `cadence_panic`; the
    // metric half is `rfc0052_1_housekeeping_panic_metric.rs`), the
    // checkpoint is
    // untouched, no cut is failed, and nothing was unlinked.
    assert!(
        matches!(panicked, HousekeepingTick::Panicked),
        "{panicked:?}"
    );
    assert_eq!(rig.commits.last_checkpoint(), Some(mark), "the checkpoint");
    assert_eq!(
        rig.epochs.capture().failed_epoch(),
        None,
        "a housekeeping panic fails no cut",
    );
    assert!(sealed.exists(), "the uncommitted plan unlinked nothing");

    // And the next pass re-plans the uncommitted plan and reclaims it.
    let HousekeepingTick::Completed(progress) = housekeeper.tick() else {
        panic!("the next pass runs");
    };
    assert_eq!(progress.removed_segments, 1, "{progress:?}");
    assert!(!sealed.exists(), "the stamped segment is reclaimed");
    assert_eq!(rig.commits.last_checkpoint(), Some(mark));
}

async fn a_join_error_at_shutdown_is_read_as_a_failed_cut() {
    // Given a healthy node with an acknowledged batch buffered.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    rig.ingest("checkout", &["user 1 logged in"]).await;
    rig.pipeline.quiesce_encodes();
    assert_eq!(rig.epochs.capture().failed_epoch(), None);

    // When a cadence task joins cleanly, and then one joins with a panic
    // no tick caught.
    let clean = tokio::spawn(async {}).await;
    assert!(!cadence::read_join(&rig.epochs, "housekeeping", clean));
    assert_eq!(rig.epochs.capture().failed_epoch(), None, "a clean join");
    let panicked = tokio::spawn(async { panic!("injected cadence task panic") }).await;
    let latched = cadence::read_join(&rig.epochs, "barrier", panicked);

    // Then it is read as a failed cut: nothing stamps, nothing is
    // assumed drained.
    assert!(latched, "a JoinError latches");
    assert_eq!(rig.barrier.tick(&rig.pipeline, false), CutOutcome::Latched);
    assert_eq!(rig.commits.last_checkpoint(), None, "no checkpoint");
    assert!(rig.snapshots().is_empty(), "no snapshot");
    assert_eq!(rig.sink.buffered_records(), 1, "the record stays buffered");
}

/// A WAL whose segment ages out after one second — the floor — so a
/// sleep past it makes the next append, or the barrier's idle rotation,
/// close it.
fn aging_wal(tmp: &std::path::Path) -> ourios_wal::WalConfig {
    ourios_wal::WalConfig {
        segment_age_secs: 1,
        ..wal_config(&tmp.join("wal"))
    }
}
