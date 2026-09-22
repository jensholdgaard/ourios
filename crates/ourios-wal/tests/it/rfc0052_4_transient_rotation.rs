//! RFC0052.4 — A transient rotation failure recovers without a restart.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! One leg per §3.3 rotation site — the four the code has today plus
//! the `rename(partial, final)` step the temporary name adds — each
//! failing once, plus the `proptest` §6 asks for so the five sites are
//! not tested only one way. The post-rename site is the odd one: the
//! installed segment is the live append target, so the retry is the
//! directory fsync alone, discharged by `sync`, never a re-`rotate`.
//!
//! Every leg drives its failure through `Wal::arm_rotation_faults`
//! (the `fault-injection` feature, on for this crate's test targets).
//! The five sites are `fsync`, `create` and `rename` calls; a directory
//! permission reaches only one of them, so a seam is the only way to
//! cover the matrix. The rotation itself is triggered by the §6.5 age
//! cap, with the segment backdated rather than slept for.

use std::path::{Path, PathBuf};

use proptest::prelude::*;

use ourios_wal::{
    AppendError, FrameKind, OpenError, RotationFaults, RotationKind, RotationSite, RotationState,
    Wal, WalConfig,
};

use crate::rfc0052_support::{backdate_segment, segment_files};

fn config(root: &Path, rotation_retry_attempts: u32) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: ourios_wal::MIN_SEGMENT_SIZE_BYTES,
        segment_age_secs: 1,
        housekeeping_secs: 60,
        max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
        rotation_retry_attempts,
        macos_full_fsync: false,
    }
}

const BUDGET: u32 = ourios_wal::DEFAULT_ROTATION_RETRY_ATTEMPTS;

/// A WAL whose next `append` rotates: one frame already in the segment,
/// and the segment backdated well past the 1 s age cap.
fn wal_due_for_rotation(root: &Path, budget: u32, faults: RotationFaults) -> Wal {
    let mut seed = Wal::open(config(root, budget)).expect("seed the root");
    seed.append(FrameKind::OtlpBatch, b"seed")
        .expect("seed frame");
    seed.sync().expect("seed sync");
    drop(seed);
    backdate_segment(root, 5_000);

    let mut wal = Wal::open(config(root, budget)).expect("reopen");
    wal.rebuild_ledger().expect("ledger");
    wal.arm_rotation_faults(faults);
    wal
}

fn partials(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(root)
        .expect("read_dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.to_string_lossy().ends_with(".wal.partial"))
        .collect();
    out.sort();
    out
}

/// A pre-rename site failing once: the append that meets the fault is
/// refused as *retrying* (a later one can still succeed), the next
/// append re-enters `rotate`, succeeds and is acked, and nothing the
/// failed attempt left is selectable as a segment on a subsequent open.
fn a_pre_rename_site_recovers_on_the_next_append(site: RotationSite) {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let mut wal = wal_due_for_rotation(root, BUDGET, RotationFaults::failing(site, 1));
    let before = segment_files(root).len();

    let err = wal
        .append(FrameKind::OtlpBatch, b"the append that meets the fault")
        .expect_err("the armed site fails this rotation");
    match &err {
        AppendError::RotationRetrying(fault) => {
            assert_eq!(fault.op(), site.op(), "the failing step is named");
            assert_eq!(fault.attempts(), 1, "one unit of the budget is spent");
        }
        other => panic!("{site:?}: a failure inside the budget is retrying, got {other:?}"),
    }
    assert_eq!(
        segment_files(root).len(),
        before,
        "{site:?}: nothing before the rename installs a segment",
    );

    // The arming was for one attempt, so the condition has cleared: a
    // later append re-enters `rotate`.
    let accepted = wal
        .append(FrameKind::OtlpBatch, b"the append after the fault clears")
        .expect("the retry rotates and the append lands");
    wal.sync().expect("the batch behind it is acked");
    assert!(
        matches!(wal.reclaim_state().rotation, RotationState::Healthy),
        "{site:?}: a discharged obligation resets the budget",
    );
    assert_eq!(
        segment_files(root).len(),
        before + 1,
        "{site:?}: exactly one segment was installed",
    );

    // Asserted by reopening rather than by inspecting the directory:
    // the temporary name is an implementation detail, and what the
    // criterion pins is that no *selectable* file survived.
    drop(wal);
    let mut reopened = Wal::open(config(root, BUDGET)).expect("reopen");
    let after = reopened
        .append(FrameKind::OtlpBatch, b"after reopen")
        .expect("the reopened WAL appends");
    assert_eq!(
        after.segment, accepted.segment,
        "{site:?}: the reopened WAL selects the segment the retry installed, \
         not a file the failed attempt left",
    );
}

/// The debris half: a site at or past the create leaves a
/// `<uuid>.wal.partial`, which is never selectable and which one capped
/// housekeeping pass removes.
fn a_failed_attempts_partial_is_swept(site: RotationSite) {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let mut wal = wal_due_for_rotation(root, BUDGET, RotationFaults::failing(site, 1));

    wal.append(FrameKind::OtlpBatch, b"the append that meets the fault")
        .expect_err("the armed site fails this rotation");
    assert_eq!(
        partials(root).len(),
        1,
        "{site:?}: the failed attempt left exactly one partial",
    );
    assert_eq!(
        wal.reclaim_state().stale_partials,
        1,
        "{site:?}: and the rotation registered it on the sweep's list, so the \
         pass finds it without listing the directory",
    );

    wal.append(FrameKind::OtlpBatch, b"the append after the fault clears")
        .expect("the retry rotates");
    wal.sync().expect("sync");
    wal.housekeeping(None).expect("a capped pass");
    assert!(
        partials(root).is_empty(),
        "{site:?}: one pass clears one rotation's debris",
    );
    assert_eq!(
        wal.reclaim_state().stale_partials,
        0,
        "{site:?}: and the sweep's list is drained with it",
    );
}

/// Scenario RFC0052.4 — closing-segment `fdatasync` fails once.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_4_closing_sync_fails_once_then_the_next_append_rotates() {
    a_pre_rename_site_recovers_on_the_next_append(RotationSite::CloseSync);
}

