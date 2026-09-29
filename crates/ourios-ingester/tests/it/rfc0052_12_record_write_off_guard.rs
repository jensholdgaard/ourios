//! RFC0052.12 — an append never waits for a pass's `RECLAIM` write.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! §3.7 puts the whole file half of a pass — the `RECLAIM` slot write,
//! the unlinks and the parent fsync — outside the journal guard, so this
//! leg drives the protocol where the guard lives: `CommitCoordinator::maintain`
//! over a real `Wal`, with the slot write held at the WAL's
//! fault-injection point while an append goes through the same
//! coordinator.

use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use ourios_ingester::receiver::commit::CommitCoordinator;
use ourios_wal::{FrameKind, PassOutcome, SnapshotHorizons, TenantBatch, Wal, WalConfig};

/// Long enough that a scheduler hiccup is not a failure, short enough
/// that an append parked behind the guard fails the leg promptly.
const WAIT: Duration = Duration::from_secs(5);

fn config(root: &Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 1,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
        rotation_retry_attempts: ourios_wal::DEFAULT_ROTATION_RETRY_ATTEMPTS,
        macos_full_fsync: false,
    }
}

/// Scenario RFC0052.12 — an append completes while the pass's `RECLAIM` write is held.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_12_append_completes_while_the_record_write_is_held() {
    // Given: a root whose checkpoint is settled, so the first pass plans
    // and owes a record write — it adopts its mode durably before it
    // could unlink anything.
    let tmp = tempfile::TempDir::new().expect("temp");
    let payload = TenantBatch::encode("alpha", b"frame").expect("encode");
    let mut wal = Wal::open(config(tmp.path())).expect("open");
    let mark = wal
        .append(FrameKind::TenantOtlpBatch, &payload)
        .expect("append");
    wal.sync().expect("sync");
    wal.checkpoint(mark).expect("checkpoint");

    // Given: the pass's slot write held at the fault-injection point
    // until the test lets it go.
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let entered = Mutex::new(entered_tx);
    let release = Mutex::new(release_rx);
    wal.arm_record_write_hook(move || {
        if let Ok(tx) = entered.lock() {
            let _ = tx.send(());
        }
        if let Ok(rx) = release.lock() {
            let _ = rx.recv();
        }
    });
    let coordinator = CommitCoordinator::new(Box::new(wal), Duration::from_millis(1), u64::MAX);

    // When: a pass runs and reaches its record write — between
    // `housekeeping_prepare` and `housekeeping_commit`.
    let pass = {
        let coordinator = Arc::clone(&coordinator);
        std::thread::spawn(move || {
            coordinator.maintain(
                &SnapshotHorizons::NoConsumer,
                usize::try_from(ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS).expect("the cap fits"),
            )
        })
    };
    entered_rx
        .recv_timeout(WAIT)
        .expect("the pass reached its RECLAIM write");

    // When: an append is taken through the same coordinator while that
    // write is still in flight. Awaited off the runtime: an append
    // parked on the journal mutex blocks its worker synchronously, and
    // a tokio timeout on it would never fire.
    let (done_tx, done_rx) = mpsc::channel();
    {
        let coordinator = Arc::clone(&coordinator);
        tokio::spawn(async move {
            let _ = done_tx.send(coordinator.commit(&payload).await.result);
        });
    }
    let appended = tokio::task::spawn_blocking(move || done_rx.recv_timeout(WAIT))
        .await
        .expect("join");
    // Released before anything is asserted, so a failure here cannot
    // leave the pass thread parked forever.
    release_tx.send(()).expect("release the held write");
    let progress = pass.join().expect("pass thread").expect("the pass settles");

    // Then: the append was acknowledged while the write was held, above
    // the mark the pass reclaimed under.
    let offset = appended
        .expect("the append completed while the pass's RECLAIM write was held")
        .expect("and it was acknowledged");
    assert!(
        offset > mark,
        "{offset:?} lands above the checkpoint {mark:?}"
    );
    // And: the pass really was one that owed a record write.
    assert_eq!(progress.outcome, PassOutcome::Planned);
}
