//! RFC0052.10 — No acknowledged record is lost across the whole cycle.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! This **extends** `rfc0014_5_crash_no_loss.rs` (RFC 0052 §6) rather
//! than adding a parallel crash test: the same SIGKILL of the
//! `wal_crash_fixture` child, with reclamation configured on a short
//! cadence so the kill lands in the regime this RFC introduces. The
//! existing test stays as it is; the legs here are the additional
//! assertions over the same fixture. The #791 refuse-then-resume
//! regression tests move with the bound to RFC 0053.

/// Scenario RFC0052.10 — SIGKILL with reclamation and rotation retry live.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.10 stub — implemented in the crash-and-soak green slice F (rfc0014_5 fixture on a short reclamation cadence)"]
fn rfc0052_10_every_acked_record_survives_a_kill_during_reclamation() {
    todo!(
        "RFC0052.10 — a node killed with SIGKILL mid-batch while \
         reclamation and rotation retry are both live; it restarts and \
         recovery completes: every acknowledged record is present in \
         Parquet, including those whose segments were candidates for \
         reclamation at the moment of the kill"
    );
}

/// Scenario RFC0052.10 — recovery republishes nothing at or below `max(X, S)`.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.10 stub — implemented in the crash-and-soak green slice F (replay into the miner only, never the record sink, at or below the mark)"]
fn rfc0052_10_replay_below_the_mark_feeds_the_miner_but_not_the_record_sink() {
    todo!(
        "RFC0052.10 — a kill following a failed snapshot write and an \
         advanced checkpoint replays frames at or below the checkpoint \
         into the miner only, never into the record sink; the same holds \
         when the snapshot succeeded and the checkpoint write failed so \
         a tenant restarts with S > X: nothing in (X, S] is republished; \
         an at-least-once client retry (RFC0003.2) is not what this leg \
         counts"
    );
}

/// Scenario RFC0052.10 — the audit stream is gated the same way.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.10 stub — implemented in the crash-and-soak green slice F (reference mine with the same snapshot and injected clock)"]
fn rfc0052_10_audit_events_above_the_mark_are_forwarded_exactly_once_in_frame_order() {
    todo!(
        "RFC0052.10 — no template event for a frame at or below X is \
         forwarded again on replay; every event the miner regenerates \
         for a frame above X is forwarded exactly once, in frame order, \
         through the capture sink, with stored AuditEvent frames ignored \
         as a source; the test mines the same frames from the same \
         snapshot with the same injected clock as a reference and \
         asserts the forwarded set equals the reference's events for \
         (X, tail]"
    );
}
