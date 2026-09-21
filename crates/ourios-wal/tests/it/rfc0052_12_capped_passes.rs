//! RFC0052.12 — A reclamation pass bounds its per-file work.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! Per RFC 0052 §6 the capped-pass and rotation-retry tests together
//! **replace** `rfc0008_6_rotation_failure_quiesces_the_wal`, whose
//! "even after the underlying condition clears" assertion is the
//! contract §3.3 changes. That replacement needs explicit approval
//! (`CLAUDE.md` §6.2) and lands with slice C, not here: the original
//! stays untouched while the RFC is red.

/// Scenario RFC0052.12 — a pass unlinks at most the cap and reads nothing.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.12 stub — implemented in the housekeeping green slice B (eligible queue popped at most cap per pass, no header reads, no listing)"]
fn rfc0052_12_pass_unlinks_at_most_the_cap() {
    todo!(
        "RFC0052.12 — a backlog far larger than max_unlinks_per_pass \
         (the incident's 1,113 segments is the shape); one housekeeping \
         pass unlinks at most the cap and returns having read no segment \
         header and listed no directory; successive passes drain the \
         backlog to the same end state an uncapped pass would reach"
    );
}

/// Scenario RFC0052.12 — an append is never held across the file half.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.12 stub — implemented in the housekeeping green slice B (housekeeping_prepare / housekeeping_commit split; regression guard, not wall clock)"]
fn rfc0052_12_append_completes_while_file_half_is_held() {
    todo!(
        "RFC0052.12 — with the file half held at a fault-injection point \
         between housekeeping_prepare and housekeeping_commit (journal \
         guard released), a concurrent append completes; an append taken \
         against the ledger half waits for O(cap) work whatever the \
         backlog, asserted by timing out the append against an uncapped \
         pass in a regression guard"
    );
}

/// Scenario RFC0052.12 — horizon application is capped and resumed per tenant.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.12 stub — implemented in the housekeeping green slice B (per-tenant horizon cursor, O(1) checkpoint advance)"]
fn rfc0052_12_horizon_application_is_capped_and_resumes() {
    todo!(
        "RFC0052.12 — a tenant catching up after a long outage applies \
         at most the cap's worth of segments per pass, resumed from a \
         per-tenant cursor next pass; an unchanged pinned backlog costs \
         none; a checkpoint advance promotes nothing eagerly, and a \
         segment whose set emptied above the old checkpoint is reclaimed \
         by the first pass after the mark passes it"
    );
}

/// Scenario RFC0052.12 — a pinned oldest segment does not shadow a later one.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.12 stub — implemented in the housekeeping green slice B (eligibility is per segment, not a prefix scan)"]
fn rfc0052_12_pinned_oldest_does_not_shadow_a_later_eligible_segment() {
    todo!(
        "RFC0052.12 — with the oldest segment pinned by a tenant without \
         a snapshot and a later segment fully covered, the pass reclaims \
         the later segment on its first tick"
    );
}

/// Scenario RFC0052.12 — stale partials share the cap with segments.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.12 stub — implemented in the housekeeping green slice B (partial list seeded at recovery, swept under the same cap)"]
fn rfc0052_12_partials_are_swept_first_under_the_same_cap() {
    todo!(
        "RFC0052.12 — a segment backlog larger than the cap beside \
         stale .wal.partial files left by a previous process: the first \
         pass removes every partial from the list seeded at recovery \
         (no directory listing on the pass) and spends only \
         cap-minus-partials on segments; a backlog of stale temporaries \
         alone is bounded by the same cap"
    );
}
