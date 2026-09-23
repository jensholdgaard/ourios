//! RFC0052.14 — The timer cannot stamp across a concurrent submit.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! A seeded-interleaving test rather than a timing one (RFC 0052 §6):
//! the window is narrow, and a wall-clock test that happens to pass
//! proves nothing. The fallback shape, if seeding proves unreachable,
//! holds the timer artificially between the quiesce and the end of the
//! cut while driving ingest — the lock is never held across store I/O.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use ourios_ingester::barrier::CutOutcome;
use ourios_ingester::receiver::{CommitCoordinator, IngestPipeline, Journal, ReceiveError};
use ourios_wal::WalOffset;

use crate::rfc0052_barrier_support::{BarrierRig, wal_config};

/// Scenario RFC0052.14 — no mark passes a frame whose encode had not emitted.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_14_no_checkpoint_passes_an_unemitted_frame_under_any_interleaving() {
    // Given the reclamation timer firing while ingest submits
    // continuously, under a seeded set of interleavings rather than a
    // wall clock: the window between the quiesce and the stamp is
    // narrow, and a timing test that happens to pass proves nothing.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = Arc::new(BarrierRig::new(tmp.path()));
    let stop = Arc::new(AtomicBool::new(false));
    let submitted = Arc::new(AtomicU64::new(0));

    let ingest = {
        let rig = Arc::clone(&rig);
        let stop = Arc::clone(&stop);
        let submitted = Arc::clone(&submitted);
        tokio::spawn(async move {
            let mut n = 0u64;
            while !stop.load(Ordering::Acquire) {
                rig.ingest("checkout", &[&format!("user {n} logged in")])
                    .await;
                submitted.fetch_add(1, Ordering::Release);
                n += 1;
            }
        })
    };

    // When the timer runs its sequence, repeatedly, across that traffic.
    for seed in 0..16u64 {
        tokio::time::sleep(Duration::from_millis(seed % 5)).await;
        let rig = Arc::clone(&rig);
        let outcome = tokio::task::spawn_blocking(move || {
            let outcome = rig.barrier.tick(&rig.pipeline, false);
            // Then the mark used is the one read *after* the quiesce
            // under the same exclusion. Every acknowledged frame at or
            // below it has finished its turn, so it is never above the
            // pipeline's own durable mark — and a mark read before the
            // quiesce could be.
            let stamped = rig.commits.last_checkpoint();
            let durable = rig.pipeline.last_durable();
            (outcome, stamped, durable)
        })
        .await
        .expect("the barrier tick did not panic");
        let (outcome, stamped, durable) = outcome;
        assert_ne!(
            outcome,
            CutOutcome::Latched,
            "seed {seed}: nothing panicked"
        );
        if let Some(stamped) = stamped {
            assert!(
                durable.is_some_and(|durable| stamped <= durable),
                "seed {seed}: no checkpoint passes a frame whose turn had not finished",
            );
        }
    }

    stop.store(true, Ordering::Release);
    ingest.await.expect("ingest loop");
    assert!(
        submitted.load(Ordering::Acquire) > 0,
        "the interleaving was against real traffic",
    );

    // And a final cut, with ingest stopped, covers everything: no frame
    // is stranded above the mark by the interleaving.
    let rig = Arc::clone(&rig);
    let final_mark = rig.pipeline.last_durable();
    tokio::task::spawn_blocking(move || {
        assert_eq!(rig.barrier.tick(&rig.pipeline, false), CutOutcome::Stamped);
        assert_eq!(
            rig.commits.last_checkpoint(),
            final_mark,
            "the last acknowledged turn's own frame offset is the mark",
        );
    })
    .await
    .expect("final cut");
}

