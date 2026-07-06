//! `Wal::sync` + `Wal::replay` integration tests (RFC 0008
//! §6.3 / §6.6 — the durability + crash-recovery halves).
//!
//! These exercise recovery *in process*: append + `sync`, drop
//! the `Wal` (which only closes the file handle — there is no
//! flush-on-drop), then reopen and `replay`. That leaves the
//! segment in exactly the on-disk state a `SIGKILL` would: every
//! byte that reached a `write(2)` is in the kernel page cache and
//! survives a process death, so a reopen reads it back. The
//! real-`SIGKILL` subprocess harness that flips the §5
//! `rfc0008_2` acceptance test lands in a follow-up; this file
//! pins the `sync`/`replay` *mechanism* those tests stand on.

use std::io::Write;
use std::path::{Path, PathBuf};

use ourios_wal::{
    CorruptionReason, FrameKind, FrameSink, RecoveryError, Wal, WalConfig, WalOffset,
};

/// On-disk layout sizes (RFC 0008 §6.2.1 / §6.2.2), mirrored
/// here because the crate-internal constants are `pub(crate)`
/// and not visible to this integration test. A frame's payload
/// begins `SEGMENT_HEADER_LEN + FRAME_HEADER_LEN` bytes into the
/// first segment.
const SEGMENT_HEADER_LEN: usize = 24;
const FRAME_HEADER_LEN: usize = 12;

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

/// Recovery sink that records every frame it is handed, in
/// order, so a test can assert the recovered sequence.
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

fn the_one_segment(root: &Path) -> PathBuf {
    let mut paths: Vec<_> = std::fs::read_dir(root)
        .expect("read_dir")
        // Surface a per-entry error rather than swallowing it: a
        // permission / transient-IO failure should fail loudly,
        // not masquerade as "the wrong number of segments".
        .map(|e| e.expect("read_dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "wal"))
        .collect();
    paths.sort();
    assert_eq!(paths.len(), 1, "exactly one segment file");
    paths.into_iter().next().expect("one segment")
}

/// `sync` reports the segment length as the highest durable
/// offset — the same byte position the preceding `append`
/// returned (everything written so far is now durable).
#[test]
fn sync_reports_the_segment_length_as_the_durable_offset() {
    // Arrange
    let tmp = tempfile::TempDir::new().expect("temp");
    let mut wal = open(tmp.path());
    let appended = wal
        .append(FrameKind::OtlpBatch, b"durable-payload")
        .expect("append");

    // Act
    let durable = wal.sync().expect("sync must succeed");

    // Assert
    assert_eq!(
        durable, appended,
        "durable offset == the post-append offset of the last write",
    );
}

/// The happy path: synced frames of both kinds replay back in
/// append order, payloads byte-identical. Covers the RFC 0005
/// §3.7 audit-durability contract — `AuditEvent` frames survive
/// the round trip alongside `OtlpBatch` frames.
#[test]
fn synced_frames_of_both_kinds_replay_in_order() {
    // Arrange: append a mixed sequence, sync, then drop the WAL
    // (close == the on-disk state a crash leaves).
    let tmp = tempfile::TempDir::new().expect("temp");
    {
        let mut wal = open(tmp.path());
        wal.append(FrameKind::OtlpBatch, b"alpha").expect("a1");
        wal.append(FrameKind::AuditEvent, b"bravo").expect("a2");
        wal.append(FrameKind::OtlpBatch, b"charlie").expect("a3");
        wal.sync().expect("sync");
    }

    // Act
    let mut sink = CollectingSink::default();
    open(tmp.path()).replay(&mut sink).expect("replay");

    // Assert
    assert_eq!(
        sink.frames,
        vec![
            (FrameKind::OtlpBatch, b"alpha".to_vec()),
            (FrameKind::AuditEvent, b"bravo".to_vec()),
            (FrameKind::OtlpBatch, b"charlie".to_vec()),
        ],
        "every synced frame recovered, in order, payloads verbatim",
    );
}

/// A freshly-opened WAL with no appends replays cleanly to zero
/// frames — the segment is header-only, so the scan reaches EOF
/// on the first frame boundary (the `CleanTail` path).
#[test]
fn replay_on_a_fresh_wal_yields_no_frames() {
    // Arrange
    let tmp = tempfile::TempDir::new().expect("temp");
    let mut wal = open(tmp.path());

    // Act
    let mut sink = CollectingSink::default();
    wal.replay(&mut sink).expect("replay");

    // Assert
    assert!(sink.frames.is_empty(), "no frames on a header-only segment");
}

/// A torn (partial) tail frame on the newest segment is
/// RFC0008.4 clean truncation: `replay` recovers the intact
/// frames, heals the segment back to the last valid boundary,
/// and a subsequent `append` resumes there — a second replay
/// sees the healed sequence with no trace of the torn bytes.
#[test]
fn torn_tail_on_newest_is_healed_and_next_append_resumes_on_boundary() {
    // Arrange: two good frames, synced, then a short garbage tail
    // written straight to the file (fewer than 12 B → a partial
    // frame header, which the decoder hits as an unexpected EOF).
    let tmp = tempfile::TempDir::new().expect("temp");
    {
        let mut wal = open(tmp.path());
        wal.append(FrameKind::OtlpBatch, b"good-one").expect("a1");
        wal.append(FrameKind::OtlpBatch, b"good-two").expect("a2");
        wal.sync().expect("sync");
    }
    let seg = the_one_segment(tmp.path());
    let healthy_len = std::fs::metadata(&seg).expect("stat").len();
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&seg)
            .expect("open seg to corrupt");
        f.write_all(&[0xAB, 0xCD, 0xEF, 0x01, 0x02])
            .expect("write torn tail");
    }

    // Act: replay heals the torn tail; then append a third frame.
    let mut sink = CollectingSink::default();
    {
        let mut wal = open(tmp.path());
        wal.replay(&mut sink).expect("replay over a torn tail");
        assert_eq!(
            std::fs::metadata(&seg).expect("stat").len(),
            healthy_len,
            "torn tail truncated back to the last valid frame boundary",
        );
        wal.append(FrameKind::AuditEvent, b"good-three")
            .expect("append after heal");
        wal.sync().expect("sync");
    }

    // Assert: the first replay saw exactly the two intact frames,
    // and a fresh replay sees the healed three-frame sequence —
    // proof the torn bytes were removed (otherwise the third
    // frame would be unreadable past the garbage).
    assert_eq!(
        sink.frames,
        vec![
            (FrameKind::OtlpBatch, b"good-one".to_vec()),
            (FrameKind::OtlpBatch, b"good-two".to_vec()),
        ],
        "torn tail dropped, intact frames recovered",
    );
    let mut sink2 = CollectingSink::default();
    open(tmp.path()).replay(&mut sink2).expect("replay 2");
    assert_eq!(
        sink2.frames,
        vec![
            (FrameKind::OtlpBatch, b"good-one".to_vec()),
            (FrameKind::OtlpBatch, b"good-two".to_vec()),
            (FrameKind::AuditEvent, b"good-three".to_vec()),
        ],
        "post-heal append landed on the boundary; replay is clean",
    );
}

