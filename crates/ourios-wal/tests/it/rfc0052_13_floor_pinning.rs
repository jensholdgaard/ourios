//! RFC0052.13 — A tenant without a snapshot pins the floor, and is never
//! read as unbounded.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Placement note: the `RetainFloor` cases and the churn leg are
//! `ourios-wal` ledger behaviour. The startup leg — a snapshots-root
//! fsync that fails must fail startup — is the ingester's snapshot
//! listing (RFC 0052 §6) and lives in
//! `ourios-ingester/tests/it/rfc0052_13_startup_fsync.rs`.

use ourios_wal::{RetainFloor, SnapshotHorizons};

use crate::rfc0052_support::{build_tenant_segment, known, open, segment_files};

const CAP: usize = 64;

/// Scenario RFC0052.13 — `RetainFloor::Pinned` at the tenant's oldest surviving frame.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_13_pinned_tenant_keeps_every_frame_and_reports_pinned() {
    // Given: a snapshot consumer exists and one tenant with WAL data
    // has no valid snapshot. Its frames sit in the middle segment, so
    // the pass has covered segments both below and above it.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let below = build_tenant_segment(root, &[("alpha", b"a1")]);
    let pinning = build_tenant_segment(root, &[("pinned", b"p1"), ("alpha", b"a2")]);
    let above = build_tenant_segment(root, &[("alpha", b"a3")]);
    build_tenant_segment(root, &[("alpha", b"a4")]);
    let before = segment_files(root);

    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(above[0]).expect("checkpoint past all three");

    // When: housekeeping runs.
    let progress = wal
        .housekeeping_pass(&known(&[("alpha", above[0])]), CAP)
        .expect("housekeeping");

    // Then: every frame of the pinning tenant survives — a segment
    // holding exactly one frame for it survives even though `alpha`'s
    // horizon covers that segment, because a pinned tenant has no
    // horizon and equality can never unlink its frame — while the
    // segments below and above it, holding only covered frames, go.
    assert_eq!(progress.removed_segments, 2);
    assert_eq!(
        segment_files(root),
        vec![before[1].clone(), before[3].clone()],
        "exactly the pinning tenant's segment is retained",
    );
    assert_eq!(
        progress.floor,
        RetainFloor::Pinned {
            offset: pinning[0],
            tenants: 1,
        },
        "the floor is reported as Pinned at the tenant's oldest surviving frame",
    );
    assert!(
        below[0] < pinning[0],
        "fixture: the reclaimed segment really was below the pin",
    );
}

