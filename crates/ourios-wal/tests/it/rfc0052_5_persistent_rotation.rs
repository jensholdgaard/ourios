//! RFC0052.5 — A persistent rotation failure gives up distinguishably,
//! and never acks.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! The always-failing half of the §6 fault-injection matrix: the two
//! halves `rfc0008_6_rotation_failure_quiesces_the_wal` protected — no
//! ack on an incomplete rotation, a permanent refusal when the fault is
//! persistent — are kept here, on the bounded budget. The transport
//! classification of that terminal state is RFC0052.15's, in the
//! ingester harness.

use std::path::{Path, PathBuf};

use proptest::prelude::*;

use ourios_wal::{
    AppendError, FrameKind, RotationFault, RotationFaults, RotationSite, RotationState, SyncError,
    Wal, WalConfig,
};

use crate::rfc0052_support::backdate_segment;

const BUDGET: u32 = 3;

fn config(root: &Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: ourios_wal::MIN_SEGMENT_SIZE_BYTES,
        segment_age_secs: 1,
        housekeeping_secs: 60,
        max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
        rotation_retry_attempts: BUDGET,
        macos_full_fsync: false,
    }
}

/// A WAL whose next `append` rotates, with `site` failing on every
/// attempt.
fn wal_failing_always(root: &Path, site: RotationSite) -> Wal {
    let mut seed = Wal::open(config(root)).expect("seed the root");
    seed.append(FrameKind::OtlpBatch, b"seed")
        .expect("seed frame");
    seed.sync().expect("seed sync");
    drop(seed);
    backdate_segment(root, std::time::Duration::from_secs(5));

    let mut wal = Wal::open(config(root)).expect("reopen");
    wal.rebuild_ledger().expect("ledger");
    wal.arm_rotation_faults(RotationFaults::always(site));
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

fn terminal_fault(state: &RotationState) -> &RotationFault {
    match state {
        RotationState::Terminal(fault) => fault,
        other => panic!("expected the terminal state, got {other:?}"),
    }
}

/// Append once, expecting a refusal from *inside* the budget, and
/// report how much of it that attempt had spent.
fn retrying_attempts(wal: &mut Wal) -> u32 {
    match wal.append(FrameKind::OtlpBatch, b"attempt") {
        Err(AppendError::RotationRetrying(fault)) => fault.attempts(),
        other => panic!("expected a retrying rotation failure, got {other:?}"),
    }
}

/// Append once, expecting the terminal refusal, and report its fault.
fn terminal_refusal(wal: &mut Wal) -> RotationFault {
    match wal.append(FrameKind::OtlpBatch, b"attempt") {
        Err(AppendError::RotationTerminal(fault)) => fault,
        other => panic!("expected the terminal state, got {other:?}"),
    }
}

/// Append into the installed segment, then `sync` — the post-rename
/// site's retry — expecting the discharge to fail, and report its
/// error. The append must succeed: the segment is complete and current,
/// and it is the *ack* that is refused.
fn failed_discharge(wal: &mut Wal) -> SyncError {
    wal.append(FrameKind::OtlpBatch, b"lands in the installed segment")
        .expect("appends land: the segment is complete and current");
    wal.sync()
        .expect_err("the discharge fails, so nothing is acked")
}

/// Scenario RFC0052.5 — every append past the budget is refused as terminal.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_5_appends_past_the_budget_are_refused_as_terminal() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let mut wal = wal_failing_always(root, RotationSite::CloseSync);

    // Every attempt inside the budget is reported as transient: a later
    // append genuinely could have succeeded.
    let inside: Vec<u32> = (1..BUDGET).map(|_| retrying_attempts(&mut wal)).collect();
    assert_eq!(
        inside,
        (1..BUDGET).collect::<Vec<_>>(),
        "each attempt spends exactly one unit of the budget",
    );

    // The attempt that spends the last unit is the one that turns
    // terminal.
    let fault = terminal_refusal(&mut wal);
    assert_eq!(fault.attempts(), BUDGET);
    assert_eq!(
        fault.op(),
        RotationSite::CloseSync.op(),
        "the first underlying failure is still named, not replaced by a generic quiesce",
    );
    assert!(
        fault.detail().contains(RotationSite::CloseSync.op()),
        "and its cause is still recoverable from the reported state: {}",
        fault.detail(),
    );

    // Every append after it is refused without another attempt on a
    // disk that has already failed the same step its whole budget over.
    let past: Vec<u32> = (0..3)
        .map(|_| terminal_refusal(&mut wal).attempts())
        .collect();
    assert_eq!(past, vec![BUDGET; 3], "a refused append makes no attempt");
    assert_eq!(
        terminal_fault(&wal.reclaim_state().rotation).attempts(),
        BUDGET
    );
}

