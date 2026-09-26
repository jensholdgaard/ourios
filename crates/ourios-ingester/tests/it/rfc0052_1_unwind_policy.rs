//! RFC0052.1 — What an unwind leaves behind: no checkpoint, no
//! snapshot, no Parquet object, and a WAL that still replays.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! These legs use the seeded scheduler RFC0052.14 uses, so a latch
//! stored after the pending count settles fails the test rather than
//! passing by timing. The pool-level half — where the epoch is stamped,
//! and what the guard covers — is [`crate::rfc0052_1_epoch_scope`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ourios_ingester::barrier::{CaptureOutcome, CutOutcome};

use crate::rfc0052_barrier_support::{BarrierRig, wal_config};

/// Scenario RFC0052.1 — unwind leg: an age-sweep publish panics inside `quiesce_publishes`.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_1_publish_panic_during_quiesce_leaves_checkpoint_and_snapshots_unchanged() {
    // Given a publish registered *before* the barrier began — the age
    // sweep's own in-flight guard — and an acknowledged frame.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    rig.ingest("checkout", &["user 1 logged in"]).await;

    let sink = rig.sink.clone();
    let panicking = Arc::new(AtomicBool::new(false));
    let release = Arc::clone(&panicking);
    // The guard is taken on this thread, before the barrier starts, so
    // it carries the epoch of the cut the barrier is about to take.
    let guard = sink.begin_publish();
    let epoch = guard.epoch();
    let sweep = std::thread::spawn(move || {
        while !release.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        // The guard drops during this unwind, which is what releases the
        // barrier waiting in `quiesce_publishes`.
        let _held = guard;
        panic!("injected age-sweep publish panic");
    });

    // When the barrier reaches `quiesce_publishes` and the publish then
    // panics: the latch it sets lands *after* the barrier's first check.
    let barrier = Arc::clone(&rig.barrier);
    let pipeline = rig.pipeline.clone();
    let running = std::thread::spawn(move || barrier.tick(&pipeline, false));
    std::thread::sleep(Duration::from_millis(50));
    panicking.store(true, Ordering::Release);
    assert!(sweep.join().is_err(), "the publish panicked");
    let outcome = running.join().expect("the barrier itself did not panic");

    // Then the checkpoint and every snapshot are unchanged.
    assert_eq!(
        outcome,
        CutOutcome::Latched,
        "the latch is observed at the recheck before stamping",
    );
    assert_eq!(rig.commits.last_checkpoint(), None, "no checkpoint");
    assert!(rig.snapshots().is_empty(), "and no snapshot was installed");

    // And the publish's outcome is reported failed independently of the
    // latch: the recheck defends the ordering, the outcome defends the
    // data, and either alone refuses the stamp.
    assert!(
        !rig.sink.quiesce_publishes().all_ok(epoch),
        "the unwind is recorded as a failed publish, not only as a latch",
    );
}

/// Scenario RFC0052.1 — unwind leg: a failed cut flush still waits out the publishes before it.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_1_a_cut_whose_own_flush_failed_still_reports_the_publishes_before_it() {
    // Given a publish registered *before* the cut and still in flight,
    // and a cut whose own flush will fail: §3.1's `prior_ok` is "always
    // evaluated, so a failed cut flush cannot skip their outcome and
    // requeue path".
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    rig.ingest("checkout", &["user 1 logged in"]).await;
    rig.pipeline.quiesce_encodes();
    let guard = rig.sink.begin_publish();
    let registered = guard.epoch();

    assert_eq!(
        rig.barrier.capture(&rig.pipeline, false),
        CaptureOutcome::Filled,
    );
    assert_eq!(
        rig.barrier.pending_epoch(),
        Some(registered),
        "the guard was taken before the cut, so the cut carries its epoch",
    );
    // The cut's own `write_ordered` now fails, which is the arm that used
    // to return before the quiesce.
    rig.sabotage_data_store();

    // When the cut runs and the prior publish then panics — after the
    // failed flush, so only a path that actually waits can observe it.
    let panicking = Arc::new(AtomicBool::new(false));
    let release = Arc::clone(&panicking);
    let sweep = std::thread::spawn(move || {
        while !release.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        let _held = guard;
        panic!("injected age-sweep publish panic");
    });
    let barrier = Arc::clone(&rig.barrier);
    let running = std::thread::spawn(move || barrier.run_pending());
    std::thread::sleep(Duration::from_millis(50));
    panicking.store(true, Ordering::Release);
    assert!(sweep.join().is_err(), "the publish panicked");
    let outcome = running.join().expect("the barrier itself did not panic");

    // Then the cut reports the latch that publish set. Returning on the
    // failed flush instead answers `Retained` — the publish is left in
    // flight past the cut that was meant to wait for it, and the requeue
    // it is about to make lands beside a buffer the next capture has
    // already drained.
    assert_eq!(
        outcome,
        CutOutcome::Latched,
        "the prior publish's outcome is evaluated even when the cut's own flush failed",
    );
    assert_nothing_reached_durability(&rig);
}

