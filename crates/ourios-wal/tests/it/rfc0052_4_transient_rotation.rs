//! RFC0052.4 — A transient rotation failure recovers without a restart.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! One leg per §3.3 rotation site — the four the code has today plus
//! the `rename(partial, final)` step the temporary name adds — each
//! failing once, plus the `proptest` §6 asks for so the five sites are
//! not tested only one way. The post-rename site is the odd one: the
//! installed segment is the live append target, so the retry is the
//! directory fsync alone, discharged by `sync`, never a re-`rotate`.

/// Scenario RFC0052.4 — closing-segment `fdatasync` fails once.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.4 stub — implemented in the rotation green slice C (fault injection at the closing-segment sync; retry re-enters rotate)"]
fn rfc0052_4_closing_sync_fails_once_then_the_next_append_rotates() {
    todo!(
        "RFC0052.4 — rotation fails once at the closing-segment \
         fdatasync; the condition clears and a later append arrives: \
         rotate is re-entered and succeeds, the append is accepted and \
         acked, nothing new was left behind, and a subsequent Wal::open \
         selects no file from the failed attempt"
    );
}

/// Scenario RFC0052.4 — `create_fresh_segment` fails once.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.4 stub — implemented in the rotation green slice C (fault injection at create; at most a .wal.partial is left and swept)"]
fn rfc0052_4_create_fails_once_then_the_next_append_rotates() {
    todo!(
        "RFC0052.4 — rotation fails once creating the fresh segment; a \
         later append re-enters rotate, succeeds and is acked; opening \
         the WAL again selects no segment from the failed attempt, and \
         the .wal.partial (if any) is gone after a capped housekeeping \
         pass"
    );
}

/// Scenario RFC0052.4 — fresh-segment header `fsync` fails once.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.4 stub — implemented in the rotation green slice C (fault injection at the header sync; the torn partial is never selectable)"]
fn rfc0052_4_header_sync_fails_once_then_the_next_append_rotates() {
    todo!(
        "RFC0052.4 — rotation fails once fsyncing the fresh segment's \
         header, leaving a .wal.partial with possibly-torn bytes; a \
         later append re-enters rotate and is acked; Wal::open never \
         selects the partial, and the temp sweep removes it"
    );
}

/// Scenario RFC0052.4 — `rename(partial, final)` fails once.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.4 stub — implemented in the rotation green slice C (fault injection at the rename; nothing is installed)"]
fn rfc0052_4_rename_fails_once_then_the_next_append_rotates() {
    todo!(
        "RFC0052.4 — rotation fails once at rename(partial, final); \
         nothing is installed, a later append re-enters rotate and is \
         acked, Wal::open selects no file from the failed attempt, and \
         the partial is swept"
    );
}

/// Scenario RFC0052.4 — post-rename parent-directory `fsync` fails once.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.4 stub — implemented in the rotation green slice C (dir_fsync_pending discharged by sync; the installed segment is never unlinked)"]
fn rfc0052_4_post_rename_dir_fsync_fails_once_then_sync_discharges_it() {
    todo!(
        "RFC0052.4 — the parent-directory fsync after the rename fails \
         once: the installed segment is current, rotate is not \
         re-entered, the next sync discharges the pending directory \
         fsync and the batches behind it are acked; a subsequent open \
         selects the installed segment (complete and valid) and its \
         first sync discharges the pending fsync again"
    );
}

/// Scenario RFC0052.4 — the barrier task's idle rotation recovers the same way.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.4 stub — implemented in the rotation green slice C (Journal::rotate(RotationKind::Discretionary) shares the retry state with the append path)"]
fn rfc0052_4_idle_rotation_failure_recovers_on_the_next_append() {
    todo!(
        "RFC0052.4 — a transient failure in the barrier task's idle \
         rotation (RotationKind::Discretionary) rather than an append's: \
         the condition clears, the next append re-enters rotate and is \
         acked, and the debris is swept under the same cap"
    );
}

/// Scenario RFC0052.4 — debris clears in one pass because the cap is validated at open.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.4 stub — implemented in the rotation green slice C (WalConfig::max_unlinks_per_pass >= rotation_retry_attempts refused at open)"]
fn rfc0052_4_open_refuses_a_cap_below_the_retry_budget() {
    todo!(
        "RFC0052.4 — Wal::open with max_unlinks_per_pass below \
         rotation_retry_attempts is refused; with a valid config the \
         temporary files left by successive failed attempts are gone \
         after one capped housekeeping pass, so a persistently retrying \
         node cannot fill its disk with retry debris"
    );
}

/// Scenario RFC0052.4 — `proptest` over which site fails and how many times.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §6.
#[test]
#[ignore = "RFC0052.4 stub — implemented in the rotation green slice C (proptest over site × failure count within the budget)"]
fn rfc0052_4_proptest_any_site_failing_within_the_budget_recovers() {
    todo!(
        "RFC0052.4 — for any of the five sites and any failure count \
         below rotation_retry_attempts, the WAL recovers on the next \
         append (or sync, for the post-rename site), every batch is \
         acked exactly once, Wal::open succeeds afterwards and no \
         partial survives the sweep"
    );
}
