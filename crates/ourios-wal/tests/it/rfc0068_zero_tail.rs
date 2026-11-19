//! RFC 0068 — WAL recovery from a zero-filled tail `[H3]`.
//! See `docs/rfcs/0068-wal-zero-tail-recovery.md` §5.
//!
//! Every arm forges the newest segment's tail after `B`, the end of its
//! last valid frame, and checks the §3.1 partition: a partial frame (T1)
//! or an all-zero run from `B` to EOF (T2) is healed back to `B`;
//! anything else is RFC0008.5 corruption with the segment untouched.
//! RFC0068.4, the crash test, lives beside RFC0008.2's.

use std::io::Write;
use std::path::{Path, PathBuf};

use ourios_wal::{
    FrameKind, FrameSink, MAX_FRAME_BYTES, RecoveryError, RotationKind, Wal, WalConfig, WalOffset,
};
use proptest::prelude::*;

const FRAME_HEADER_LEN: usize = 12;
const SECTOR: usize = 512;

fn default_config(root: &Path) -> WalConfig {
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

#[derive(Default)]
struct CollectingSink {
    frames: Vec<Vec<u8>>,
}

impl FrameSink for CollectingSink {
    fn consume(
        &mut self,
        _offset: WalOffset,
        _kind: FrameKind,
        payload: &[u8],
    ) -> Result<(), RecoveryError> {
        self.frames.push(payload.to_vec());
        Ok(())
    }
}

/// A root whose only segment holds `payloads` as synced frames. Returns
/// the segment and `B`, its length.
fn synced_segment(root: &Path, payloads: &[&[u8]]) -> (PathBuf, u64) {
    let mut wal = open(root);
    for p in payloads {
        wal.append(FrameKind::OtlpBatch, p).expect("append");
    }
    wal.sync().expect("sync");
    drop(wal);
    let segment = segment_files(root).pop().expect("one segment");
    let end = std::fs::metadata(&segment).expect("stat").len();
    (segment, end)
}

fn append_bytes(segment: &Path, bytes: &[u8]) {
    std::fs::OpenOptions::new()
        .append(true)
        .open(segment)
        .expect("open segment")
        .write_all(bytes)
        .expect("forge the tail");
}

fn frame_header(len: u32, kind: u8, crc: u32) -> [u8; FRAME_HEADER_LEN] {
    let mut h = [0u8; FRAME_HEADER_LEN];
    h[0..4].copy_from_slice(&len.to_le_bytes());
    h[4] = kind;
    h[8..12].copy_from_slice(&crc.to_le_bytes());
    h
}

fn checksum(kind: u8, payload: &[u8]) -> u32 {
    crc32c::crc32c_append(crc32c::crc32c(&[kind, 0, 0, 0]), payload)
}

fn replay(root: &Path) -> (Result<(), RecoveryError>, Vec<Vec<u8>>) {
    let mut sink = CollectingSink::default();
    let result = open(root).replay(&mut sink);
    (result, sink.frames)
}

fn owned(payloads: &[&[u8]]) -> Vec<Vec<u8>> {
    payloads.iter().map(|p| p.to_vec()).collect()
}

/// The tail is healed: replay opens with every frame before `boundary`,
/// and the segment is cut back to `boundary`.
fn assert_healed(root: &Path, segment: &Path, boundary: u64, payloads: &[&[u8]]) {
    let (result, frames) = replay(root);
    result.expect("a healable tail must not stop recovery");
    assert_eq!(frames, owned(payloads), "every frame before B is replayed");
    assert_eq!(
        std::fs::metadata(segment).expect("stat").len(),
        boundary,
        "the segment is truncated to B",
    );
}

/// The tail is corruption: replay stops at `boundary` on this segment
/// and leaves its bytes as they were.
fn assert_corrupt_untouched(root: &Path, segment: &Path, boundary: u64) {
    let before = std::fs::read(segment).expect("read segment");
    let (result, _) = replay(root);
    match result {
        Err(RecoveryError::Corrupt { byte, .. }) => {
            assert_eq!(byte, boundary, "corruption names the tail's offset");
        }
        other => panic!("expected RecoveryError::Corrupt at {boundary}, got {other:?}"),
    }
    assert_eq!(
        std::fs::read(segment).expect("read segment"),
        before,
        "a corrupt segment is never modified",
    );
}

const PAYLOADS: [&[u8]; 2] = [b"acknowledged-one", b"acknowledged-two"];

#[test]
#[ignore = "RFC0068 red: replay reads a zero header as an unknown kind until §3.1 T2 lands"]
fn rfc0068_1_zero_tail_at_a_frame_boundary_is_healed() {
    for zeros in [12, 13, 511, 512, 4096, 4096 + 7] {
        let tmp = tempfile::TempDir::new().expect("temp");
        let (segment, boundary) = synced_segment(tmp.path(), &PAYLOADS);
        append_bytes(&segment, &vec![0; zeros]);

        assert_healed(tmp.path(), &segment, boundary, &PAYLOADS);

        let mut wal = open(tmp.path());
        let next = wal
            .append(FrameKind::OtlpBatch, b"after-the-heal")
            .expect("append after the heal");
        wal.sync().expect("sync");
        drop(wal);
        assert_eq!(
            next.byte,
            boundary + (FRAME_HEADER_LEN + b"after-the-heal".len()) as u64,
            "the next append lands at B ({zeros} zero bytes)",
        );
        let (result, frames) = replay(tmp.path());
        result.expect("replay after the heal");
        assert_eq!(
            frames.last().map(Vec::as_slice),
            Some(&b"after-the-heal"[..]),
            "the append after the heal survives a reopen",
        );
    }
}

#[test]
fn rfc0068_2_a_short_tail_of_any_bytes_is_healed() {
    for (len, byte) in [(1, 0x00), (11, 0x00), (5, 0xAB), (11, 0xFF)] {
        let tmp = tempfile::TempDir::new().expect("temp");
        let (segment, boundary) = synced_segment(tmp.path(), &PAYLOADS);
        append_bytes(&segment, &vec![byte; len]);
        assert_healed(tmp.path(), &segment, boundary, &PAYLOADS);
    }
}

#[test]
fn rfc0068_2_a_header_whose_payload_runs_past_eof_is_healed() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (segment, boundary) = synced_segment(tmp.path(), &PAYLOADS);
    append_bytes(&segment, &frame_header(64, 0x01, 0));
    append_bytes(&segment, b"only-part-of-it");
    assert_healed(tmp.path(), &segment, boundary, &PAYLOADS);
}