/// Scenario RFC0052.4 — `create_fresh_segment` fails once.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_4_create_fails_once_then_the_next_append_rotates() {
    a_pre_rename_site_recovers_on_the_next_append(RotationSite::Create);
}

/// Scenario RFC0052.4 — fresh-segment header `fsync` fails once.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
///
/// The partial this one leaves may hold torn header bytes, which is
/// exactly why it must never be selectable — asserted by the reopen in
/// the shared leg above, and by the sweep here.
#[test]
fn rfc0052_4_header_sync_fails_once_then_the_next_append_rotates() {
    a_pre_rename_site_recovers_on_the_next_append(RotationSite::HeaderSync);
    a_failed_attempts_partial_is_swept(RotationSite::HeaderSync);
}

/// Scenario RFC0052.4 — `rename(partial, final)` fails once.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_4_rename_fails_once_then_the_next_append_rotates() {
    a_pre_rename_site_recovers_on_the_next_append(RotationSite::Rename);
    a_failed_attempts_partial_is_swept(RotationSite::Rename);
}

/// Scenario RFC0052.4 — post-rename parent-directory `fsync` fails once.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_4_post_rename_dir_fsync_fails_once_then_sync_discharges_it() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let mut wal = wal_due_for_rotation(
        root,
        BUDGET,
        RotationFaults::failing(RotationSite::ParentFsync, 1),
    );
    let before = segment_files(root).len();

    let err = wal
        .append(FrameKind::OtlpBatch, b"the append that meets the fault")
        .expect_err("the parent fsync fails this rotation");
    assert!(
        matches!(err, AppendError::RotationRetrying(_)),
        "inside the budget, got {err:?}",
    );
    assert_eq!(
        segment_files(root).len(),
        before + 1,
        "the rename already landed: the installed segment is kept, never unlinked",
    );
    assert!(
        partials(root).is_empty(),
        "and it is a real segment, not a partial",
    );

    // `rotate` is not re-entered: the fresh segment is current, so the
    // next append's rotation check is false and it simply writes its
    // frame. What retries is the `sync` that follows.
    let landed = wal
        .append(FrameKind::OtlpBatch, b"lands in the installed segment")
        .expect("the installed segment is current and accepts appends");
    assert_eq!(
        segment_files(root).len(),
        before + 1,
        "no second rotation happened",
    );
    let durable = wal
        .sync()
        .expect("the next sync discharges the pending directory fsync");
    assert_eq!(
        durable.segment, landed.segment,
        "the batches behind it are acked in the installed segment",
    );
    assert!(
        matches!(wal.reclaim_state().rotation, RotationState::Healthy),
        "discharging the obligation resets the budget",
    );

    drop(wal);
    let mut reopened = Wal::open(config(root, BUDGET)).expect("reopen");
    let after = reopened
        .append(FrameKind::OtlpBatch, b"after reopen")
        .expect("append");
    assert_eq!(
        after.segment, landed.segment,
        "a subsequent open selects the installed segment — it is complete and valid",
    );
    reopened
        .sync()
        .expect("and its first sync discharges that open's own pending fsync");
}

