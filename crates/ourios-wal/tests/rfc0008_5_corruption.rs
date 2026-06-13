//! RFC0008.5 — Corrupt frame `[H3]`.
//! See `docs/rfcs/0008-wal.md` §5 (five arms, one per sub-case).
//!
//! Each arm corrupts a frame on a **closed** (non-newest) segment
//! so the corruption is unambiguous (the newest segment's torn
//! tail is RFC0008.4 clean truncation), reopens, and replays. The
//! assertions per arm: `RecoveryError::Corrupt` names the segment
//! `UUIDv7` + byte offset + the specific `CorruptionReason`;
//! recovery stops scanning *all* segments — a `CollectingSink`
//! receives nothing past the corrupt point (and nothing from
//! later segments); and `wal_corrupt_frames_total` advances by
//! exactly 1.
//!
//! NO audit-event assertion: the `WalRecoveryCorruption` audit
//! event is **deferred** per the 2026-06-13 §5 amendment (WAL
//! corruption is a system event with no tenant; the durable
//! forensic record needs a system-scoped-audit design that does
//! not exist yet). RFC0008.5 is satisfied by the structured-error
//! and halt-all-segments halves plus the `wal_corrupt_frames_total`
//! counter, all asserted here.
//!
//! Read order in `frame::read_frame` (mirrored from
//! `src/frame.rs`): `len` (→ `OversizeLen`) is checked first, then
//! `kind` (→ `UnknownKind`), then `_pad` (→ `NonZeroPad`), and only
//! *after* the payload read is the CRC checked (→ `CrcMismatch`).
//! So the `len`/`kind`/`_pad` arms are caught by their own field
//! check, which fires **before** the CRC is ever consulted — the
//! CRC value is irrelevant to those three. The `CrcMismatch` arm
//! is the only one the CRC catches, and it flips a *payload* byte
//! (every header field stays valid). The forged headers mirror
//! `frame.rs`'s own `read_frame_rejects_*` unit tests.

use std::io::Write;
use std::path::{Path, PathBuf};

use ourios_wal::{
    CorruptionReason, FrameKind, FrameSink, MAX_FRAME_BYTES, RecoveryError, Wal, WalConfig,
    WalOffset,
};

const SEGMENT_HEADER_LEN: usize = 24;
const FRAME_HEADER_LEN: usize = 12;
const FRAME_PAD_ZEROS: [u8; 3] = [0, 0, 0];

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

fn segment_files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(root)
        .expect("read_dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "wal"))
        .collect();
    out.sort();
    out
}

/// Mint a closed segment holding one `OtlpBatch` frame per
/// payload in a scratch root, then move it into `dest_root`.
/// Returns its path under `dest_root`.
fn build_closed_segment(dest_root: &Path, payloads: &[&[u8]]) -> PathBuf {
    let scratch = tempfile::TempDir::new().expect("scratch root");
    let mut wal = open(scratch.path());
    for p in payloads {
        wal.append(FrameKind::OtlpBatch, p).expect("append");
    }
    wal.sync().expect("sync");
    drop(wal);
    let seg = segment_files(scratch.path())
        .into_iter()
        .next()
        .expect("scratch holds one segment");
    let dest = dest_root.join(seg.file_name().expect("segment file name"));
    std::fs::rename(&seg, &dest).expect("move segment into dest root");
    dest
}

fn segment_uuid(path: &Path) -> uuid::Uuid {
    path.file_stem()
        .and_then(|s| s.to_str())
        .expect("stem")
        .parse()
        .expect("segment filename is its UUID")
}

/// CRC32-C over `kind || _pad || payload` per §6.2.2 — mirrors
/// `frame::checksum_input`. Lets the kind/pad/len arms forge a
/// header whose CRC is *valid* for the tampered bytes, so the
/// intended field check fires instead of `CrcMismatch`.
fn checksum_input(kind: u8, pad: [u8; 3], payload: &[u8]) -> u32 {
    let mut prefix = [0u8; 4];
    prefix[0] = kind;
    prefix[1..4].copy_from_slice(&pad);
    let h = crc32c::crc32c(&prefix);
    crc32c::crc32c_append(h, payload)
}

/// Build the exact 12 B on-disk frame header (RFC 0008 §6.2.2):
/// `len:u32_le | kind:u8 | _pad:3B | crc32:u32_le`.
fn frame_header(len: u32, kind: u8, pad: [u8; 3], crc: u32) -> [u8; FRAME_HEADER_LEN] {
    let mut h = [0u8; FRAME_HEADER_LEN];
    h[0..4].copy_from_slice(&len.to_le_bytes());
    h[4] = kind;
    h[5..8].copy_from_slice(&pad);
    h[8..12].copy_from_slice(&crc.to_le_bytes());
    h
}