#[test]
fn rfc0068_2_a_crc_mismatch_whose_payload_ends_in_zeros_is_corruption() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (segment, boundary) = synced_segment(tmp.path(), &PAYLOADS);
    let mut payload = b"written-then-zeroed".to_vec();
    let crc = checksum(0x01, &payload);
    payload.extend_from_slice(&[0; 32]);
    let len = u32::try_from(payload.len()).expect("small payload");
    append_bytes(&segment, &frame_header(len, 0x01, crc));
    append_bytes(&segment, &payload);
    assert_corrupt_untouched(tmp.path(), &segment, boundary);
}

/// §4.2: a frame whose header reached disk and whose payload is zero
/// from a sector boundary to EOF stays corruption.
#[test]
fn rfc0068_2_a_payload_zeroed_from_a_sector_boundary_is_corruption() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (segment, boundary) = synced_segment(tmp.path(), &PAYLOADS);
    let start = usize::try_from(boundary).expect("small segment");
    let payload_start = start + FRAME_HEADER_LEN;
    let sector_end = (payload_start / SECTOR + 1) * SECTOR;
    let payload: Vec<u8> = (0..2 * SECTOR)
        .map(|i| u8::try_from(i % 251 + 1).expect("below 256"))
        .collect();
    let mut forged = payload.clone();
    forged[sector_end - payload_start..].fill(0);
    let len = u32::try_from(payload.len()).expect("small payload");
    append_bytes(&segment, &frame_header(len, 0x01, checksum(0x01, &payload)));
    append_bytes(&segment, &forged);
    assert_corrupt_untouched(tmp.path(), &segment, boundary);
}

#[test]
fn rfc0068_2_zeros_followed_by_a_non_zero_byte_are_corruption() {
    for zeros in [12, 511, 4096] {
        let tmp = tempfile::TempDir::new().expect("temp");
        let (segment, boundary) = synced_segment(tmp.path(), &PAYLOADS);
        let mut tail = vec![0; zeros];
        tail.push(0x01);
        append_bytes(&segment, &tail);
        assert_corrupt_untouched(tmp.path(), &segment, boundary);
    }
}

#[test]
fn rfc0068_2_zeros_followed_by_a_valid_frame_are_corruption() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (segment, boundary) = synced_segment(tmp.path(), &PAYLOADS);
    append_bytes(&segment, &[0; SECTOR]);
    let payload = b"a-frame-after-the-zeros";
    let len = u32::try_from(payload.len()).expect("small payload");
    append_bytes(&segment, &frame_header(len, 0x01, checksum(0x01, payload)));
    append_bytes(&segment, payload);
    assert_corrupt_untouched(tmp.path(), &segment, boundary);
}

#[test]
fn rfc0068_3_a_zero_tail_on_a_closed_segment_is_corruption() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (older, boundary) = synced_segment(tmp.path(), &PAYLOADS);
    append_bytes(&older, &[0; SECTOR]);
    let mut wal = open(tmp.path());
    wal.rotate(RotationKind::Owed)
        .expect("rotate to a newer segment");
    wal.append(FrameKind::OtlpBatch, b"on-the-newest")
        .expect("append");
    wal.sync().expect("sync");
    drop(wal);
    assert_eq!(
        segment_files(tmp.path()).len(),
        2,
        "the zero tail is closed"
    );
    assert_corrupt_untouched(tmp.path(), &older, boundary);
}

