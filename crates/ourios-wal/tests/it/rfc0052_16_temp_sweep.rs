//! RFC0052.16 — The temp sweep touches only files of the reserved
//! partial shape.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! A directory fixture rather than a live rotation (RFC 0052 §6): the
//! point is the *selector*, and the dangerous neighbours
//! (`CHECKPOINT.tmp`, `*.snap.tmp`) are produced by other subsystems.

use crate::rfc0052_support::{CHECKPOINT, RECLAIM, open, segment_files};

/// Scenario RFC0052.16 — exactly one of four file kinds is removed.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
///
/// The parent-directory fsync that follows the unlink is on the same
/// code path as the unlink itself and is not separately injectable
/// from here; what this asserts is the selector, that the swept file
/// leaves the in-memory list, and that a partial the sweep *cannot*
/// remove stays on the list for the next pass rather than being
/// silently forgotten.
#[test]
fn rfc0052_16_only_the_partial_is_unlinked() {
    // Given: a WAL root holding a `CHECKPOINT.tmp` and a `RECLAIM`, a
    // snapshots directory holding a `*.snap.tmp`, and a stale
    // `<uuid>.wal.partial`.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let mut wal = open(root);
    drop(wal);

    let checkpoint_tmp = root.join(format!("{CHECKPOINT}.tmp"));
    std::fs::write(&checkpoint_tmp, b"an in-progress checkpoint").expect("checkpoint temp");
    let snapshots = root.join("snapshots");
    std::fs::create_dir_all(&snapshots).expect("snapshots root");
    let snap_tmp = snapshots.join("checkout.42.snap.tmp");
    std::fs::write(&snap_tmp, b"an in-progress snapshot").expect("snapshot temp");
    let partial = root.join(format!("{}.wal.partial", uuid::Uuid::now_v7()));
    std::fs::write(&partial, b"rotation debris").expect("stale partial");
    // A name outside the reserved shape: the sweep cannot tell debris
    // from an operator's file under a reserved name, so it keys on the
    // shape and ignores everything else.
    let foreign = root.join("foo.wal.partial");
    std::fs::write(&foreign, b"not a segment name").expect("foreign partial");

    // When: a housekeeping pass runs.
    wal = open(root);
    wal.rebuild_ledger().expect("seed the partial list");
    assert_eq!(
        wal.reclaim_state().stale_partials,
        1,
        "only the reserved shape is seeded, so `foo.wal.partial` is never a candidate",
    );
    wal.housekeeping(None).expect("housekeeping");

    // Then: only the partial is unlinked.
    assert!(!partial.exists(), "the stale partial is swept");
    assert!(checkpoint_tmp.exists(), "the checkpoint temp survives");
    assert!(snap_tmp.exists(), "the snapshot temp survives");
    assert!(root.join(RECLAIM).exists(), "the reclaim record survives");
    assert!(
        foreign.exists(),
        "a name outside the reserved shape survives"
    );
    assert_eq!(segment_files(root).len(), 1, "the sweep touches no segment");
    assert_eq!(
        wal.reclaim_state().stale_partials,
        0,
        "a swept partial leaves the list",
    );

    // And: a partial the sweep cannot remove stays on the list, so it
    // is retried on the next pass rather than lost.
    let stuck = root.join(format!("{}.wal.partial", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(stuck.join("occupied")).expect("an unlinkable partial");
    let mut wal = open(root);
    wal.rebuild_ledger().expect("seed the partial list");
    wal.housekeeping(None).expect("housekeeping");
    assert!(stuck.exists(), "the failed unlink leaves the file in place");
    assert_eq!(
        wal.reclaim_state().stale_partials,
        1,
        "and keeps it on the list for the next pass",
    );
}
