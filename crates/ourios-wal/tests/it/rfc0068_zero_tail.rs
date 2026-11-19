//! RFC 0068 — WAL recovery from a zero-filled tail `[H3]`.
//! See `docs/rfcs/0068-wal-zero-tail-recovery.md` §5.
//!
//! Every arm forges the newest segment's tail after `B`, the end of its
//! last valid frame, and checks the §3.1 partition: a partial frame (T1)
//! or an all-zero run from `B` to EOF (T2) is healed back to `B`;
//! anything else is RFC0008.5 corruption with the segment untouched.
//! RFC0068.4, the crash test, lives beside RFC0008.2's; RFC0068.5, the
//! heal's event and counter, in `ourios-ingester`'s `tail_heal` module.

use std::io::Write;
use std::path::{Path, PathBuf};

use ourios_wal::{
    FrameKind, FrameSink, MAX_FRAME_BYTES, RecoveryError, RotationKind, Wal, WalConfig, WalOffset,
};
use proptest::prelude::*;

const FRAME_HEADER_LEN: usize = 12;
const SECTOR: usize = 512;
const PAYLOADS: [&[u8]; 2] = [b"acknowledged-one", b"acknowledged-two"];

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

/// A WAL root whose only segment holds `acknowledged` as synced frames
/// ending at `boundary`, `B`, the offset every arm forges its tail after.
struct Newest {
    root: tempfile::TempDir,
    segment: PathBuf,
    boundary: u64,
    acknowledged: Vec<Vec<u8>>,
}

impl Newest {
    fn synced(payloads: &[&[u8]]) -> Self {
        let root = tempfile::TempDir::new().expect("temp");
        let mut wal = open(root.path());
        for p in payloads {
            wal.append(FrameKind::OtlpBatch, p).expect("append");
        }
        wal.sync().expect("sync");
        drop(wal);
        let segment = segment_files(root.path()).pop().expect("one segment");
        let boundary = std::fs::metadata(&segment).expect("stat").len();
        Self {
            root,
            segment,
            boundary,
            acknowledged: payloads.iter().map(|p| p.to_vec()).collect(),
        }
    }
}

fn forge(newest: &Newest, tail: &[u8]) {
    std::fs::OpenOptions::new()
        .append(true)
        .open(&newest.segment)
        .expect("open segment")
        .write_all(tail)
        .expect("forge the tail");
}

fn segment_bytes(newest: &Newest) -> Vec<u8> {
    std::fs::read(&newest.segment).expect("read segment")
}

fn replay(newest: &Newest) -> (Result<(), RecoveryError>, Vec<Vec<u8>>) {
    let (_, result, frames) = recover(newest);
    (result, frames)
}

/// Open the root and replay it, keeping the recovering writer.
fn recover(newest: &Newest) -> (Wal, Result<(), RecoveryError>, Vec<Vec<u8>>) {
    let mut sink = CollectingSink::default();
    let mut wal = open(newest.root.path());
    let result = wal.replay(&mut sink);
    (wal, result, sink.frames)
}

/// The tail is healed: replay opens with every acknowledged frame, and
/// the segment is cut back to `B`. Returns the writer that recovered it.
fn assert_healed(newest: &Newest) -> Wal {
    let (wal, result, frames) = recover(newest);
    result.expect("a healable tail must not stop recovery");
    assert_eq!(
        frames, newest.acknowledged,
        "every frame before B is replayed"
    );
    assert_eq!(
        segment_bytes(newest).len() as u64,
        newest.boundary,
        "the segment is truncated to B",
    );
    wal
}

/// The tail is corruption: replay stops at `B` on this segment and
/// leaves its bytes as they were.
fn assert_corrupt_untouched(newest: &Newest) {
    let before = segment_bytes(newest);
    let (result, _) = replay(newest);
    match result {
        Err(RecoveryError::Corrupt { byte, .. }) => {
            assert_eq!(byte, newest.boundary, "corruption names the tail's offset");
        }
        other => panic!(
            "expected RecoveryError::Corrupt at {}, got {other:?}",
            newest.boundary
        ),
    }
    assert_eq!(
        segment_bytes(newest),
        before,
        "a corrupt segment is never modified"
    );
}

/// A forged frame (RFC 0008 §6.2.2): `len`, `kind` and `crc` are written
/// as given, so a test can make any of them disagree with `payload`.
#[derive(Clone, Default)]
struct Frame {
    len: u32,
    kind: u8,
    crc: u32,
    payload: Vec<u8>,
}

impl Frame {
    /// The frame with `len` and `crc` matching its kind and payload.
    fn sealed(mut self) -> Self {
        self.len = u32::try_from(self.payload.len()).expect("small payload");
        let prefix = crc32c::crc32c(&[self.kind, 0, 0, 0]);
        self.crc = crc32c::crc32c_append(prefix, &self.payload);
        self
    }

