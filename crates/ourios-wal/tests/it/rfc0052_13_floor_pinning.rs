//! RFC0052.13 — A tenant without a snapshot pins the floor, and is never
//! read as unbounded.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! Placement note: the `RetainFloor` cases and the churn leg are
//! `ourios-wal` ledger behaviour. The startup leg — a snapshots-root
//! fsync that fails must fail startup — is the ingester's snapshot
//! listing (RFC 0052 §6) and lives in
//! `ourios-ingester/tests/it/rfc0052_13_startup_fsync.rs`.

/// Scenario RFC0052.13 — `RetainFloor::Pinned` at the tenant's oldest surviving frame.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.13 stub — implemented in the housekeeping green slice B (RetainFloor::Pinned reported, frames retained)"]
fn rfc0052_13_pinned_tenant_keeps_every_frame_and_reports_pinned() {
    todo!(
        "RFC0052.13 — a snapshot consumer exists and one tenant with WAL \
         data has no valid snapshot; housekeeping runs: every frame of \
         that tenant survives, segments below its oldest surviving frame \
         holding only other tenants' covered frames are reclaimed, and \
         the floor is reported as Pinned; a segment holding exactly one \
         frame for the pinning tenant survives (no horizon, so equality \
         can never unlink it) while a later segment holding none of its \
         frames is reclaimed"
    );
}

/// Scenario RFC0052.13 — `RetainFloor::Min` once the snapshot lands: the pin lifts.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.13 stub — implemented in the housekeeping green slice B (horizon install lifts the pin on the next pass)"]
fn rfc0052_13_pin_lifts_when_a_valid_snapshot_is_written() {
    todo!(
        "RFC0052.13 — once a valid snapshot for the pinning tenant is \
         written, the floor becomes Min and the next pass reclaims what \
         the pin had held"
    );
}

/// Scenario RFC0052.13 — `RetainFloor::None` is not `Pinned`: no-consumer reclaims by checkpoint alone.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.13 stub — implemented in the housekeeping green slice B (SnapshotHorizons::NoConsumer vs Pinned are distinct in the API)"]
fn rfc0052_13_pinned_is_not_expressible_as_no_consumer() {
    todo!(
        "RFC0052.13 — the no-consumer case reclaims by checkpoint alone \
         and reports RetainFloor::None; the pinned case is a distinct \
         variant an operator can tell apart, and a tenant with no valid \
         snapshot under a consumer is never expressed as NoConsumer"
    );
}

/// Scenario RFC0052.13 — `RetainFloor::Unknown` before the first pass, then the churn leg.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.13 stub — implemented in the housekeeping green slice B (tenant leaves the ledger with its last surviving segment)"]
fn rfc0052_13_tenant_leaves_the_ledger_with_its_last_segment() {
    todo!(
        "RFC0052.13 — the floor reads Unknown before any pass has \
         computed it; the churn leg writes one tenant once, snapshots \
         it, and asserts it leaves the ledger when its last surviving \
         segment is unlinked, so tenant churn cannot leave a permanent \
         pin"
    );
}