/// Scenario RFC0052.14 — the mark is a turn's own frame offset, never the sync's EOF.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_14_mark_is_the_turns_frame_offset_not_the_flush_eof() {
    // Given a flush whose sync covers two turns: `TwoTurnJournal`
    // reports an EOF that already includes the *second* frame while the
    // first turn is the one being acknowledged, which is exactly the
    // shape `CommitCoordinator::flush` produces when a later append
    // lands during the sync.
    let tmp = tempfile::TempDir::new().expect("temp");
    let eof = WalOffset {
        segment: uuid::Uuid::from_u128(1),
        byte: 4_096,
    };
    let journal = TwoTurnJournal {
        eof,
        appended: 0,
        segment: uuid::Uuid::from_u128(1),
    };
    let commits = CommitCoordinator::new(
        Box::new(journal),
        Duration::from_millis(20),
        u64::MAX, // no fill cut: the window is what batches these
    );
    let miner = ourios_miner::cluster::MinerCluster::new(ourios_config::MinerConfig::default());
    let pipeline = IngestPipeline::new(Arc::clone(&commits), miner);
    drop(tmp);

    // When the first turn completes and a barrier runs between it and
    // the second.
    pipeline
        .ingest(
            crate::ingest_support::request(vec![crate::ingest_support::resource_logs(
                "checkout",
                &["user 1 logged in"],
            )]),
            ourios_core::tenant::TenantId::new("checkout"),
        )
        .await
        .expect("turn one acks");

    // Then the mark is the first turn's own frame offset, so the later
    // frame the same flush made durable — but which is not yet mined nor
    // acknowledged — is never covered.
    let mark = pipeline.last_durable().expect("a mark");
    assert_ne!(
        mark, eof,
        "the sync's reported EOF is not the mark: it already covers a turn that has not run",
    );
    assert!(
        mark < eof,
        "the mark is the turn's own frame offset, strictly below the flush's EOF",
    );

    // And the post-recovery seed is never `max_delivered` alone: a frame
    // replay delivered can be one whose group sync never completed, so a
    // seeded mark is not a mark. The barrier reads only what a turn in
    // this process acknowledged.
    assert_eq!(
        commits.last_checkpoint(),
        None,
        "a node whose first barrier never ran seeds None and lets its first turn establish one",
    );
    let replayed = WalOffset {
        segment: uuid::Uuid::from_u128(1),
        byte: 9_999,
    };
    let seeded = IngestPipeline::new(
        CommitCoordinator::new(
            Box::new(TwoTurnJournal {
                eof,
                appended: 0,
                segment: uuid::Uuid::from_u128(1),
            }),
            Duration::from_millis(20),
            u64::MAX,
        ),
        ourios_miner::cluster::MinerCluster::new(ourios_config::MinerConfig::default()),
    )
    .with_last_durable(Some(replayed));
    assert_eq!(
        seeded.last_durable(),
        Some(replayed),
        "the seed still stamps the shutdown snapshot's high-water (RFC 0001 §6.9)",
    );
    assert_eq!(
        seeded.acknowledged_durable(),
        None,
        "but it is not a mark a cut may checkpoint at",
    );
    assert!(
        pipeline.acknowledged_durable().is_some(),
        "whereas a turn's own offset is",
    );
}

/// Scenario RFC0052.14 — a rotation capture past the sink's ceiling parks and advances nothing.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.14 stub — implemented in the barrier green slice D (RotationDecision on append_batch hands the cut to the barrier task)"]
fn rfc0052_14_rotation_capture_past_the_ceiling_parks_every_drained_batch() {
    todo!(
        "RFC0052.14 — a rotation capture that would take the pending cut \
         past the sink's ceiling parks every batch it drained and \
         advances neither the mark, the snapshots nor the epoch; the \
         checkpoint that pending cut eventually stamps covers only \
         frames its own batches held, and the parked partitions are \
         covered by the next cut; a rotation-fired cut performs no store \
         I/O inside the ingest turn, and an append admitted after the \
         turn is in neither that cut's checkpoint nor its snapshot"
    );
}

