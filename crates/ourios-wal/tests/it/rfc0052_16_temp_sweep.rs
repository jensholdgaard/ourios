//! RFC0052.16 — The temp sweep touches only files of the reserved
//! partial shape.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! A directory fixture rather than a live rotation (RFC 0052 §6): the
//! point is the *selector*, and the dangerous neighbours
//! (`CHECKPOINT.tmp`, `*.snap.tmp`) are produced by other subsystems.

use ourios_wal::FrameKind;

use crate::rfc0052_support::{CHECKPOINT, RECLAIM, build_closed_segment, open, segment_files};

/// RFC 0052 §3.7's unreclaimed-byte figure is kept *incrementally*,
/// the way `unflushed_bytes` already is: seeded from the post-recovery
/// walk, raised by every frame that lands, lowered by every verified
/// unlink. Seeding alone would omit everything written since the last
/// restart and count reclaimed bytes forever, so the export would not
/// be the exact figure RFC 0053's bound is meant to be taken on.
#[test]
fn rfc0052_7_unreclaimed_bytes_rise_on_append_and_fall_on_reclaim() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let first = build_closed_segment(root, &[b"a1", b"a2"]);
    let second = build_closed_segment(root, &[b"b1"]);
    build_closed_segment(root, &[b"c1"]);

    let mut wal = open(root);
    wal.rebuild_ledger().expect("seed the ledger");
    let seeded = wal.reclaim_state().unreclaimed_bytes;
    assert!(seeded > 0, "the walk seeds the surviving frames' bytes");

    // Appending raises it by exactly the frame it wrote.
    let payload = b"a frame that lands after the seed";
    wal.append(FrameKind::OtlpBatch, payload).expect("append");
    wal.sync().expect("sync");
    let grown = wal.reclaim_state().unreclaimed_bytes;
    assert_eq!(
        grown - seeded,
        (payload.len() + 12) as u64,
        "the 12 B frame header plus the payload, and nothing else",
    );

    // Reclaiming lowers it by the bytes the unlinked segments held.
    assert!(!first.is_empty() && !second.is_empty());
    wal.checkpoint(*second.last().expect("segment two"))
        .expect("checkpoint");
    wal.housekeeping(None).expect("housekeeping");
    let survivors = segment_files(root);
    assert_eq!(survivors.len(), 1, "two segments are reclaimed");
    let reclaimed = wal.reclaim_state().unreclaimed_bytes;
    assert!(
        reclaimed < grown,
        "the figure falls with the segments rather than counting them forever",
    );
    let live_frame_bytes = std::fs::metadata(&survivors[0]).expect("stat").len() - 24;
    assert_eq!(
        reclaimed, live_frame_bytes,
        "leaving exactly the surviving segment's frame bytes — its length less its 24 B header",
    );
}

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
