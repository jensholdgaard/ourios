//! RFC0052.5 — A persistent rotation failure gives up distinguishably,
//! and never acks.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! The always-failing half of the §6 fault-injection matrix: the two
//! halves `rfc0008_6_rotation_failure_quiesces_the_wal` protects — no
//! ack on an incomplete rotation, a permanent refusal when the fault is
//! persistent — are kept here, on the bounded budget. The transport
//! classification of that terminal state is RFC0052.15's, in the
//! ingester harness.

/// Scenario RFC0052.5 — every append past the budget is refused as terminal.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.5 stub — implemented in the rotation green slice C (rotation_retry_attempts exhausted → terminal state, first I/O error preserved)"]
fn rfc0052_5_appends_past_the_budget_are_refused_as_terminal() {
    todo!(
        "RFC0052.5 — rotation fails on every attempt in rotate itself; \
         appends continue past rotation_retry_attempts: every append is \
         refused, no batch is acknowledged, the refusal is reported as \
         the terminal state rather than a transient one, and the first \
         underlying I/O error is still recoverable from the reported \
         state rather than replaced by a generic quiesced message"
    );
}

/// Scenario RFC0052.5 — a persistently failing directory-fsync discharge is terminal too.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.5 stub — implemented in the rotation green slice C (a failed Rotation-origin discharge consumes one unit of the same budget)"]
fn rfc0052_5_persistent_dir_fsync_discharge_is_terminal_and_never_acks() {
    todo!(
        "RFC0052.5 — the post-rename directory fsync fails on every \
         sync: each failed discharge consumes one unit of the retry \
         budget, no batch behind it is acked, and once the budget is \
         exhausted the state is terminal with the first I/O error \
         preserved"
    );
}

/// Scenario RFC0052.5 — `Wal::open` on the resulting directory succeeds.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.5 stub — implemented in the rotation green slice C (temporary-name property: the partial is never selectable)"]
fn rfc0052_5_open_succeeds_after_the_terminal_state() {
    todo!(
        "RFC0052.5 — after the budget is exhausted, including when the \
         cleanup of the last attempt's temporary file never completed, \
         Wal::open on the directory succeeds rather than reporting \
         corruption, and the surviving .wal.partial is swept by the \
         first housekeeping pass"
    );
}

/// Scenario RFC0052.5 — `proptest`: any site failing past the budget is terminal.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §6.
#[test]
#[ignore = "RFC0052.5 stub — implemented in the rotation green slice C (proptest over site × always-failing)"]
fn rfc0052_5_proptest_any_site_always_failing_is_terminal_and_reopenable() {
    todo!(
        "RFC0052.5 — for any of the five sites failing on every attempt, \
         the WAL reaches the terminal state after exactly \
         rotation_retry_attempts, no batch is ever acked, the first \
         error is preserved, and Wal::open succeeds afterwards"
    );
}