/// Scenario RFC0052.4 — the barrier task's idle rotation recovers the same way.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_4_idle_rotation_failure_recovers_on_the_next_append() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let mut wal = wal_due_for_rotation(
        root,
        BUDGET,
        RotationFaults::failing(RotationSite::Create, 1),
    );
    let before = segment_files(root).len();

    // No append drives this one: the timer's idle rotation does, and it
    // draws on the same budget.
    let err = wal
        .rotate(RotationKind::Discretionary)
        .expect_err("the armed site fails the idle rotation");
    match &err {
        AppendError::RotationRetrying(fault) => assert_eq!(fault.attempts(), 1),
        other => panic!("a timer-triggered failure is retrying too, got {other:?}"),
    }
    assert_eq!(segment_files(root).len(), before, "nothing was installed");

    let accepted = wal
        .append(FrameKind::OtlpBatch, b"the next append re-enters rotate")
        .expect("the retry rotates and the append lands");
    wal.sync().expect("acked");
    let installed = segment_files(root);
    assert_eq!(installed.len(), before + 1);
    assert!(
        installed
            .last()
            .is_some_and(|newest| newest.ends_with(format!("{}.wal", accepted.segment))),
        "the append landed in the segment the retry installed",
    );

    wal.housekeeping(None).expect("a capped pass");
    assert!(
        partials(root).is_empty(),
        "the idle rotation's debris is swept under the same cap",
    );
}

/// Scenario RFC0052.4 — an idle rotation of an empty segment is a no-op.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §3.7.
#[test]
fn rfc0052_4_a_discretionary_rotation_of_an_empty_segment_does_nothing() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let mut wal = Wal::open(config(root, BUDGET)).expect("open");
    wal.rotate(RotationKind::Discretionary)
        .expect("a discretionary rotation of an empty segment is a no-op");
    assert_eq!(
        segment_files(root).len(),
        1,
        "there is no recovery window to bound and nothing to seal",
    );

    // An owed rotation is a requirement, not a preference: it proceeds.
    wal.rotate(RotationKind::Owed).expect("an owed rotation");
    assert_eq!(segment_files(root).len(), 2);
}

/// Scenario RFC0052.4 — debris clears in one pass because the cap is validated at open.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_4_open_refuses_a_cap_below_the_retry_budget() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let refused = Wal::open(WalConfig {
        max_unlinks_per_pass: 2,
        ..config(tmp.path(), 3)
    })
    .expect_err("a cap below the retry budget must be refused");
    match refused {
        OpenError::InvalidConfig { field, detail } => {
            assert_eq!(field, "max_unlinks_per_pass");
            assert!(
                detail.contains("rotation_retry_attempts"),
                "the message names the knob it was validated against: {detail}",
            );
        }
        other => panic!("expected InvalidConfig, got {other:?}"),
    }

    // With a valid config, the debris of a rotation that spends its
    // whole budget clears in one pass — so a persistently retrying node
    // cannot fill its disk with retry debris.
    let root = tmp.path();
    let budget = 3;
    let mut wal = wal_due_for_rotation(
        root,
        budget,
        RotationFaults::always(RotationSite::HeaderSync),
    );
    for _ in 0..budget {
        wal.append(FrameKind::OtlpBatch, b"attempt")
            .expect_err("every attempt fails");
    }
    assert_eq!(
        partials(root).len(),
        budget as usize,
        "one partial per attempt, bounded by the budget",
    );

    wal.housekeeping(None).expect("a capped pass");
    assert!(
        partials(root).is_empty(),
        "one rotation's whole budget of debris clears in one capped pass",
    );
}

proptest! {
    // Each case opens two WALs on a scratch root and drives up to four
    // rotations; keep the case count modest.
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Scenario RFC0052.4 — `proptest` over which site fails and how many times.
    /// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §6.
    #[test]
    fn rfc0052_4_proptest_any_site_failing_within_the_budget_recovers(
        site in proptest::sample::select(RotationSite::ALL.to_vec()),
        failures in 1u32..4,
    ) {
        const PROP_BUDGET: u32 = 4;
        let tmp = tempfile::TempDir::new().expect("temp");
        let root = tmp.path();
        let mut wal =
            wal_due_for_rotation(root, PROP_BUDGET, RotationFaults::failing(site, failures));

        // One attempt per armed failure plus the one that succeeds. The
        // post-rename site is the exception the criterion calls out: its
        // retry is `sync`'s, so the append succeeds and the ack waits.
        let mut acked = 0usize;
        for _ in 0..=failures {
            match wal.append(FrameKind::OtlpBatch, b"frame") {
                Ok(_) => {
                    if wal.sync().is_ok() {
                        acked += 1;
                    }
                }
                Err(e) => prop_assert!(
                    matches!(e, AppendError::RotationRetrying(_)),
                    "inside the budget every failure is retrying, got {e:?}",
                ),
            }
        }
        prop_assert!(acked >= 1, "{site:?}: the WAL recovered and acked a batch");
        prop_assert!(
            matches!(wal.reclaim_state().rotation, RotationState::Healthy),
            "{site:?}: the obligation was discharged, so the budget reset",
        );

        wal.housekeeping(None).expect("a capped pass");
        prop_assert!(partials(root).is_empty(), "{site:?}: no partial survives the sweep");

        drop(wal);
        Wal::open(config(root, PROP_BUDGET)).expect("Wal::open succeeds afterwards");
    }
}
