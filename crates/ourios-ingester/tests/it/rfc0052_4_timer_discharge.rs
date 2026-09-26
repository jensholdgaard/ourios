//! RFC0052.4 — The timer discharges an owed rotation fsync on an idle
//! node, where no append ever will.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §3.3.
//!
//! `rfc0052_4_an_idle_rotation_discharges_an_owed_fsync_on_an_empty_segment`
//! pins the WAL half: `Wal::rotate` discharges the obligation before it
//! reads the kind, so the discretionary no-op cannot skip it. The half
//! here is the caller — the segment a failed post-rename fsync leaves
//! installed is *fresh*, so an age predicate alone never reaches that
//! rotation at all, and on an idle node no append arrives to retry the
//! discharge either.

use ourios_wal::{
    FrameKind, RotationFaults, RotationKind, RotationSite, RotationState, Wal, WalConfig,
};

use crate::ingest_support::{coordinator, wal_config};

/// A WAL whose post-rename fsync has failed once: the fresh segment is
/// installed, empty, and owes the directory fsync.
fn wal_owing_a_rotation_fsync(config: WalConfig) -> Wal {
    let mut wal = Wal::open(config).expect("open WAL");
    wal.append(FrameKind::OtlpBatch, b"a frame worth sealing")
        .expect("append");
    wal.sync().expect("sync");
    wal.arm_rotation_faults(RotationFaults::failing(RotationSite::ParentFsync, 1));
    wal.rotate(RotationKind::Owed)
        .expect_err("the parent fsync fails after the rename");
    assert!(
        matches!(wal.reclaim_state().rotation, RotationState::Retrying(_)),
        "the obligation is outstanding",
    );
    assert!(
        !wal.segment_age_exceeded(),
        "and the segment it left installed is fresh, so age alone never calls",
    );
    wal
}

/// Scenario RFC0052.4 — the timer discharges an owed fsync behind a young segment.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §3.3.
#[test]
fn rfc0052_4_the_timer_discharges_an_owed_fsync_behind_a_young_segment() {
    // Given an idle node owing a rotation-origin directory fsync.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let commits = coordinator(Box::new(wal_owing_a_rotation_fsync(wal_config(root))));
    let installed = segment_files(root).len();

    // When the barrier's idle-rotation tick runs.
    commits.rotate_if_aged().expect("the tick");

    // Then the obligation is discharged — an age check alone returns
    // before `rotate`, and leaves it owed until traffic returns.
    assert!(
        matches!(commits.reclaim_state().rotation, RotationState::Healthy),
        "the one caller that can discharge it without an append did",
    );
    // And the young segment is not rotated: `rotate` discharges before
    // it reads the kind, so the discretionary no-op still holds.
    assert_eq!(
        segment_files(root).len(),
        installed,
        "discharging an obligation is not a reason to churn a fresh segment",
    );
}

/// Scenario RFC0052.4 — a young segment owing nothing is still left alone.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §3.3.
#[test]
fn rfc0052_4_a_young_segment_owing_nothing_is_not_rotated_by_the_tick() {
    // The obligation widens the guard; it must not dissolve it. Without
    // this leg the fix is satisfied by rotating on every tick, which
    // would seal a segment per `barrier_secs` on a busy node.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let mut wal = Wal::open(wal_config(root)).expect("open WAL");
    wal.append(FrameKind::OtlpBatch, b"a frame the tick must not seal")
        .expect("append");
    wal.sync().expect("sync");
    let commits = coordinator(Box::new(wal));
    let before = segment_files(root).len();

    commits.rotate_if_aged().expect("the tick");

    assert_eq!(
        segment_files(root).len(),
        before,
        "a young segment holding a frame is rotated by neither trigger",
    );
    assert!(matches!(
        commits.reclaim_state().rotation,
        RotationState::Healthy
    ));
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
