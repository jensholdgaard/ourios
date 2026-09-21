//! RFC0052.15 — Only the terminal rotation state is reported
//! server-terminal, client-retryable.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! The classifier tests held on `hold/794-wedged-classification`
//! (`a_quiesced_wal_classifies_as_wedged_not_transient`,
//! `the_append_that_wedged_the_wal_is_also_wedged_not_transient`,
//! `other_append_and_sync_failures_stay_transient`,
//! `an_oversize_batch_is_still_its_own_outcome`) are reintroduced here
//! rewritten to the terminal-only rule (RFC 0052 §6), and extended to
//! assert the *narrowness*: without the third leg the test passes on a
//! blanket removal of `Retry-After`, which would contradict RFC 0018
//! §3.2 rather than amend it. `rfc0018_retryable.rs` keeps the
//! transient class's own assertions.

/// Scenario RFC0052.15 — every rotation failure carries `503` / `UNAVAILABLE`.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.15 stub — implemented in the rotation green slice C (RFC0018.3 holds on both transports for within-budget and terminal)"]
fn rfc0052_15_within_budget_and_terminal_both_carry_503_unavailable() {
    todo!(
        "RFC0052.15 — a rotation failure still within the retry budget \
         and one that has exhausted it, reported on gRPC and HTTP: both \
         carry 503 / UNAVAILABLE, never a non-retryable code that would \
         tell the client to drop an unacked batch"
    );
}

/// Scenario RFC0052.15 — within the budget: transient, message says retrying, no hint.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.15 stub — implemented in the rotation green slice C (IngestFailure::classify keeps a retrying rotation transient)"]
fn rfc0052_15_a_failure_within_the_budget_is_transient_and_says_retrying() {
    todo!(
        "RFC0052.15 — a rotation failure within the budget is classified \
         transient and its message says it is retrying, because a later \
         append genuinely can succeed; no retry hint is carried, since \
         the server schedules no retry of its own"
    );
}

/// Scenario RFC0052.15 — only the terminal state is server-terminal, client-retryable.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.15 stub — implemented in the rotation green slice C (RFC 0018 §3.2 third class; hold/794-wedged-classification @81ee66cd rewritten terminal-only)"]
fn rfc0052_15_only_the_terminal_state_is_server_terminal_client_retryable() {
    todo!(
        "RFC0052.15 — the terminal rotation state, and the append that \
         drove it there, are classified server-terminal, \
         client-retryable — RFC 0018 §3.2's third class — with the \
         message naming that state and still no Retry-After, so a \
         conforming client keeps the batch and backs off exponentially"
    );
}

/// Scenario RFC0052.15 — the reclassification is narrow.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.15 stub — implemented in the rotation green slice C (narrowness leg: append/fsync I/O and oversize keep their own outcomes)"]
fn rfc0052_15_ordinary_append_and_fsync_failures_stay_transient() {
    todo!(
        "RFC0052.15 — an ordinary append or fsync I/O failure stays in \
         the transient class with its retry hint, and an oversize batch \
         is still its own non-retryable outcome, so the change is narrow \
         rather than a blanket change to RFC 0018 §3.2's transient class"
    );
}