/// Scenario RFC0052.5 — a persistently failing directory-fsync discharge is terminal too.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_5_persistent_dir_fsync_discharge_is_terminal_and_never_acks() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let mut wal = wal_failing_always(root, RotationSite::ParentFsync);

    // The rotation's own parent fsync is the first failed attempt; the
    // rename already landed, so the segment is installed and current.
    let err = wal
        .append(FrameKind::OtlpBatch, b"the append that rotates")
        .expect_err("the parent fsync fails");
    assert!(matches!(err, AppendError::RotationRetrying(_)), "{err:?}");

    // Each later `sync` retries that one fsync and charges the same
    // budget. Nothing behind it is acked while the directory entry is
    // not durable.
    let inside: Vec<u32> = (2..BUDGET)
        .map(|_| match failed_discharge(&mut wal) {
            SyncError::RotationRetrying(fault) => fault.attempts(),
            other => panic!("expected a retrying discharge, got {other:?}"),
        })
        .collect();
    assert_eq!(inside, (2..BUDGET).collect::<Vec<_>>());

    let fault = match failed_discharge(&mut wal) {
        SyncError::RotationTerminal(fault) => fault,
        other => panic!("the exhausting discharge is terminal, got {other:?}"),
    };
    assert_eq!(fault.attempts(), BUDGET);
    assert_eq!(fault.op(), RotationSite::ParentFsync.op());

    // Terminal is terminal on both surfaces, and nothing is ever acked.
    assert!(
        matches!(
            wal.append(FrameKind::OtlpBatch, b"after"),
            Err(AppendError::RotationTerminal(_)),
        ),
        "appends are refused",
    );
    let later: Vec<u32> = (0..3)
        .map(
            |_| match wal.sync().expect_err("and no batch is acknowledged") {
                SyncError::RotationTerminal(fault) => fault.attempts(),
                other => panic!("expected the terminal state, got {other:?}"),
            },
        )
        .collect();
    assert_eq!(later, vec![BUDGET; 3], "a refused sync makes no attempt");
}

/// Scenario RFC0052.5 — `Wal::open` on the resulting directory succeeds.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_5_open_succeeds_after_the_terminal_state() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let mut wal = wal_failing_always(root, RotationSite::HeaderSync);
    for _ in 0..BUDGET {
        wal.append(FrameKind::OtlpBatch, b"attempt")
            .expect_err("every attempt fails");
    }
    assert!(
        matches!(wal.reclaim_state().rotation, RotationState::Terminal(_)),
        "the budget is spent",
    );
    // The cleanup of the last attempt's temporary file never completed:
    // the process died terminal with its debris still on disk.
    assert_eq!(partials(root).len(), BUDGET as usize);
    drop(wal);

    let mut reopened =
        Wal::open(config(root)).expect("open succeeds rather than reporting corruption");
    reopened.rebuild_ledger().expect("ledger");
    assert_eq!(
        reopened.reclaim_state().stale_partials,
        BUDGET as usize,
        "the restart finds the debris without a pass listing the directory",
    );
    reopened.housekeeping(None).expect("the first pass");
    assert!(
        partials(root).is_empty(),
        "and the surviving partials are swept by it",
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// Scenario RFC0052.5 — `proptest`: any site failing past the budget is terminal.
    /// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §6.
    #[test]
    fn rfc0052_5_proptest_any_site_always_failing_is_terminal_and_reopenable(
        site in proptest::sample::select(RotationSite::ALL.to_vec()),
    ) {
        let tmp = tempfile::TempDir::new().expect("temp");
        let root = tmp.path();
        let mut wal = wal_failing_always(root, site);

        // The post-rename site reaches the terminal state through
        // `sync` — its appends land in the installed segment and it is
        // the ack that is refused — every other through `append`.
        // Driving both each turn covers the five sites without
        // branching on which is which; one extra turn past the budget
        // proves no further attempt is charged. A `sync` after a
        // *failed* append is not counted: it can legitimately succeed
        // on the unchanged old segment, and it acks nothing new.
        let mut acked = 0usize;
        for _ in 0..=BUDGET {
            if wal.append(FrameKind::OtlpBatch, b"frame").is_ok() && wal.sync().is_ok() {
                acked += 1;
            }
        }
        prop_assert_eq!(acked, 0, "{:?}: no batch this WAL appended is ever acked", site);

        let state = wal.reclaim_state().rotation;
        let fault = match &state {
            RotationState::Terminal(fault) => fault,
            other => panic!("{site:?}: expected the terminal state, got {other:?}"),
        };
        prop_assert_eq!(
            fault.attempts(),
            BUDGET,
            "{:?}: terminal after exactly rotation_retry_attempts",
            site,
        );
        prop_assert_eq!(fault.op(), site.op(), "{:?}: the first error is preserved", site);

        drop(wal);
        Wal::open(config(root)).expect("Wal::open succeeds afterwards");
    }
}
