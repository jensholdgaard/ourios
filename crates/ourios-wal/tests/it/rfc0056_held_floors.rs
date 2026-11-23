//! RFC 0056 §3.4 — the `CHECKPOINT` carries each held tenant's floor
//! through a reopen. See `docs/rfcs/0056-audit-durability.md`.

use std::path::Path;

use ourios_core::tenant::TenantId;
use ourios_wal::{FrameKind, HeldFloor, HeldFloors, Wal, WalConfig, WalOffset};

fn config(root: &Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
        rotation_retry_attempts: ourios_wal::DEFAULT_ROTATION_RETRY_ATTEMPTS,
        macos_full_fsync: false,
    }
}

/// A WAL in `root` with two synced frames, and their offsets.
fn two_frames(root: &Path) -> (Wal, WalOffset, WalOffset) {
    let mut wal = Wal::open(config(root)).expect("open");
    let first = wal.append(FrameKind::OtlpBatch, b"one").expect("append");
    let second = wal.append(FrameKind::OtlpBatch, b"two").expect("append");
    wal.sync().expect("sync");
    (wal, first, second)
}

fn reopened_floors(root: &Path) -> HeldFloors {
    Wal::open(config(root))
        .expect("reopen")
        .held_floors()
        .clone()
}

#[test]
fn held_floors_survive_a_reopen() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (mut wal, first, second) = two_frames(tmp.path());
    let floors = HeldFloors::from([
        (TenantId::new("acme"), HeldFloor::At(first)),
        (TenantId::new("initech"), HeldFloor::StartOfLog),
    ]);
    wal.checkpoint_with_floors(second, &floors)
        .expect("checkpoint");
    assert_eq!(wal.held_floors(), &floors);
    drop(wal);

    assert_eq!(reopened_floors(tmp.path()), floors);
}

/// An unchanged mark is no reason to keep stale floors: a tenant held
/// while the mark stood still must still reach the sidecar.
#[test]
fn an_equal_mark_with_new_floors_is_written() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (mut wal, first, second) = two_frames(tmp.path());
    wal.checkpoint(second).expect("first checkpoint");
    let floors = HeldFloors::from([(TenantId::new("acme"), HeldFloor::At(first))]);
    wal.checkpoint_with_floors(second, &floors)
        .expect("same mark, new floor");
    drop(wal);

    assert_eq!(reopened_floors(tmp.path()), floors);
}

/// A cut that holds no tenant writes no floors (RFC 0056 §3.4).
#[test]
fn a_checkpoint_without_floors_clears_them() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (mut wal, first, second) = two_frames(tmp.path());
    let floors = HeldFloors::from([(TenantId::new("acme"), HeldFloor::At(first))]);
    wal.checkpoint_with_floors(first, &floors)
        .expect("held checkpoint");
    wal.checkpoint(second).expect("repaired checkpoint");
    assert!(wal.held_floors().is_empty());
    drop(wal);

    assert!(reopened_floors(tmp.path()).is_empty());
}
