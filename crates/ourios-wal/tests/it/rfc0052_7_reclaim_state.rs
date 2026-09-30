//! RFC0052.7 — the WAL half of the export: the age of the oldest
//! unreclaimed frame and the retain-floor lag on `ReclaimState`.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §3.5.
//!
//! The exporter half (registry names in the exported stream) lives in
//! `ourios-ingester/tests/rfc0052_7_telemetry.rs`.

use std::time::{Duration, SystemTime};

use ourios_wal::{FrameKind, RetainFloor};

use crate::rfc0052_support::{
    backdate_segment, build_closed_segment, build_tenant_segment, known, open,
};

/// §3.5: no frame, no age — an idle WAL holding only the header-only
/// segment `Wal::open` creates must not report a growing age.
#[test]
fn rfc0052_7_an_empty_wal_reports_no_unreclaimed_age() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let mut wal = open(tmp.path());
    wal.rebuild_ledger().expect("ledger");
    let state = wal.reclaim_state();
    assert_eq!(state.unreclaimed_bytes, 0);
    assert_eq!(state.oldest_unreclaimed, None);

    wal.append(FrameKind::OtlpBatch, b"one frame")
        .expect("append");
    wal.sync().expect("sync");
    let minted = wal
        .reclaim_state()
        .oldest_unreclaimed
        .expect("a frame now ages");
    assert!(
        minted <= SystemTime::now(),
        "the proxy is the segment's mint time, never in the future",
    );
}

/// §3.5: the age is the oldest surviving segment's `UUIDv7` time, so a
/// segment minted an hour ago reads at least an hour old however
/// recently the node restarted.
#[test]
fn rfc0052_7_the_age_is_the_oldest_surviving_segments_mint_time() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    build_closed_segment(root, &[b"old"]);
    backdate_segment(root, Duration::from_secs(3_600));
    build_closed_segment(root, &[b"new"]);

    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    let oldest = wal
        .reclaim_state()
        .oldest_unreclaimed
        .expect("frames survive");
    let age = SystemTime::now()
        .duration_since(oldest)
        .expect("minted in the past");
    assert!(
        age >= Duration::from_secs(3_600),
        "the backdated segment governs the age, got {age:?}",
    );
}

/// §3.5: `reclaim_state` carries the lag the last pass derived — zero
/// before any pass (`Unknown`), the held segment's bytes once a tenant
/// with no snapshot pins the floor.
#[test]
fn rfc0052_7_reclaim_state_reports_the_floor_lag() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    build_tenant_segment(root, &[("pinned", b"p1")]);
    let covered = build_tenant_segment(root, &[("alpha", b"a1")]);
    build_tenant_segment(root, &[("alpha", b"a2")]);

    let mut wal = open(root);
    wal.rebuild_ledger().expect("ledger");
    let before = wal.reclaim_state();
    assert_eq!(before.floor, RetainFloor::Unknown);
    assert_eq!((before.lag_bytes, before.lag_segments), (0, 0));

    wal.checkpoint(covered[0]).expect("checkpoint");
    let progress = wal
        .housekeeping_pass(&known(&[("alpha", covered[0])]), 64)
        .expect("pass");
    assert!(matches!(progress.floor, RetainFloor::Pinned { .. }));

    let after = wal.reclaim_state();
    assert_eq!(after.floor, progress.floor);
    assert_eq!(
        (after.lag_bytes, after.lag_segments),
        (progress.lag_bytes, progress.lag_segments),
        "the export is the lag the ledger holds, as the pass reported it",
    );
    assert!(
        after.lag_segments >= 1,
        "the pinned tenant's segment is held back"
    );
}