    fn bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(FRAME_HEADER_LEN + self.payload.len());
        bytes.extend_from_slice(&self.len.to_le_bytes());
        bytes.extend_from_slice(&[self.kind, 0, 0, 0]);
        bytes.extend_from_slice(&self.crc.to_le_bytes());
        bytes.extend_from_slice(&self.payload);
        bytes
    }
}

#[test]
fn rfc0068_1_zero_tail_at_a_frame_boundary_is_healed() {
    for zeros in [12, 13, 511, 512, 4096, 4096 + 7] {
        let newest = Newest::synced(&PAYLOADS);
        forge(&newest, &vec![0; zeros]);

        let mut wal = assert_healed(&newest);
        let next = wal
            .append(FrameKind::OtlpBatch, b"after-the-heal")
            .expect("append after the heal");
        wal.sync().expect("sync");
        drop(wal);
        assert_eq!(
            next.byte,
            newest.boundary + (FRAME_HEADER_LEN + b"after-the-heal".len()) as u64,
            "the next append lands at B ({zeros} zero bytes)",
        );
        let (result, frames) = replay(&newest);
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
        let newest = Newest::synced(&PAYLOADS);
        forge(&newest, &vec![byte; len]);
        assert_healed(&newest);
    }
}

#[test]
fn rfc0068_2_a_header_whose_payload_runs_past_eof_is_healed() {
    let newest = Newest::synced(&PAYLOADS);
    let cut_short = Frame {
        len: 64,
        kind: 0x01,
        payload: b"only-part-of-it".to_vec(),
        ..Frame::default()
    };
    forge(&newest, &cut_short.bytes());
    assert_healed(&newest);
}

#[test]
fn rfc0068_2_a_crc_mismatch_whose_payload_ends_in_zeros_is_corruption() {
    let newest = Newest::synced(&PAYLOADS);
    let mut frame = Frame {
        kind: 0x01,
        payload: b"written-then-zeroed".to_vec(),
        ..Frame::default()
    }
    .sealed();
    frame.payload.extend_from_slice(&[0; 32]);
    frame.len = u32::try_from(frame.payload.len()).expect("small payload");
    forge(&newest, &frame.bytes());
    assert_corrupt_untouched(&newest);
}

/// §4.2: a frame whose header reached disk and whose payload is zero
/// from a sector boundary to EOF stays corruption.
#[test]
fn rfc0068_2_a_payload_zeroed_from_a_sector_boundary_is_corruption() {
    let newest = Newest::synced(&PAYLOADS);
    let payload_start = usize::try_from(newest.boundary).expect("small segment") + FRAME_HEADER_LEN;
    let sector_end = (payload_start / SECTOR + 1) * SECTOR;
    let mut frame = Frame {
        kind: 0x01,
        payload: (0..2 * SECTOR)
            .map(|i| u8::try_from(i % 251 + 1).expect("below 256"))
            .collect(),
        ..Frame::default()
    }
    .sealed();
    frame.payload[sector_end - payload_start..].fill(0);
    forge(&newest, &frame.bytes());
    assert_corrupt_untouched(&newest);
}

#[test]
fn rfc0068_2_zeros_followed_by_a_non_zero_byte_are_corruption() {
    for zeros in [12, 511, 4096] {
        let newest = Newest::synced(&PAYLOADS);
        let mut tail = vec![0; zeros];
        tail.push(0x01);
        forge(&newest, &tail);
        assert_corrupt_untouched(&newest);
    }
}

#[test]
fn rfc0068_2_zeros_followed_by_a_valid_frame_are_corruption() {
    let newest = Newest::synced(&PAYLOADS);
    forge(&newest, &[0; SECTOR]);
    let frame = Frame {
        kind: 0x01,
        payload: b"a-frame-after-the-zeros".to_vec(),
        ..Frame::default()
    }
    .sealed();
    forge(&newest, &frame.bytes());
    assert_corrupt_untouched(&newest);
}

#[test]
fn rfc0068_3_a_zero_tail_on_a_closed_segment_is_corruption() {
    let older = Newest::synced(&PAYLOADS);
    forge(&older, &[0; SECTOR]);
    let mut wal = open(older.root.path());
    wal.rotate(RotationKind::Owed)
        .expect("rotate to a newer segment");
    wal.append(FrameKind::OtlpBatch, b"on-the-newest")
        .expect("append");
    wal.sync().expect("sync");
    drop(wal);
    assert_eq!(
        segment_files(older.root.path()).len(),
        2,
        "the zero tail is closed"
    );
    assert_corrupt_untouched(&older);
}

/// What a power cut leaves in the sector the last acknowledged frame
/// shares with the unsynced append after it (RFC 0068 §3.4).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SharedSector {
    /// Zeros after `X`, as before the append.
    Old,
    /// The append's bytes up to the sector's end.
    New,
}

