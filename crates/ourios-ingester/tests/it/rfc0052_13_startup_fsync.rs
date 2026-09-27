//! RFC0052.13 — the startup leg: a snapshot governs reclamation only once
//! its directory entry is durable in this process.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! Placement note: the `RetainFloor` cases and the churn leg are WAL
//! ledger behaviour and live in
//! `ourios-wal/tests/it/rfc0052_13_floor_pinning.rs`; this leg is the
//! ingester's snapshot listing at startup. The fsync is made to fail
//! with the file-in-place-of-directory technique the rotation tests
//! use (RFC 0052 §6), since read-only permissions do not bind under
//! root.

/// Scenario RFC0052.13 — a failed startup fsync of the snapshots root fails startup.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.13 stub — implemented in the barrier green slice D (snapshots root + parent fsynced before any listed horizon is used)"]
fn rfc0052_13_failed_snapshots_root_fsync_fails_startup_rather_than_pinning() {
    todo!(
        "RFC0052.13 — a snapshots-root fixture whose directory fsync \
         fails at startup: startup fails rather than discarding \
         snapshots whose frames reclamation may already have removed, \
         so a horizon whose directory entry may not be durable never \
         governs reclamation and no state only a snapshot could rebuild \
         is thrown away; with the fsync succeeding, the listed snapshots \
         are used as horizons"
    );
}
