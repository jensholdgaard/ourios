//! RFC0052.16 — The temp sweep touches only files of the reserved
//! partial shape.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! A directory fixture rather than a live rotation (RFC 0052 §6): the
//! point is the *selector*, and the dangerous neighbours
//! (`CHECKPOINT.tmp`, `*.snap.tmp`) are produced by other subsystems.

/// Scenario RFC0052.16 — exactly one of four file kinds is removed.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.16 stub — implemented in the reclaim-record green slice A (.wal.partial selector + parent fsync after unlink)"]
fn rfc0052_16_only_the_partial_is_unlinked() {
    todo!(
        "RFC0052.16 — a WAL root holding CHECKPOINT.tmp and RECLAIM, a \
         snapshots directory holding a *.snap.tmp, and a stale \
         <uuid>.wal.partial; a housekeeping pass runs: only the partial \
         is unlinked, the checkpoint temp, the reclaim record and the \
         snapshot temp survive, and the unlink is followed by a \
         parent-directory fsync so a crash cannot resurrect it"
    );
}