/// RFC0068.6: the last acknowledged frame ends at `X` inside a sector,
/// and an unsynced append starts at `X` and runs past it. Every byte of
/// the append past the shared sector is zero.
fn shared_sector_case(sector: SharedSector) {
    for first in [1, 100, 300, 470] {
        let newest = Newest::synced(&[&vec![0x5A; first]]);
        let x = usize::try_from(newest.boundary).expect("small segment");
        assert_ne!(x % SECTOR, 0, "X sits inside a sector");

        let mut wal = open(newest.root.path());
        wal.append(FrameKind::OtlpBatch, &[0xC3; 3 * SECTOR])
            .expect("the append that never synced");
        wal.sync().expect("sync, only to read its bytes");
        drop(wal);
        let mut before = segment_bytes(&newest);
        let zero_from = match sector {
            SharedSector::Old => x,
            SharedSector::New => (x / SECTOR + 1) * SECTOR,
        };
        before[zero_from..].fill(0);
        std::fs::write(&newest.segment, &before).expect("forge the power cut");

        let (result, frames) = replay(&newest);
        let after = segment_bytes(&newest);
        match result {
            Ok(()) => {
                assert_eq!(
                    frames, newest.acknowledged,
                    "the acknowledged frame is replayed"
                );
                assert!(after.len() >= x, "recovery never truncates below X");
            }
            Err(RecoveryError::Corrupt { .. }) => {
                assert_eq!(after, before, "a corrupt segment is never modified");
            }
            Err(other) => panic!("expected a heal or corruption, got {other:?}"),
        }
        if sector == SharedSector::Old {
            assert_eq!(after.len(), x, "the old-contents arm is healed as T2 at X");
        }
    }
}

#[test]
fn rfc0068_6_a_shared_sector_left_old_is_healed_at_x() {
    shared_sector_case(SharedSector::Old);
}

#[test]
fn rfc0068_6_a_shared_sector_left_new_never_cuts_below_x() {
    shared_sector_case(SharedSector::New);
}

/// The first frame of a forged tail, read by the §3.1 rules written out
/// independently of the decoder.
enum Step {
    /// A complete, valid frame of this many bytes.
    Valid(usize),
    /// T1 or T2, or nothing left: healed here.
    Healed,
    Corrupt,
}

impl Newest {
    /// Admit a valid frame forged after `B`: replay delivers it, so `B`
    /// moves past it.
    fn admit(&mut self, frame: &[u8]) {
        self.acknowledged.push(frame[FRAME_HEADER_LEN..].to_vec());
        self.boundary += frame.len() as u64;
    }
}

fn first_step(rest: &[u8]) -> Step {
    if rest.len() < FRAME_HEADER_LEN || rest.iter().all(|&b| b == 0) {
        return Step::Healed;
    }
    let len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
    let well_formed = len <= MAX_FRAME_BYTES && (1..=4).contains(&rest[4]) && rest[5..8] == [0; 3];
    if !well_formed {
        return Step::Corrupt;
    }
    let Some(payload) = rest.get(FRAME_HEADER_LEN..FRAME_HEADER_LEN + len) else {
        return Step::Healed;
    };
    let sealed = Frame {
        kind: rest[4],
        payload: payload.to_vec(),
        ..Frame::default()
    }
    .sealed();
    if sealed.bytes()[8..12] == rest[8..12] {
        Step::Valid(FRAME_HEADER_LEN + len)
    } else {
        Step::Corrupt
    }
}

fn damaged_tails() -> impl Strategy<Value = Vec<u8>> {
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
            Frame {
                len,
                kind,
                crc: 0,
                payload: vec![0xEE; have],
            }
            .bytes()
        }),
    ]
}

/// Up to three valid frames, then a damaged tail.
fn tails() -> impl Strategy<Value = Vec<u8>> {
    let payloads = prop::collection::vec(prop::collection::vec(any::<u8>(), 0..40), 0..3);
    (payloads, damaged_tails()).prop_map(|(payloads, damaged)| {
        let mut tail: Vec<u8> = payloads
            .into_iter()
            .flat_map(|payload| {
                Frame {
                    kind: 0x01,
                    payload,
                    ..Frame::default()
                }
                .sealed()
                .bytes()
            })
            .collect();
        tail.extend(damaged);
        tail
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn rfc0068_healed_exactly_when_t1_or_t2_holds(tail in tails()) {
        let mut newest = Newest::synced(&PAYLOADS);
        forge(&newest, &tail);
        let mut rest = tail.as_slice();
        loop {
            match first_step(rest) {
                Step::Valid(len) => {
                    newest.admit(&rest[..len]);
                    rest = &rest[len..];
                }
                Step::Healed => {
                    assert_healed(&newest);
                    break;
                }
                Step::Corrupt => {
                    assert_corrupt_untouched(&newest);
                    break;
                }
            }
        }
    }
}
