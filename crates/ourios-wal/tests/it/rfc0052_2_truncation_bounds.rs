//! RFC0052.2 — Segments are reclaimed, and never past the *minimum*
//! tenant snapshot floor.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! Placement note: the retain rule is `ourios-wal` housekeeping over
//! the `RECLAIM` ledger (RFC 0052 §3.2), so the file-survival
//! assertions live beside the other WAL directory tests rather than
//! with the ingester's barrier tests. The floor case is the one that
//! matters (§6): "segments disappear" passes on a bound that ignores
//! the floor.

/// Scenario RFC0052.2 — two tenants, the lagging horizon below the checkpoint.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.2 stub — implemented in the housekeeping green slice B (ledger-driven eligibility + per-tenant horizons)"]
fn rfc0052_2_only_segments_under_every_horizon_are_unlinked() {
    todo!(
        "RFC0052.2 — WAL with several closed segments, a checkpoint \
         above them, two tenants whose horizons differ with the lagging \
         one below the checkpoint; housekeeping runs: exactly the \
         segments at or below the checkpoint whose every tenant's last \
         frame is at or below that tenant's horizon are unlinked, the \
         append segment survives, the segment count falls, a segment \
         holding a frame above the lagging tenant's horizon is still \
         present and a restart re-mines it"
    );
}

/// Scenario RFC0052.2 — an idle tenant's horizon frame does not pin its segment.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.2 stub — implemented in the housekeeping green slice B (inclusive horizon bound + RECLAIM explains the absent segment)"]
fn rfc0052_2_horizon_frame_segment_is_unlinked_and_restart_is_clean() {
    todo!(
        "RFC0052.2 — a segment whose only frame for an idle tenant is \
         exactly that tenant's horizon frame is unlinked; a restart \
         restores the tenant from its snapshot with no stale-gap report, \
         the RECLAIM entry at or above S explaining the absent segment; \
         a segment holding a frame above the horizon is retained until \
         the horizon reaches it"
    );
}

/// Scenario RFC0052.2 — a snapshot-less tenant pins only its own segments.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.2 stub — implemented in the housekeeping green slice B (Pinned floor is per tenant, temp sweep still runs)"]
fn rfc0052_2_pinned_tenant_retains_only_its_segments() {
    todo!(
        "RFC0052.2 — a tenant with WAL data and no valid snapshot: the \
         pass retains exactly that tenant's segments (RFC0052.13's pin), \
         still reclaims a later segment holding only other tenants' \
         covered frames, and the .wal.partial sweep still runs"
    );
}