/// A closed segment carrying one forged frame (the given header +
/// payload bytes, written verbatim), preceded by the 24 B segment
/// header minted by the real API. Built by minting a real
/// single-frame segment, then overwriting its frame region with
/// the forged bytes (segment header stays valid). Returns the
/// path; the forged frame begins at `SEGMENT_HEADER_LEN`.
fn build_segment_with_forged_frame(
    dest_root: &Path,
    header: &[u8; FRAME_HEADER_LEN],
    payload: &[u8],
) -> PathBuf {
    // Mint a real segment with a placeholder frame large enough
    // that truncating to (segment header + forged frame) only
    // shrinks it — we then overwrite from SEGMENT_HEADER_LEN on.
    let path = build_closed_segment(dest_root, &[b"placeholder-frame-bytes"]);
    let mut bytes = std::fs::read(&path).expect("read seg");
    bytes.truncate(SEGMENT_HEADER_LEN);
    bytes.extend_from_slice(header);
    bytes.extend_from_slice(payload);
    std::fs::write(&path, &bytes).expect("write forged seg");
    path
}

#[derive(Default)]
struct CollectingSink {
    frames: Vec<(FrameKind, Vec<u8>)>,
}

impl FrameSink for CollectingSink {
    fn consume(
        &mut self,
        _offset: WalOffset,
        kind: FrameKind,
        payload: &[u8],
    ) -> Result<(), RecoveryError> {
        self.frames.push((kind, payload.to_vec()));
        Ok(())
    }
}

/// Assert the common RFC0008.5 contract: replay halts with
/// `Corrupt` naming `expected_uuid` + `expected_byte` +
/// `expected_reason`, the sink got nothing (the corrupt frame is
/// the first on its segment and recovery stops scanning all
/// segments, so even a later segment's frames are never
/// delivered), and `wal_corrupt_frames_total` advanced by 1.
fn assert_corrupt(
    root: &Path,
    expected_uuid: uuid::Uuid,
    expected_byte: u64,
    expected_reason: CorruptionReason,
) {
    let mut wal = open(root);
    let before = wal.metrics().corrupt_frames_total;
    let mut sink = CollectingSink::default();
    let result = wal.replay(&mut sink);
    match result {
        Err(RecoveryError::Corrupt {
            segment,
            byte,
            reason,
        }) => {
            assert_eq!(reason, expected_reason, "corruption reason");
            assert_eq!(
                segment, expected_uuid,
                "Corrupt names the offending segment"
            );
            assert_eq!(byte, expected_byte, "Corrupt names the byte offset");
        }
        other => panic!("expected RecoveryError::Corrupt({expected_reason:?}), got {other:?}"),
    }
    assert!(
        sink.frames.is_empty(),
        "recovery stops scanning all segments — no frames past the corrupt point",
    );
    assert_eq!(
        wal.metrics().corrupt_frames_total,
        before + 1,
        "wal_corrupt_frames_total advances by exactly 1",
    );
}

/// Arm 1 — CRC mismatch: flip a payload bit on a closed segment's
/// frame. Only the CRC check catches a payload flip.
#[test]
fn rfc0008_5_payload_bit_flip_is_crc_mismatch_corruption() {
    let tmp = tempfile::TempDir::new().expect("temp");
    // Older segment to corrupt + a newer one whose frames must
    // never be delivered (proves the halt-all-segments contract).
    let older = build_closed_segment(tmp.path(), &[b"corrupt-me", b"and-me"]);
    let _newer = build_closed_segment(tmp.path(), &[b"later-frame"]);
    let uuid = segment_uuid(&older);

    let mut bytes = std::fs::read(&older).expect("read");
    // First frame's first payload byte (past segment + frame
    // headers).
    bytes[SEGMENT_HEADER_LEN + FRAME_HEADER_LEN] ^= 0xFF;
    std::fs::write(&older, &bytes).expect("write");

    assert_corrupt(
        tmp.path(),
        uuid,
        SEGMENT_HEADER_LEN as u64,
        CorruptionReason::CrcMismatch,
    );
}

/// Arm 2 — unknown kind: forge a frame header with `kind = 0x03`
/// (reserved range) and a CRC recomputed over the tampered bytes,
/// so `UnknownKind` fires rather than `CrcMismatch`.
#[test]
fn rfc0008_5_unknown_kind_is_corruption() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let payload = b"unknown-kind-payload";
    let kind = 0x03u8;
    let crc = checksum_input(kind, FRAME_PAD_ZEROS, payload);
    let header = frame_header(
        u32::try_from(payload.len()).expect("fits u32"),
        kind,
        FRAME_PAD_ZEROS,
        crc,
    );
    let seg = build_segment_with_forged_frame(tmp.path(), &header, payload);
    let _newer = build_closed_segment(tmp.path(), &[b"later-frame"]);
    let uuid = segment_uuid(&seg);

    assert_corrupt(
        tmp.path(),
        uuid,
        SEGMENT_HEADER_LEN as u64,
        CorruptionReason::UnknownKind,
    );
}