/// A *complete* frame with a corrupt payload (CRC mismatch) is
/// RFC0008.5 corruption, not a torn tail: `replay` halts with a
/// `Corrupt` error naming the segment, byte, and reason, and
/// hands nothing to the sink. This holds even on the newest
/// segment — only a short read (genuine torn tail) gets the
/// clean-truncation treatment.
#[test]
fn crc_corruption_halts_replay_without_feeding_the_sink() {
    // Arrange: one synced frame, then flip a payload byte so the
    // stored CRC no longer matches.
    let tmp = tempfile::TempDir::new().expect("temp");
    let segment_uuid = {
        let mut wal = open(tmp.path());
        let off = wal
            .append(FrameKind::OtlpBatch, b"corrupt-me")
            .expect("append");
        wal.sync().expect("sync");
        off.segment
    };
    let seg = the_one_segment(tmp.path());
    let mut bytes = std::fs::read(&seg).expect("read seg");
    // Flip the first payload byte (past the segment + frame
    // headers) so the stored CRC no longer matches.
    let payload_start = SEGMENT_HEADER_LEN + FRAME_HEADER_LEN;
    bytes[payload_start] ^= 0xFF;
    std::fs::write(&seg, &bytes).expect("write corrupted seg");

    // Act
    let mut sink = CollectingSink::default();
    let result = open(tmp.path()).replay(&mut sink);

    // Assert
    match result {
        Err(RecoveryError::Corrupt {
            segment,
            byte,
            reason,
        }) => {
            assert_eq!(
                reason,
                CorruptionReason::CrcMismatch,
                "reason is CRC mismatch"
            );
            assert_eq!(
                byte, SEGMENT_HEADER_LEN as u64,
                "frame starts right after the segment header",
            );
            assert_eq!(segment, segment_uuid, "Corrupt names the offending segment");
        }
        other => panic!("expected RecoveryError::Corrupt, got {other:?}"),
    }
    assert!(
        sink.frames.is_empty(),
        "a corrupt first frame is never handed to the sink",
    );
}