/// Scenario RFC0052.1 — unwind leg: an encode worker panics mid-batch, barrier after.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_1_encode_worker_panic_then_barrier_stamps_nothing_and_restart_replays() {
    // Given a whole ingest path whose encode worker panics mid-batch:
    // the sink's inline audit barrier runs inside `emit_concurrent`, on
    // the record that crosses the size target, which is exactly where
    // §3.1 says a worker panic leaves a batch's remainder in neither the
    // buffers nor Parquet. The batch is a real acknowledged OTLP export,
    // so "replayed on restart" is something this can actually observe.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::with_panicking_encode(tmp.path());
    let epoch = rig.epochs.current();
    let mark = rig
        .ingest("checkout", &["user 1 logged in", "user 2 logged in"])
        .await;
    rig.pipeline.quiesce_encodes();

    // Then the unwinding guard stored the latch before the decrement
    // that settled the count, so a barrier cannot see a quiet pool and a
    // clear latch...
    let latched = rig.epochs.capture();
    assert_eq!(
        latched.failed_epoch(),
        Some(epoch),
        "the worker's unwinding batch guard reported its own epoch",
    );

    // ...and a barrier that follows stamps nothing.
    assert_eq!(rig.barrier.tick(&rig.pipeline, false), CutOutcome::Latched);
    assert_nothing_reached_durability(&rig);

    // And the batch's unemitted records are replayed on restart: the
    // frame is in the WAL, which is the only place they survive.
    let report = restart_and_recover(rig);
    assert_eq!(
        report.max_delivered,
        Some(mark),
        "the acknowledged frame survived the worker's unwind",
    );
    assert_eq!(
        report.records_fed_to_miner, 2,
        "and every record in it is re-mined, with nothing suppressed by a mark that never stamped",
    );
}

/// Scenario RFC0052.1 — unwind leg: a real barrier tick over the panic, every schedule.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_1_barrier_tick_concurrent_with_a_worker_panic_stamps_nothing_under_every_schedule()
{
    // The leg above pins the pool-level ordering — a settled count has
    // a set latch — with `epochs.capture()` standing in for a barrier.
    // This one runs the barrier itself, over the rig whose own encode
    // worker panics, started at a seeded point in the panic's window
    // and **without** a prior `quiesce_encodes`. A tick that read the
    // latch only before its capture, or stamped after quiescing, would
    // be caught here rather than assumed away.
    for seed in 0..16u64 {
        let tmp = tempfile::TempDir::new().expect("temp");
        let rig = Arc::new(BarrierRig::with_panicking_encode(tmp.path()));
        // The batch is genuinely acknowledged — durable in the WAL —
        // and its worker then panics inside `emit_concurrent`.
        rig.ingest("checkout", &["user 1 logged in", "user 2 logged in"])
            .await;

        let outcome = {
            let rig = Arc::clone(&rig);
            tokio::task::spawn_blocking(move || {
                for _ in 0..(seed % 8) {
                    std::thread::yield_now();
                }
                rig.barrier.tick(&rig.pipeline, false)
            })
            .await
            .expect("the tick is caught inside the barrier, not propagated")
        };

        // Then the cut refuses under every schedule. Whichever check
        // catches it — the pre-capture one when the panic already
        // landed, `run_cut`'s when the capture's own quiesce waited it
        // out — the answer is the same, because the unwinding guard
        // reports before the decrement that settles the count.
        assert_eq!(outcome, CutOutcome::Latched, "seed {seed}");
        assert_eq!(
            rig.commits.last_checkpoint(),
            None,
            "seed {seed}: no mark passed the batch's unemitted remainder",
        );
        assert!(
            rig.snapshots().is_empty(),
            "seed {seed}: and no snapshot was installed",
        );
    }
}

/// Neither half of a cut landed: no checkpoint, no snapshot artefact, no
/// Parquet object.
fn assert_nothing_reached_durability(rig: &BarrierRig) {
    assert_eq!(rig.commits.last_checkpoint(), None, "no checkpoint");
    assert!(rig.snapshots().is_empty(), "and no snapshot was installed");
    assert!(
        rig.data_files().is_empty(),
        "and no batch reached a Parquet object",
    );
}

/// Drop the rig — releasing its `Wal` — and recover from the roots it
/// leaves behind, the way a restarting process would.
fn restart_and_recover(rig: BarrierRig) -> ourios_ingester::recovery::RecoveryReport {
    let wal_root = rig.wal_root.clone();
    let snapshots_root = rig.snapshots_root.clone();
    drop(rig);
    let mut wal = ourios_wal::Wal::open(wal_config(&wal_root)).expect("reopen");
    let mut miner = ourios_miner::cluster::MinerCluster::new(ourios_config::MinerConfig::default());
    ourios_ingester::recovery::recover(&mut wal, &snapshots_root, &mut miner)
        .expect("recovery completes")
}