/// Arm 3 — non-zero `_pad`: forge a header with a non-zero pad
/// byte and a CRC recomputed over `kind || tampered-pad ||
/// payload`, so `NonZeroPad` fires (the read checks `kind` first,
/// so the kind stays valid; pad is checked before the CRC).
#[test]
fn rfc0008_5_non_zero_pad_is_corruption() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let payload = b"non-zero-pad-payload";
    let kind = FrameKind::OtlpBatch as u8;
    let pad = [0u8, 0xAA, 0u8];
    let crc = checksum_input(kind, pad, payload);
    let header = frame_header(
        u32::try_from(payload.len()).expect("fits u32"),
        kind,
        pad,
        crc,
    );
    let seg = build_segment_with_forged_frame(tmp.path(), &header, payload);
    let _newer = build_closed_segment(tmp.path(), &[b"later-frame"]);
    let uuid = segment_uuid(&seg);

    assert_corrupt(
        tmp.path(),
        uuid,
        SEGMENT_HEADER_LEN as u64,
        CorruptionReason::NonZeroPad,
    );
}

/// Arm 4 — oversize `len`: forge a header declaring
/// `len > MAX_FRAME_BYTES`. The read rejects on `len` alone
/// before attempting the (absent) payload read, so no payload is
/// written and the CRC is irrelevant.
#[test]
fn rfc0008_5_oversize_len_is_corruption() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let oversize = u32::try_from(MAX_FRAME_BYTES + 1).expect("fits u32");
    let header = frame_header(oversize, FrameKind::OtlpBatch as u8, FRAME_PAD_ZEROS, 0);
    // No payload — the `len` check must fire before any payload
    // read, otherwise the test would block trying to read 16 MiB.
    let seg = build_segment_with_forged_frame(tmp.path(), &header, b"");
    let _newer = build_closed_segment(tmp.path(), &[b"later-frame"]);
    let uuid = segment_uuid(&seg);

    assert_corrupt(
        tmp.path(),
        uuid,
        SEGMENT_HEADER_LEN as u64,
        CorruptionReason::OversizeLen,
    );
}

/// Arm 5 — torn tail on a closed segment: truncate a closed
/// segment mid-frame. A short read on a non-newest segment is
/// `TornOnClosedSegment` corruption (mirrors RFC0008.4 (c) — same
/// assertion, distinct acceptance criterion).
#[test]
fn rfc0008_5_torn_tail_on_closed_segment_is_corruption() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let older = build_closed_segment(tmp.path(), &[b"good-frame", b"torn-frame"]);
    let _newer = build_closed_segment(tmp.path(), &[b"later-frame"]);
    let uuid = segment_uuid(&older);

    // Boundary after the first (good) frame; lop off a few bytes
    // past it so the second frame is a torn tail.
    let first_boundary = (SEGMENT_HEADER_LEN + FRAME_HEADER_LEN + b"good-frame".len()) as u64;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(&older)
        .expect("open older");
    f.set_len(first_boundary + 4).expect("truncate mid-frame");
    f.flush().expect("flush");

    // The first frame IS valid and would be delivered before the
    // scan hits the torn second frame, so the sink is not empty
    // here — assert the specifics inline rather than via the
    // empty-sink helper.
    let mut wal = open(tmp.path());
    let before = wal.metrics().corrupt_frames_total;
    let mut sink = CollectingSink::default();
    let result = wal.replay(&mut sink);
    match result {
        Err(RecoveryError::Corrupt {
            segment,
            byte,
            reason,
        }) => {
            assert_eq!(reason, CorruptionReason::TornOnClosedSegment);
            assert_eq!(segment, uuid, "names the offending older segment");
            assert_eq!(byte, first_boundary, "byte is the start of the torn frame");
        }
        other => panic!("expected Corrupt(TornOnClosedSegment), got {other:?}"),
    }
    assert_eq!(
        sink.frames,
        vec![(FrameKind::OtlpBatch, b"good-frame".to_vec())],
        "the valid frame before the torn point is delivered; nothing past it (no later-segment frame)",
    );
    assert_eq!(
        wal.metrics().corrupt_frames_total,
        before + 1,
        "wal_corrupt_frames_total advances by exactly 1",
    );
}