/// RFC0068.6: the last acknowledged frame ends at `X` inside a sector,
/// and an unsynced append starts at `X` and runs past it. A power cut
/// leaves the shared sector old (zeros after `X`) or new, and every
/// later byte of the append zero.
fn shared_sector_case(first: usize, keep_new_sector: bool) {
    let tmp = tempfile::TempDir::new().expect("temp");
    let acknowledged = vec![0x5A; first];
    let (segment, x) = synced_segment(tmp.path(), &[&acknowledged]);
    let x = usize::try_from(x).expect("small segment");
    assert_ne!(x % SECTOR, 0, "X sits inside a sector");

    let mut wal = open(tmp.path());
    wal.append(FrameKind::OtlpBatch, &[0xC3; 3 * SECTOR])
        .expect("the append that never synced");
    wal.sync().expect("sync, only to read its bytes");
    drop(wal);
    let mut bytes = std::fs::read(&segment).expect("read segment");
    let sector_end = (x / SECTOR + 1) * SECTOR;
    let zero_from = if keep_new_sector { sector_end } else { x };
    bytes[zero_from..].fill(0);
    std::fs::write(&segment, &bytes).expect("forge the power cut");

    let before = bytes;
    let (result, frames) = replay(tmp.path());
    let after = std::fs::read(&segment).expect("read segment");
    match result {
        Ok(()) => {
            assert_eq!(
                frames,
                vec![acknowledged],
                "the acknowledged frame is replayed"
            );
            assert!(after.len() >= x, "recovery never truncates below X");
        }
        Err(RecoveryError::Corrupt { .. }) => {
            assert_eq!(after, before, "a corrupt segment is never modified");
        }
        Err(other) => panic!("expected a heal or corruption, got {other:?}"),
    }
    if !keep_new_sector {
        assert_eq!(after.len(), x, "the old-contents arm is healed as T2 at X");
    }
}

#[test]
#[ignore = "RFC0068 red: replay reads a zero header as an unknown kind until §3.1 T2 lands"]
fn rfc0068_6_a_shared_sector_left_old_is_healed_at_x() {
    for first in [1, 100, 300, 470] {
        shared_sector_case(first, false);
    }
}

#[test]
fn rfc0068_6_a_shared_sector_left_new_never_cuts_below_x() {
    for first in [1, 100, 300, 470] {
        shared_sector_case(first, true);
    }
}

#[test]
#[ignore = "RFC0068 red: Wal::replay reports each healed tail and the server's recovery driver emits the event and counter"]
fn rfc0068_5_a_heal_is_reported() {
    todo!(
        "RFC0068.5: one WARN ourios.wal.recovery.tail.truncated per heal, and the counter grows by E - B"
    );
}

/// The §3.1 partition, written out independently of the decoder: healed
/// exactly when T1 or T2 holds. `None` is a tail whose first frame is
/// valid, which is no tail at all.
fn expected_heal(tail: &[u8]) -> Option<bool> {
    if tail.len() < FRAME_HEADER_LEN {
        return Some(true);
    }
    if tail.iter().all(|&b| b == 0) {
        return Some(true);
    }
    let len = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]) as usize;
    let well_formed = len <= MAX_FRAME_BYTES && (1..=4).contains(&tail[4]) && tail[5..8] == [0; 3];
    if !well_formed {
        return Some(false);
    }
    let Some(payload) = tail.get(FRAME_HEADER_LEN..FRAME_HEADER_LEN + len) else {
        return Some(true);
    };
    let crc = u32::from_le_bytes([tail[8], tail[9], tail[10], tail[11]]);
    (crc != checksum(tail[4], payload)).then_some(false)
}

fn tails() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 1..80),
        (12usize..1200).prop_map(|n| vec![0; n]),
        (12usize..600, 1u8..=255, 0usize..8).prop_map(|(n, b, extra)| {
            let mut t = vec![0; n];
            t.push(b);
            t.extend(std::iter::repeat_n(0, extra));
            t
        }),
        (1u32..64, 1u8..=4, 0usize..64).prop_map(|(len, kind, have)| {
            let mut t = frame_header(len, kind, 0).to_vec();
            t.extend(std::iter::repeat_n(0xEE, have));
            t
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    #[ignore = "RFC0068 red: replay reads a zero header as an unknown kind until §3.1 T2 lands"]
    fn rfc0068_healed_exactly_when_t1_or_t2_holds(tail in tails()) {
        let Some(healed) = expected_heal(&tail) else {
            return Ok(());
        };
        let tmp = tempfile::TempDir::new().expect("temp");
        let (segment, boundary) = synced_segment(tmp.path(), &PAYLOADS);
        append_bytes(&segment, &tail);
        if healed {
            assert_healed(tmp.path(), &segment, boundary, &PAYLOADS);
        } else {
            assert_corrupt_untouched(tmp.path(), &segment, boundary);
        }
    }
}
