//! RFC0008.6 — Segment rotation.
//! See `docs/rfcs/0008-wal.md` §5.
//!
//! Size-cap and time-cap arms; close-fsync + new-header +
//! parent-dir fsync sequence; no drop/duplicate across the
//! rotation boundary; rotation-failure quiesce per §6.5. The
//! size-cap arm works at the §6.9 minimum segment size
//! (17 MiB), so it really writes ~16 MiB — still fast on
//! local disk; the time-cap arm uses the §6.9 minimum age
//! (1 s) and a real sleep, since the segment's age comes from
//! its `UUIDv7` mint time.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ourios_wal::{
    AppendError, FrameKind, FrameSink, MIN_SEGMENT_SIZE_BYTES, RecoveryError, Wal, WalConfig,
    WalOffset,
};

fn config(root: &Path, segment_size_bytes: u64, segment_age_secs: u64) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes,
        segment_age_secs,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    }
}

fn segment_files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(root)
        .expect("read_dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "wal"))
        .collect();
    out.sort();
    out
}

#[derive(Default)]
struct OffsetSink {
    frames: Vec<(WalOffset, Vec<u8>)>,
}

impl FrameSink for OffsetSink {
    fn consume(
        &mut self,
        offset: WalOffset,
        _kind: FrameKind,
        payload: &[u8],
    ) -> Result<(), RecoveryError> {
        self.frames.push((offset, payload.to_vec()));
        Ok(())
    }
}

fn replayed(root: &Path) -> Vec<(WalOffset, Vec<u8>)> {
    let mut sink = OffsetSink::default();
    Wal::open(config(root, MIN_SEGMENT_SIZE_BYTES, 600))
        .expect("reopen")
        .replay(&mut sink)
        .expect("replay");
    sink.frames
}

/// Size-cap arm: a frame that would push the segment past
/// `wal_segment_size_bytes` rotates first — the old segment is
/// closed, a fresh `UUIDv7` segment opens, the frame lands
/// there, and replay sees both frames exactly once, in order.
#[test]
fn rfc0008_6_size_cap_rotates_without_drop_or_duplicate() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let mut wal = Wal::open(config(tmp.path(), MIN_SEGMENT_SIZE_BYTES, 600)).expect("open");

    // 16 MiB fits a fresh 17 MiB segment; the next 2 MiB would
    // straddle the cap, so it must trigger the rotation.
    let first = wal
        .append(FrameKind::OtlpBatch, &vec![0xAA; 16 * 1024 * 1024])
        .expect("first append");
    assert_eq!(segment_files(tmp.path()).len(), 1, "no rotation yet");

    let second = wal
        .append(FrameKind::OtlpBatch, &vec![0xBB; 2 * 1024 * 1024])
        .expect("second append rotates first");
    wal.sync().expect("sync");

    assert_ne!(
        first.segment, second.segment,
        "the second frame lands in a fresh segment",
    );
    assert!(second > first, "UUIDv7 keeps offsets globally ordered");
    let files = segment_files(tmp.path());
    assert_eq!(files.len(), 2, "rotation opened exactly one new segment");
    assert_eq!(
        std::fs::metadata(&files[0]).expect("stat old").len(),
        first.byte,
        "the closed segment ends exactly at the first frame's offset — \
         nothing of the second frame leaked into it",
    );
    drop(wal);

    let frames = replayed(tmp.path());
    assert_eq!(
        frames.iter().map(|(o, _)| *o).collect::<Vec<_>>(),
        vec![first, second],
        "replay sees both frames exactly once, in order — no drop, no dup",
    );
    assert_eq!(frames[0].1.len(), 16 * 1024 * 1024);
    assert_eq!(frames[1].1.len(), 2 * 1024 * 1024);
}

/// Time-cap arm: once the segment's age (its `UUIDv7` mint
/// time) exceeds `wal_segment_age_secs`, the next append
/// rotates. An *empty* segment never age-rotates — there is no
/// recovery window to bound.
#[test]
fn rfc0008_6_time_cap_rotates_without_drop_or_duplicate() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let mut wal = Wal::open(config(tmp.path(), MIN_SEGMENT_SIZE_BYTES, 1)).expect("open");

    // The empty segment crosses the age cap first: no rotation —
    // the first frame still lands in it.
    std::thread::sleep(Duration::from_millis(1_200));
    let first = wal.append(FrameKind::OtlpBatch, &[0xAA]).expect("first");
    assert_eq!(
        segment_files(tmp.path()).len(),
        1,
        "an over-age but empty segment is not rotated",
    );

    // Now it holds a frame and is over the cap: the next append
    // rotates.
    std::thread::sleep(Duration::from_millis(1_200));
    let second = wal.append(FrameKind::OtlpBatch, &[0xBB]).expect("second");
    wal.sync().expect("sync");

    assert_ne!(first.segment, second.segment, "age cap rotated");
    assert_eq!(segment_files(tmp.path()).len(), 2);
    drop(wal);

    let frames = replayed(tmp.path());
    assert_eq!(
        frames,
        vec![(first, vec![0xAA]), (second, vec![0xBB])],
        "no drop, no dup across the time-cap rotation",
    );
}

/// Quiesce arm: a rotation step failing surfaces as a hard
/// `AppendError`, and every subsequent append is refused with
/// `QuiescedAfterRotationFailure` until an operator
/// intervenes — even after the underlying condition clears.
/// `sync` stays available: the old segment is still the append
/// target and acking frames already written to it is safe.
#[cfg(unix)]
#[test]
fn rfc0008_6_rotation_failure_quiesces_the_wal() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::TempDir::new().expect("temp");
    let mut wal = Wal::open(config(tmp.path(), MIN_SEGMENT_SIZE_BYTES, 600)).expect("open");
    wal.append(FrameKind::OtlpBatch, &vec![0xAA; 16 * 1024 * 1024])
        .expect("first append");

    // A read-only WAL root makes the rotation's fresh-segment
    // creation fail mid-sequence.
    let writable = std::fs::metadata(tmp.path())
        .expect("stat root")
        .permissions();
    let mut read_only = writable.clone();
    read_only.set_mode(0o555);
    std::fs::set_permissions(tmp.path(), read_only).expect("chmod ro");

    let err = wal
        .append(FrameKind::OtlpBatch, &vec![0xBB; 2 * 1024 * 1024])
        .expect_err("rotation must fail on a read-only root");
    assert!(
        matches!(err, AppendError::Io { .. }),
        "the triggering append surfaces the underlying IO error, got {err:?}",
    );

    // Restore the root: the condition is gone, but the WAL stays
    // quiesced — only operator intervention (a fresh open) clears it.
    std::fs::set_permissions(tmp.path(), writable).expect("chmod rw");
    let err = wal
        .append(FrameKind::OtlpBatch, &[0xCC])
        .expect_err("quiesced WAL refuses appends");
    assert!(
        matches!(err, AppendError::QuiescedAfterRotationFailure),
        "subsequent appends get the quiesce variant, got {err:?}",
    );
    assert!(
        wal.sync().is_ok(),
        "sync stays available — acking frames already in the old segment is safe",
    );
    assert_eq!(
        segment_files(tmp.path()).len(),
        1,
        "the failed rotation left no half-created segment",
    );
}