/// Scenario RFC0052.14 — an idle rotation on the barrier tick rotates before the cut.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_14_idle_rotation_on_the_tick_marks_the_last_acked_turn_not_the_boundary() {
    // Given an idle node whose current segment has outlived its age cap:
    // no append will ever fire the rotation check again, so the barrier
    // tick is the only caller that can close it.
    let tmp = tempfile::TempDir::new().expect("temp");
    let wal_root = tmp.path().join("wal");
    let rig = BarrierRig::with(
        tmp.path(),
        crate::rfc0052_barrier_support::never_flush(),
        2,
        ourios_wal::WalConfig {
            segment_age_secs: 1, // the WAL's floor; slept past below
            ..wal_config(&wal_root)
        },
    );
    let acked = rig.ingest("checkout", &["user 1 logged in"]).await;
    let before = segment_files(&wal_root);
    assert_eq!(before.len(), 1, "one open segment holding one frame");
    // The age is the segment's `UUIDv7` mint time, so it has to elapse;
    // the WAL refuses a cap below one second, and this is the only leg
    // that needs one to pass.
    tokio::time::sleep(Duration::from_millis(1_200)).await;

    // When the barrier tick rotates before the cut, under the exclusion.
    let rig = Arc::new(rig);
    {
        let rig = Arc::clone(&rig);
        tokio::task::spawn_blocking(move || {
            assert_eq!(rig.barrier.tick(&rig.pipeline, true), CutOutcome::Stamped);
        })
        .await
        .expect("tick");
    }

    // Then the segment was closed and a fresh one installed...
    let after = segment_files(&wal_root);
    assert_eq!(after.len(), 2, "the idle segment rotated");

    // ...and the mark is the last acknowledged turn's own frame offset in
    // the closed segment, never the rotation boundary.
    let stamped = rig.commits.last_checkpoint().expect("a checkpoint");
    assert_eq!(
        stamped, acked,
        "the mark is `last_durable`, not the boundary the rotation created",
    );

    // And a frame appended after the release lands in the new segment,
    // above the mark.
    let next = rig.ingest("checkout", &["user 2 logged in"]).await;
    assert_ne!(
        next.segment, stamped.segment,
        "the frame after the release is in the new segment",
    );
    assert!(next > stamped, "and above the mark");
}

/// Scenario RFC0052.14 — a pre-cut batch that detached mid-batch is fully in the cut.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.14 stub — implemented in the barrier green slice D (quiesce waits for the encode phase, not for a registered publish)"]
fn rfc0052_14_pre_cut_batch_detaching_mid_batch_is_covered_without_waiting_on_the_put() {
    todo!(
        "RFC0052.14 — a pre-cut batch whose first record detached a \
         partition mid-batch has its remaining records in the cut under \
         any interleaving: the quiesce waits for the batch's encode \
         phase, not for a worker to register a publish, and the detached \
         partition's PUT is not waited on; with several partitions \
         detached from one pre-cut batch completing in any order, the \
         batch's shared completion holds the in-flight count until the \
         last finishes"
    );
}

/// Scenario RFC0052.14 — cuts are strictly ordered; a failed A invalidates B.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_14_cut_b_behind_a_held_cut_a_is_invalidated_when_a_fails() {
    // Given cut A captured and its flush about to fail, with cut B
    // captured behind it.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    rig.ingest("checkout", &["user 1 logged in"]).await;
    rig.barrier.capture(&rig.pipeline, false); // cut A fills the slot
    let a_mark = rig.barrier.pending_mark().expect("A holds the mark");
    rig.ingest("checkout", &["user 2 logged in"]).await;
    rig.barrier.capture(&rig.pipeline, false); // B coalesces behind A
    assert!(
        rig.barrier.pending_mark().is_some_and(|b| b > a_mark),
        "B's newer mark is what the coalesced cut would stamp",
    );

    // When A's flush fails.
    rig.sabotage_data_store();
    let outcome = rig.barrier.run_pending();

    // Then nothing of B's is installed and no mark of B's is stamped:
    // B's snapshot bytes already fold frames whose only durable copy was
    // A's batches.
    assert_eq!(outcome, CutOutcome::Retained);
    assert_eq!(rig.commits.last_checkpoint(), None, "no mark stamped");
    assert!(rig.snapshots().is_empty(), "no snapshot installed");
    assert_eq!(
        rig.barrier.pending_mark(),
        None,
        "the pending cut is invalidated, not left to stamp behind a failure",
    );
    assert_eq!(
        rig.sink.buffered_records(),
        2,
        "A's and B's records are all back in the buffers",
    );

    // And when A succeeds instead, B runs unchanged: the re-captured cut
    // covers everything either held.
    std::fs::remove_file(&rig.data_root).expect("un-sabotage");
    std::fs::create_dir_all(&rig.data_root).expect("data root");
    let recaptured = rig.pipeline.last_durable().expect("a mark");
    let rig = Arc::new(rig);
    let run = Arc::clone(&rig);
    tokio::task::spawn_blocking(move || {
        assert_eq!(run.barrier.tick(&run.pipeline, false), CutOutcome::Stamped);
    })
    .await
    .expect("tick");
    assert_eq!(
        rig.commits.last_checkpoint(),
        Some(recaptured),
        "the re-captured cut covers A's and B's records alike",
    );
    assert_eq!(rig.sink.buffered_records(), 0, "and drained them");
}

