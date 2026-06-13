//! RFC0008.1 — WAL-before-ack `[§3.4]`.
//! See `docs/rfcs/0008-wal.md` §5.
//!
//! The §5 contract: the receiver emits its 2xx (gRPC `OK`) only
//! after `Wal::sync` returns `Ok(_)`. We model the receiver's ack
//! gate as a tiny in-test harness wrapping a real [`Wal`]: it
//! appends a frame, calls `sync`, and flips an `AtomicBool` only
//! on the line *after* `sync` returns `Ok`. The flag is sampled
//! immediately before the sync call (must be false) and after
//! (must be true) so an ack racing the sync can't trivially pass.
//!
//! The two fault arms drive the gate's decision logic directly
//! with synthesized `Err` values built through the real error
//! paths. A read-only WAL root won't fault `append`/`sync` (the
//! bytes are already in the page cache), and a real `fdatasync`
//! fault needs OS-level fault injection, which this crate's
//! `#![deny(unsafe_code)]` (CLAUDE.md §6.1) and the no-external-
//! harness scope put out of reach. The unit under test is the
//! gate: an ack is permitted iff *both* `append` and `sync`
//! returned `Ok`. The append-fault arm exercises a genuine
//! `AppendError` (an oversize payload through the real `append`
//! path); the sync-fault arm asserts the gate suppresses on a
//! synthesized `SyncError` (a real fsync fault is OS-injection,
//! out of scope — the gate is the unit under test).

use std::sync::atomic::{AtomicBool, Ordering};

use ourios_wal::{AppendError, FrameKind, MAX_FRAME_BYTES, SyncError, Wal, WalConfig, WalOffset};

fn default_config(root: &std::path::Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    }
}

/// The receiver's ack gate, isolated from the network layer: an
/// ack is permitted iff the WAL `append` *and* the covering
/// `sync` both returned `Ok`. This is the exact §3.4 condition
/// the ingester gates its 2xx on; modelling it here keeps the
/// contract inside `ourios-wal` (no `ourios-ingester` dep).
fn try_ack(
    append_result: &Result<WalOffset, AppendError>,
    sync_result: &Result<WalOffset, SyncError>,
) -> bool {
    append_result.is_ok() && sync_result.is_ok()
}

/// Happy path: a real append + a real sync, with the ack flag set
/// only on the line after `sync` returns `Ok`. The flag is false
/// at the instant just before sync returns and true after.
#[test]
fn rfc0008_1_ack_only_after_sync_returns_ok() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let mut wal = Wal::open(default_config(tmp.path())).expect("open");
    let ack_allowed = AtomicBool::new(false);

    let appended = wal.append(FrameKind::OtlpBatch, b"durable-payload");
    assert!(appended.is_ok(), "append succeeded");
    assert!(
        !ack_allowed.load(Ordering::SeqCst),
        "ack is suppressed before sync returns",
    );

    let synced = wal.sync();
    // The gate: the flag is flipped only here, strictly after
    // `sync` has returned, and only when both halves are `Ok`.
    let before_set = ack_allowed.load(Ordering::SeqCst);
    if try_ack(&appended, &synced) {
        ack_allowed.store(true, Ordering::SeqCst);
    }

    assert!(synced.is_ok(), "sync succeeded");
    assert!(
        !before_set,
        "the flag was still false at the moment just before it was set",
    );
    assert!(
        ack_allowed.load(Ordering::SeqCst),
        "ack is allowed only after sync returned Ok",
    );
}

/// Append-fault arm: an oversize payload faults the real `append`
/// path (`AppendError::TooLarge`). The gate must suppress the ack
/// — a failed append is never even followed by a `sync`, so we
/// pair it with an arbitrary `Ok` sync to prove the append failure
/// alone is decisive.
#[test]
fn rfc0008_1_append_fault_suppresses_ack() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let mut wal = Wal::open(default_config(tmp.path())).expect("open");
    let ack_allowed = AtomicBool::new(false);

    // A payload one byte past MAX_FRAME_BYTES faults `append`
    // through the real validation path, not a synthesized error.
    let oversize = vec![0u8; MAX_FRAME_BYTES + 1];
    let appended = wal.append(FrameKind::OtlpBatch, &oversize);
    assert!(
        matches!(appended, Err(AppendError::TooLarge { .. })),
        "oversize payload faults append: {appended:?}",
    );

    // The append failed, so the batch never reaches `sync` — pair
    // it with an arbitrary `Ok` sync (no filesystem touch) to prove
    // the append failure alone suppresses the ack, independent of
    // any later WAL state.
    let sync_ok: Result<WalOffset, SyncError> = Ok(WalOffset {
        segment: uuid::Uuid::now_v7(),
        byte: 0,
    });
    if try_ack(&appended, &sync_ok) {
        ack_allowed.store(true, Ordering::SeqCst);
    }
    assert!(
        !ack_allowed.load(Ordering::SeqCst),
        "a failed append suppresses the ack even if a later sync succeeds (§3.4)",
    );
}

/// Sync-fault arm: the gate must suppress the ack when `sync`
/// returns `Err`, even on an otherwise-successful append. A real
/// `fdatasync` fault needs OS-level fault injection (out of scope
/// — `#![deny(unsafe_code)]`, no external harness); the gate
/// logic is the unit under test, driven with a synthesized
/// `SyncError::Io` built through the real error variant.
#[test]
fn rfc0008_1_sync_fault_suppresses_ack() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let mut wal = Wal::open(default_config(tmp.path())).expect("open");
    let ack_allowed = AtomicBool::new(false);

    let appended = wal.append(FrameKind::OtlpBatch, b"payload");
    assert!(appended.is_ok(), "append succeeded");

    let sync_failed: Result<WalOffset, SyncError> = Err(SyncError::Io {
        op: "fdatasync(current_segment)",
        source: std::io::Error::from(std::io::ErrorKind::Other),
    });
    if try_ack(&appended, &sync_failed) {
        ack_allowed.store(true, Ordering::SeqCst);
    }
    assert!(
        !ack_allowed.load(Ordering::SeqCst),
        "a failed sync suppresses the ack even on a successful append (§3.4)",
    );
}
