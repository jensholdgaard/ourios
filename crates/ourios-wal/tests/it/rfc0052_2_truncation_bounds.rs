//! RFC0052.2 — Segments are reclaimed, and never past the *minimum*
//! tenant snapshot floor.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Placement note: the retain rule is `ourios-wal` housekeeping over
//! the `RECLAIM` ledger (RFC 0052 §3.2), so the file-survival
//! assertions live beside the other WAL directory tests rather than
//! with the ingester's barrier tests. The floor case is the one that
//! matters (§6): "segments disappear" passes on a bound that ignores
//! the floor.

use ourios_wal::{FrameKind, FrameSink, RecoveryError, RetainFloor, WalOffset};

use crate::rfc0052_support::{build_tenant_segment, known, open, segment_files, write_partial};

/// Every tenant frame replay delivered, so the leg that asserts a
/// retained frame asserts it is still *readable*, not merely that a
/// file is still present.
#[derive(Default)]
struct TenantSink {
    frames: Vec<(String, Vec<u8>)>,
}

impl FrameSink for TenantSink {
    fn consume(
        &mut self,
        _offset: WalOffset,
        _kind: FrameKind,
        payload: &[u8],
    ) -> Result<(), RecoveryError> {
        let batch = ourios_wal::TenantBatch::decode(payload).expect("tenant frame");
        self.frames
            .push((batch.tenant.to_owned(), batch.protobuf.to_vec()));
        Ok(())
    }
}

/// The cap every leg here runs under: far above the fixtures, so a
/// pass's own bound never masks the retain rule under test.
const CAP: usize = 64;

/// Scenario RFC0052.2 — two tenants, the lagging horizon below the checkpoint.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_2_only_segments_under_every_horizon_are_unlinked() {
    // Given: three closed segments shared by two tenants, a checkpoint
    // above the first two, and horizons that differ — `beta` lags
    // below the checkpoint while `alpha` is caught up.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let a = build_tenant_segment(root, &[("alpha", b"a1"), ("beta", b"b1")]);
    let b = build_tenant_segment(root, &[("alpha", b"a2"), ("beta", b"b2")]);
    build_tenant_segment(root, &[("alpha", b"a3")]);
    let before = segment_files(root);
    assert_eq!(before.len(), 3, "fixture: three segments");

    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(b[1]).expect("checkpoint past A and B");

    // When: housekeeping runs with `beta` still folded only through A.
    let progress = wal
        .housekeeping_pass(&known(&[("alpha", b[0]), ("beta", a[1])]), CAP)
        .expect("housekeeping");

    // Then: A goes — every tenant's last frame in it is at or below
    // that tenant's horizon — while B holds `b2`, above `beta`'s
    // horizon, and the current append segment is never a candidate.
    assert_eq!(progress.removed_segments, 1);
    assert_eq!(
        segment_files(root),
        before[1..].to_vec(),
        "only the segment every tenant's horizon covers is unlinked",
    );
    assert_eq!(
        progress.floor,
        RetainFloor::Min(a[1]),
        "the floor is the minimum over the horizons, not the latest",
    );

    // And: the frame above the lagging tenant's horizon is still
    // there, so a restart re-mines it rather than losing `beta`'s
    // miner state.
    drop(wal);
    let mut sink = TenantSink::default();
    open(root).replay(&mut sink).expect("replay");
    assert!(
        sink.frames.contains(&("beta".to_owned(), b"b2".to_vec())),
        "b2 survives for the lagging tenant to re-mine: {:?}",
        sink.frames,
    );
}

/// Scenario RFC0052.2 — an idle tenant's horizon frame does not pin its segment.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_2_horizon_frame_segment_is_unlinked_and_restart_is_clean() {
    // Given: a segment whose only frame for an idle tenant is exactly
    // that tenant's horizon frame, and a later one holding two frames
    // for a tenant whose horizon reaches only the first.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let idle = build_tenant_segment(root, &[("idle", b"i1")]);
    let busy = build_tenant_segment(root, &[("alpha", b"b1"), ("alpha", b"b2")]);
    build_tenant_segment(root, &[("alpha", b"c1")]);
    let before = segment_files(root);

    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(busy[1]).expect("checkpoint past both");

    // When: the pass runs with the idle tenant's horizon exactly at
    // its own last frame.
    let first = wal
        .housekeeping_pass(&known(&[("idle", idle[0]), ("alpha", busy[0])]), CAP)
        .expect("housekeeping");

    // Then: the horizon is durably installed by construction, so its
    // own segment is not needed and goes; the segment holding a frame
    // above `alpha`'s horizon is retained until the horizon reaches it.
    assert_eq!(first.removed_segments, 1);
    assert_eq!(
        segment_files(root),
        before[1..].to_vec(),
        "the horizon frame's segment is unlinked, the straddling one is not",
    );
    let second = wal
        .housekeeping_pass(&known(&[("idle", idle[0]), ("alpha", busy[1])]), CAP)
        .expect("housekeeping");
    assert_eq!(second.removed_segments, 1, "the pin lifts with the horizon");

    // And: a restart restores the idle tenant from its snapshot with
    // no stale-gap report — the `RECLAIM` entry at or above `S` is
    // what explains the absent segment.
    drop(wal);
    let mut restarted = open(root);
    restarted.rebuild_ledger().expect("ledger");
    restarted
        .housekeeping_prepare(&known(&[("idle", idle[0]), ("alpha", busy[1])]), CAP)
        .expect("a restorable horizon at S satisfies the entry");

    // And the same entry is what refuses a start whose snapshot is
    // gone: absence would otherwise read as "nothing reclaimed".
    let refused = restarted
        .housekeeping_prepare(&known(&[("alpha", busy[1])]), CAP)
        .expect_err("the entry for `idle` must be satisfied");
    assert!(
        format!("{refused}").contains("idle"),
        "the refusal names the tenant: {refused}",
    );
}

/// Scenario RFC0052.2 — a snapshot-less tenant pins only its own segments.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_2_pinned_tenant_retains_only_its_segments() {
    // Given: a tenant with WAL data and no valid snapshot in the
    // oldest segment, a covered tenant in the next, and rotation
    // debris beside them.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    build_tenant_segment(root, &[("pinned", b"p1")]);
    let covered = build_tenant_segment(root, &[("alpha", b"a1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);
    let before = segment_files(root);
    let debris = write_partial(root);

    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(covered[0]).expect("checkpoint past both");

    // When: the pass runs with no horizon for `pinned`.
    let progress = wal
        .housekeeping_pass(&known(&[("alpha", covered[0])]), CAP)
        .expect("housekeeping");

    // Then: exactly that tenant's segment is retained, the later one
    // holding only covered frames is reclaimed whatever its offset,
    // and the temp sweep still runs.
    assert_eq!(progress.removed_segments, 1);
    assert_eq!(progress.removed_partials, 1);
    assert!(!debris.exists(), "the .wal.partial sweep still runs");
    let after = segment_files(root);
    assert_eq!(
        after,
        vec![before[0].clone(), before[2].clone()],
        "the pinning tenant's segment survives; the covered one does not",
    );
    assert_eq!(
        progress.floor.pinned_tenants(),
        1,
        "and the floor says a tenant is holding it down: {:?}",
        progress.floor,
    );
}
