//! RFC0052.17 — a pre-RFC root stays on the legacy branch until its
//! first version-2 checkpoint, whatever else it writes first.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §3.2, §5.
//!
//! A legacy root rotates long before it checkpoints, and that rotation
//! creates the `RECLAIM` record (§3.2's ordering rule) without moving
//! the checkpoint witness. Reading "no record" as "legacy" would then
//! drop the stale-gap belt from a root that has not migrated — at pass
//! time and, after a restart, at open. A fresh post-RFC root is never
//! held to the belt, before or after it rotates.

use std::collections::HashMap;
use std::path::Path;

use ourios_wal::{
    PassOutcome, RotationKind, SkipReason, SnapshotHorizons, TenantHorizon, Wal, WalOffset,
};

use crate::rfc0052_support::{
    RECLAIM, build_tenant_segment, downgrade_segments, open, tenant_id, write_legacy_checkpoint,
};

/// A pre-RFC root: two version-1 segments holding `alpha`, no record,
/// and — when asked — the version-1 `CHECKPOINT` such a root may carry.
fn legacy_root(root: &Path, with_checkpoint: bool) {
    let frames = build_tenant_segment(root, &[("alpha", b"a1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);
    std::fs::remove_file(root.join(RECLAIM)).expect("a pre-RFC root has no record");
    downgrade_segments(root);
    if with_checkpoint {
        write_legacy_checkpoint(root, frames[0]);
    }
}

fn reopen(root: &Path) -> Wal {
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal
}

/// A recorded horizon below every surviving frame — the stale gap.
fn stale() -> HashMap<ourios_core::tenant::TenantId, TenantHorizon> {
    HashMap::from([(
        tenant_id("alpha"),
        TenantHorizon::RecordedOnly(WalOffset {
            segment: uuid::Uuid::nil(),
            byte: 0,
        }),
    )])
}

/// Both checks refuse the root, naming the tenant.
fn assert_belted(wal: &mut Wal, when: &str) {
    assert!(wal.on_legacy_branch(), "{when}: still on the legacy branch");
    let refused = wal
        .refuse_legacy_stale_gaps_at_open(&stale())
        .expect_err("the startup check refuses the stale gap");
    assert!(format!("{refused}").contains("alpha"), "{when}: {refused}");
    let refused = wal
        .housekeeping_prepare(&SnapshotHorizons::Known(HashMap::new()), 64)
        .expect_err("the pass refuses a tenant no horizon explains");
    assert!(format!("{refused}").contains("alpha"), "{when}: {refused}");
}

#[test]
fn rfc0052_17_a_legacy_root_that_rotated_is_still_belted_before_and_after_a_reopen() {
    for with_checkpoint in [false, true] {
        // Given: a pre-RFC root that rotates before its first
        // version-2 checkpoint, which writes the record.
        let tmp = tempfile::TempDir::new().expect("temp");
        let root = tmp.path();
        legacy_root(root, with_checkpoint);
        let mut wal = reopen(root);
        wal.rotate(RotationKind::Owed).expect("rotate");
        assert!(root.join(RECLAIM).exists(), "the rotation wrote the record");

        // Then: the belt still holds in this process, and after a
        // restart onto the same root.
        let when = format!("v1 checkpoint: {with_checkpoint}");
        assert_belted(&mut wal, &format!("{when}, rotated"));
        drop(wal);
        assert_belted(&mut reopen(root), &format!("{when}, reopened"));
    }
}

#[test]
fn rfc0052_17_a_fresh_root_that_rotated_is_never_belted() {
    // Given: a post-RFC root whose tenant has written, rotated, and has
    // no checkpoint yet.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    build_tenant_segment(root, &[("alpha", b"a1")]);
    let mut wal = reopen(root);
    wal.rotate(RotationKind::Owed).expect("rotate");

    // Then: neither check applies, here or after a restart, and a pass
    // is §3.2's no-checkpoint skip.
    for when in ["rotated", "reopened"] {
        assert!(!wal.on_legacy_branch(), "{when}");
        wal.refuse_legacy_stale_gaps_at_open(&stale())
            .expect("a post-RFC root is not on the legacy branch");
        let progress = wal
            .housekeeping_pass(&SnapshotHorizons::Known(HashMap::new()), 64)
            .expect("a skipped pass, not a refusal");
        assert_eq!(
            progress.outcome,
            PassOutcome::Skipped(SkipReason::NoCheckpoint),
            "{when}"
        );
        drop(wal);
        wal = reopen(root);
    }
}