/// Scenario RFC0052.13 — `RetainFloor::Min` once the snapshot lands: the pin lifts.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_13_pin_lifts_when_a_valid_snapshot_is_written() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let pinning = build_tenant_segment(root, &[("pinned", b"p1")]);
    let covered = build_tenant_segment(root, &[("alpha", b"a1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);

    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(covered[0]).expect("checkpoint past both");
    let held = wal
        .housekeeping_pass(&known(&[("alpha", covered[0])]), CAP)
        .expect("housekeeping");
    assert_eq!(segment_files(root).len(), 2, "the pin holds its segment");
    assert_eq!(held.floor.pinned_tenants(), 1);

    // When: a valid snapshot for the pinning tenant is written. Then:
    // the floor becomes Min and the next pass reclaims what the pin
    // had held.
    let lifted = wal
        .housekeeping_pass(
            &known(&[("alpha", covered[0]), ("pinned", pinning[0])]),
            CAP,
        )
        .expect("housekeeping");
    assert_eq!(lifted.removed_segments, 1);
    assert_eq!(lifted.floor, RetainFloor::Min(pinning[0]));
    assert_eq!(segment_files(root).len(), 1);
}

/// Scenario RFC0052.13 — `RetainFloor::None` is not `Pinned`: no-consumer reclaims by checkpoint alone.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_13_pinned_is_not_expressible_as_no_consumer() {
    // The same fixture, told apart only by what the caller claims
    // about its own miner state.
    let fixture = |horizons: &SnapshotHorizons| {
        let tmp = tempfile::TempDir::new().expect("temp");
        let root = tmp.path();
        let first = build_tenant_segment(root, &[("pinned", b"p1")]);
        build_tenant_segment(root, &[("pinned", b"p2")]);
        let mut wal = open(root);
        wal.rebuild_ledger().expect("ledger");
        wal.checkpoint(first[0]).expect("checkpoint");
        let progress = wal.housekeeping_pass(horizons, CAP).expect("housekeeping");
        (progress, segment_files(root).len())
    };

    // No consumer: the checkpoint alone governs and the floor says so.
    let (no_consumer, survivors) = fixture(&SnapshotHorizons::NoConsumer);
    assert_eq!(no_consumer.floor, RetainFloor::None);
    assert_eq!(no_consumer.removed_segments, 1);
    assert_eq!(survivors, 1);

    // A consumer that holds no valid snapshot for the tenant: the
    // frames stay and the floor is a distinct variant an operator can
    // tell apart. A tenant with no valid snapshot under a consumer is
    // never expressed as `NoConsumer`.
    let (pinned, survivors) = fixture(&known(&[]));
    assert_eq!(pinned.removed_segments, 0);
    assert_eq!(survivors, 2);
    let floor = pinned.floor;
    assert!(
        matches!(floor, RetainFloor::Pinned { tenants: 1, .. }),
        "the pinned case is its own variant, not None: {floor:?}",
    );
    assert_ne!(pinned.floor, RetainFloor::None);
}

/// A horizon that **regresses or disappears** — a snapshot that stopped
/// restoring — puts the tenant back behind every one of its segments.
/// Reporting `Pinned` while the segments a higher horizon had already
/// cleared sat in the eligible head would reclaim exactly the frames
/// the pin exists to keep.
#[test]
fn rfc0052_13_a_horizon_that_regresses_re_pins_what_it_had_cleared() {
    // Given: one closed segment holding two frames for a tenant, and a
    // checkpoint that stops between them — so a horizon can clear the
    // segment without the pass being able to reclaim it, and nothing
    // of the tenant's is ever reclaimed. The record therefore holds no
    // entry for it, which is what keeps this leg about the pin rather
    // than about RFC0052.17's halt.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let frames = build_tenant_segment(root, &[("alpha", b"a1"), ("alpha", b"a2")]);
    build_tenant_segment(root, &[("alpha", b"a3")]);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(frames[0]).expect("checkpoint mid-segment");

    // A horizon covering the whole segment arrives and clears it into
    // the eligible head, where the checkpoint still holds it back.
    let cleared = wal
        .housekeeping_pass(&known(&[("alpha", frames[1])]), CAP)
        .expect("housekeeping");
    assert_eq!(cleared.removed_segments, 0, "the mark has not reached it");
    assert_eq!(cleared.floor, RetainFloor::Min(frames[1]));

    // When: the snapshot stops restoring and the mark then passes the
    // segment.
    wal.checkpoint(frames[1]).expect("advance past the segment");
    let pinned = wal
        .housekeeping_pass(&known(&[]), CAP)
        .expect("housekeeping");

    // Then: the cleared segment is held again rather than reclaimed —
    // without the rewind it would still be sitting in the eligible
    // head, at or below the mark, and the pass would take it.
    assert_eq!(pinned.removed_segments, 0);
    assert_eq!(segment_files(root).len(), 2);
    assert_eq!(
        pinned.floor,
        RetainFloor::Pinned {
            offset: frames[0],
            tenants: 1,
        },
        "and the floor is the pin at the tenant's oldest surviving frame",
    );

    // And: the pin lifts again when the snapshot comes back.
    let lifted = wal
        .housekeeping_pass(&known(&[("alpha", frames[1])]), CAP)
        .expect("housekeeping");
    assert_eq!(lifted.removed_segments, 1);
}

/// A plan whose commit never ran leaves its segments marked
/// reclaiming, and §3.7 re-plans those ahead of anything newly
/// eligible — right while horizons are monotone, which §3.7 states as
/// a property of the input. A horizon that regresses in that window
/// would otherwise unlink frames the tenant needs again, so the
/// rewind withdraws the popped entry too.
#[test]
fn rfc0052_13_a_regression_withdraws_a_plan_that_was_never_committed() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let frames = build_tenant_segment(root, &[("alpha", b"a1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);
    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    wal.checkpoint(frames[0]).expect("checkpoint");

    // A pass pops the segment and then dies before its commit.
    let abandoned = wal
        .housekeeping_prepare(&known(&[("alpha", frames[0])]), CAP)
        .expect("prepare");
    assert_eq!(abandoned.segments.len(), 1);

    // The snapshot stops restoring before the next pass.
    let pinned = wal
        .housekeeping_pass(&known(&[]), CAP)
        .expect("housekeeping");
    assert_eq!(
        pinned.removed_segments, 0,
        "the popped entry is withdrawn, not re-planned and unlinked",
    );
    assert_eq!(segment_files(root).len(), 2);
    assert_eq!(pinned.floor.pinned_tenants(), 1);
    assert_eq!(
        pinned.unlink_remaining, 0,
        "and nothing is left marked reclaiming: {pinned:?}",
    );
}

/// Scenario RFC0052.13 — `RetainFloor::Unknown` before the first pass, then the churn leg.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_13_tenant_leaves_the_ledger_with_its_last_segment() {
    // Given: a tenant that writes once and is then never heard from
    // again — the churn shape.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let once = build_tenant_segment(root, &[("ephemeral", b"e1")]);
    let covered = build_tenant_segment(root, &[("alpha", b"a1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);

    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");

    // Then: the floor reads Unknown before any pass has computed it —
    // not None, which would claim no consumer exists.
    assert_eq!(
        wal.reclaim_state().floor,
        RetainFloor::Unknown,
        "no pass has derived a floor yet",
    );

    // When: it is snapshotted and its last surviving segment unlinked.
    wal.checkpoint(covered[0]).expect("checkpoint past both");
    let progress = wal
        .housekeeping_pass(
            &known(&[("ephemeral", once[0]), ("alpha", covered[0])]),
            CAP,
        )
        .expect("housekeeping");
    assert_eq!(progress.removed_segments, 2);

    // Then: it leaves the ledger with that segment, so it no longer
    // holds the floor down at its own long-reclaimed frame. Tenant
    // churn cannot leave a permanent pin.
    assert!(
        once[0] < covered[0],
        "fixture: the departed tenant's frame is the lower of the two",
    );
    let after = wal
        .housekeeping_pass(
            &known(&[("ephemeral", once[0]), ("alpha", covered[0])]),
            CAP,
        )
        .expect("housekeeping");
    assert_eq!(
        after.floor,
        RetainFloor::Min(covered[0]),
        "the minimum is taken over the tenants the ledger still holds",
    );
    assert_eq!(after.floor.pinned_tenants(), 0);
    assert_eq!(
        wal.reclaim_state().floor,
        after.floor,
        "and the export is the floor the last pass derived",
    );
}