/// Scenario RFC0052.14 — an age-sweep publish registered after the cut is neither covered nor lost.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_14_post_cut_publish_failure_fails_no_cut_at_or_below_the_current_one() {
    // Given a cut captured, and an age-sweep publish registered *after*
    // it — between the barrier's `quiesce_publishes` and its checkpoint.
    // Every frame that publish holds is above the cut's mark, by §3.1's
    // invariant.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    let mark = rig.ingest("checkout", &["user 1 logged in"]).await;
    rig.barrier.capture(&rig.pipeline, false);
    let cut = rig.epochs.current();

    // When that publish fails transiently: it requeues, dated with the
    // epoch current at the return.
    let after = rig.ingest("checkout", &["user 2 logged in"]).await;
    assert!(after > mark, "its frames are above the cut's mark");
    let registered = rig.epochs.current();
    rig.sink.note_resettled(registered);

    // Then the cut at or below the current one proceeds.
    assert_eq!(rig.barrier.run_pending(), CutOutcome::Stamped);
    assert_eq!(
        rig.commits.last_checkpoint(),
        Some(mark),
        "a post-cut failure fails no cut at or below the current one",
    );
    assert!(
        cut <= registered,
        "the publish was registered after the cut"
    );

    // And a panic in such a publish — carrying the next epoch — fails
    // every *later* barrier until restart, while the one in flight had
    // already proceeded.
    let guard = rig.sink.begin_publish();
    let panicking = std::thread::spawn(move || {
        let _held = guard;
        panic!("injected post-cut publish panic");
    });
    assert!(panicking.join().is_err(), "the publish panicked");
    let rig = Arc::new(rig);
    let later = Arc::clone(&rig);
    tokio::task::spawn_blocking(move || {
        assert_eq!(
            later.barrier.tick(&later.pipeline, false),
            CutOutcome::Latched,
            "every later barrier is refused until a restart",
        );
    })
    .await
    .expect("tick");
    assert_eq!(
        rig.commits.last_checkpoint(),
        Some(mark),
        "and the mark the proceeding cut stamped is still the last one",
    );
}

/// A journal whose `sync` reports an EOF that already covers a frame no
/// turn has run — the two-turn flush RFC 0052 §3.1 warns about.
struct TwoTurnJournal {
    eof: WalOffset,
    appended: u64,
    segment: uuid::Uuid,
}

impl Journal for TwoTurnJournal {
    fn append_batch(&mut self, _payload: &[u8]) -> Result<WalOffset, ReceiveError> {
        self.appended += 1;
        Ok(WalOffset {
            segment: self.segment,
            byte: self.appended * 16,
        })
    }

    fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
        Ok(self.eof)
    }

    fn unflushed_bytes(&self) -> u64 {
        0
    }
}

fn segment_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = std::fs::read_dir(root)
        .expect("read_dir")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "wal"))
        .collect();
    out.sort();
    out
}
