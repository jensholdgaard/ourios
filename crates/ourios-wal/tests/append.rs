//! `Wal::append` integration tests landing with PR-M5.
//!
//! Covers the on-disk byte layout that the §5 acceptance
//! criteria don't yet exercise live (RFC0008.1 needs `sync`,
//! RFC0008.5 needs `replay`). These tests pin that the
//! `append` write path produces files the §6.2.2 reader
//! accepts — the round-trip that the recovery walk will rely
//! on once it lands.

use std::io::Read;
use std::path::Path;

use ourios_wal::{AppendError, FrameKind, MAX_FRAME_BYTES, Wal, WalConfig};

fn default_config(root: &Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    }
}

fn open(root: &Path) -> Wal {
    Wal::open(default_config(root)).expect("open")
}

/// A single happy-path append writes the §6.2.2 frame bytes
/// after the §6.2.1 header. The returned [`WalOffset`] points
/// at the start of the new frame (byte 24, immediately after
/// the segment header).
#[test]
fn one_append_writes_one_frame_after_the_segment_header() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let mut wal = open(tmp.path());
    let payload = b"hello world";
    let offset = wal
        .append(FrameKind::OtlpBatch, payload)
        .expect("append must succeed");

    // Offset is at byte 24: immediately after the 24 B segment
    // header, before any frame. The segment UUID matches the
    // single segment file's name.
    assert_eq!(offset.byte, 24, "frame starts immediately after the header");
    let segment_path = exactly_one_segment_path(tmp.path());
    assert_eq!(
        segment_path
            .file_stem()
            .expect("stem")
            .to_string_lossy()
            .parse::<uuid::Uuid>()
            .expect("stem parses"),
        offset.segment,
        "WalOffset.segment matches the on-disk filename",
    );

    // On-disk: 24 B segment header + 12 B frame header + 11 B
    // payload = 47 B exactly. Pinning the total size catches a
    // double-write regression where `append` accidentally also
    // re-emits the segment header.
    let bytes = read_all(&segment_path);
    assert_eq!(bytes.len(), 24 + 12 + payload.len());
    // Frame's `len` (LE u32) sits at byte 24.
    assert_eq!(
        &bytes[24..28],
        &u32::try_from(payload.len())
            .expect("len fits u32")
            .to_le_bytes(),
        "frame len = payload.len()",
    );
    // `kind` = 0x01 (OtlpBatch) at byte 28; `_pad` zeros at 29..32.
    assert_eq!(bytes[28], 0x01, "kind = OtlpBatch");
    assert_eq!(&bytes[29..32], &[0, 0, 0], "_pad = three zero bytes");
    // Payload starts at byte 36 (after the 4-byte CRC).
    assert_eq!(&bytes[36..], payload, "payload bytes match verbatim");
}

/// Two appends land back-to-back; the second `WalOffset.byte`
/// equals exactly the first frame's end (segment header + first
/// frame's 12 B + first payload). No accidental padding between
/// frames.
#[test]
fn consecutive_appends_pack_tight_with_monotonic_offsets() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let mut wal = open(tmp.path());
    let first_payload = b"first";
    let second_payload = b"second-payload";

    let first = wal.append(FrameKind::OtlpBatch, first_payload).expect("a1");
    let second = wal
        .append(FrameKind::AuditEvent, second_payload)
        .expect("a2");

    assert!(first < second, "offsets are monotonic");
    assert_eq!(
        second.byte,
        first.byte + 12 + u64::try_from(first_payload.len()).expect("len fits u64"),
        "second frame starts immediately after the first's header + payload",
    );
    assert_eq!(
        second.segment, first.segment,
        "both frames land in the same segment (no rotation yet)",
    );

    let bytes = read_all(&exactly_one_segment_path(tmp.path()));
    assert_eq!(
        bytes.len(),
        24 + (12 + first_payload.len()) + (12 + second_payload.len()),
        "no padding between frames",
    );
}

/// `MAX_FRAME_BYTES`-sized payload is accepted (boundary).
/// One byte more is rejected with `TooLarge`. The test uses
/// `0` bytes everywhere so the multi-MiB allocation is cheap.
#[test]
fn max_frame_bytes_is_accepted_one_more_is_rejected() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let mut wal = open(tmp.path());
    let max_payload = vec![0u8; MAX_FRAME_BYTES];
    wal.append(FrameKind::OtlpBatch, &max_payload)
        .expect("max-sized payload at the cap must be accepted");
    let oversize = vec![0u8; MAX_FRAME_BYTES + 1];
    match wal.append(FrameKind::OtlpBatch, &oversize) {
        Err(AppendError::TooLarge { len, limit }) => {
            assert_eq!(
                len,
                MAX_FRAME_BYTES + 1,
                "TooLarge reports the input length"
            );
            assert_eq!(limit, MAX_FRAME_BYTES, "TooLarge reports MAX_FRAME_BYTES");
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

/// Appends survive close + reopen: an `append` after `Wal::open`
/// on an existing root extends the same segment (no fresh
/// segment), and the offset is past the existing frames'
/// bytes.
#[test]
fn append_after_reopen_extends_the_existing_segment() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let first_offset = {
        let mut wal = open(tmp.path());
        wal.append(FrameKind::OtlpBatch, b"before-close")
            .expect("first append")
    };
    let segment_path = exactly_one_segment_path(tmp.path());
    let bytes_after_first = read_all(&segment_path).len();

    let second_offset = {
        let mut wal = open(tmp.path());
        wal.append(FrameKind::AuditEvent, b"after-reopen")
            .expect("second append after reopen")
    };

    // Reopen MUST NOT create a fresh segment.
    assert_eq!(
        exactly_one_segment_path(tmp.path()),
        segment_path,
        "reopen reused the existing segment (no fresh mint)",
    );
    assert_eq!(
        first_offset.segment, second_offset.segment,
        "second frame lands in the same segment",
    );
    assert_eq!(
        second_offset.byte,
        u64::try_from(bytes_after_first).expect("byte count fits u64"),
        "second frame starts at the post-first-append byte count",
    );
}

fn exactly_one_segment_path(root: &Path) -> std::path::PathBuf {
    let mut paths: Vec<_> = std::fs::read_dir(root)
        .expect("read_dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "wal"))
        .collect();
    paths.sort();
    assert_eq!(paths.len(), 1, "exactly one segment file");
    paths.into_iter().next().expect("one segment")
}

fn read_all(path: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .expect("open")
        .read_to_end(&mut bytes)
        .expect("read");
    bytes
}
