//! RFC0008.4 — Torn last frame on the newest segment `[H3]`.
//! See `docs/rfcs/0008-wal.md` §5.
//!
//! Three arms: (a) partial header on the newest segment →
//! clean-truncate + heal via §6.6 step 4 (`ftruncate` +
//! fdatasync + parent-dir fsync), (b) partial payload on the
//! newest segment → same heal, (c) partial header/payload on an
//! **older** (closed) segment → RFC0008.5 `TornOnClosedSegment`
//! corruption (the central newest-vs-older pin). After the heal,
//! the next `append` lands on the last valid frame boundary.

use std::path::{Path, PathBuf};

use ourios_wal::{
    CorruptionReason, FrameKind, FrameSink, RecoveryError, Wal, WalConfig, WalOffset,
};

/// On-disk layout sizes (RFC 0008 §6.2.1 / §6.2.2), mirrored here
/// because the crate-internal constants are `pub(crate)`.
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

fn segment_files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(root)
        .expect("read_dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "wal"))
        .collect();
    out.sort();
    out
}

fn the_one_segment(root: &Path) -> PathBuf {
    let paths = segment_files(root);
    assert_eq!(paths.len(), 1, "exactly one segment file");
    paths.into_iter().next().expect("one segment")
}

/// Mint a closed segment holding one `OtlpBatch` frame per
/// payload in a scratch root, then move it into `dest_root`.
fn build_closed_segment(dest_root: &Path, payloads: &[&[u8]]) {
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

/// (a) Partial frame *header* on the newest segment: append +
/// sync two frames, drop the WAL, truncate the file partway into
/// the second frame's header (the first frame's boundary + 6 of
/// its 12 header bytes). Replay clean-truncates: it recovers the
/// first frame, heals the segment back to the first frame's
/// boundary, and a subsequent append resumes there.
#[test]
fn rfc0008_4_partial_header_on_newest_truncates_and_heals() {
    let tmp = tempfile::TempDir::new().expect("temp");
    {
        let mut wal = open(tmp.path());
        wal.append(FrameKind::OtlpBatch, b"first-frame")
            .expect("a1");
        wal.append(FrameKind::OtlpBatch, b"second-frame")
            .expect("a2");
        wal.sync().expect("sync");
    }
    let seg = the_one_segment(tmp.path());
    // Boundary after the first frame = the byte at which the
    // second frame's header began.
    let first_frame_boundary =
        (SEGMENT_HEADER_LEN + FRAME_HEADER_LEN + b"first-frame".len()) as u64;
    // Truncate mid-header of the second frame (6 of its 12 header
    // bytes survive → a short read on the header → torn tail).
    std::fs::OpenOptions::new()
        .write(true)
        .open(&seg)
        .expect("open seg")
        .set_len(first_frame_boundary + 6)
        .expect("truncate mid-header");

    let mut sink = CollectingSink::default();
    {
        let mut wal = open(tmp.path());
        wal.replay(&mut sink).expect("replay heals torn header");
        assert_eq!(
            std::fs::metadata(&seg).expect("stat").len(),
            first_frame_boundary,
            "torn header truncated back to the first frame's boundary",
        );
        let off = wal
            .append(FrameKind::AuditEvent, b"third-frame")
            .expect("append after heal");
        assert_eq!(
            off.byte,
            first_frame_boundary + FRAME_HEADER_LEN as u64 + b"third-frame".len() as u64,
            "the post-heal append lands on the healed boundary",
        );
        wal.sync().expect("sync");
    }
    assert_eq!(
        sink.frames,
        vec![(FrameKind::OtlpBatch, b"first-frame".to_vec())],
        "only the intact frame before the torn point was recovered",
    );
    let mut sink2 = CollectingSink::default();
    open(tmp.path()).replay(&mut sink2).expect("replay 2");
    assert_eq!(
        sink2.frames,
        vec![
            (FrameKind::OtlpBatch, b"first-frame".to_vec()),
            (FrameKind::AuditEvent, b"third-frame".to_vec()),
        ],
        "post-heal append landed cleanly; second replay sees the healed sequence",
    );
}

/// (b) Partial *payload* on the newest segment: same fixture, but
/// truncate so the second frame's 12 B header is intact and only
/// its payload is short. Same clean-truncate + heal.
#[test]
fn rfc0008_4_partial_payload_on_newest_truncates_and_heals() {
    let tmp = tempfile::TempDir::new().expect("temp");
    {
        let mut wal = open(tmp.path());
        wal.append(FrameKind::OtlpBatch, b"first-frame")
            .expect("a1");
        wal.append(FrameKind::OtlpBatch, b"second-frame")
            .expect("a2");
        wal.sync().expect("sync");
    }
    let seg = the_one_segment(tmp.path());
    let first_frame_boundary =
        (SEGMENT_HEADER_LEN + FRAME_HEADER_LEN + b"first-frame".len()) as u64;
    // Keep the full 12 B header of the second frame + 3 payload
    // bytes (its payload is 12 B) → header reads fine, payload is
    // a short read → torn tail.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&seg)
        .expect("open seg")
        .set_len(first_frame_boundary + FRAME_HEADER_LEN as u64 + 3)
        .expect("truncate mid-payload");

    let mut sink = CollectingSink::default();
    {
        let mut wal = open(tmp.path());
        wal.replay(&mut sink).expect("replay heals torn payload");
        assert_eq!(
            std::fs::metadata(&seg).expect("stat").len(),
            first_frame_boundary,
            "torn payload truncated back to the first frame's boundary",
        );
        wal.append(FrameKind::OtlpBatch, b"third-frame")
            .expect("append after heal");
        wal.sync().expect("sync");
    }
    assert_eq!(
        sink.frames,
        vec![(FrameKind::OtlpBatch, b"first-frame".to_vec())],
        "only the intact frame before the torn payload was recovered",
    );
    let mut sink2 = CollectingSink::default();
    open(tmp.path()).replay(&mut sink2).expect("replay 2");
    assert_eq!(
        sink2.frames,
        vec![
            (FrameKind::OtlpBatch, b"first-frame".to_vec()),
            (FrameKind::OtlpBatch, b"third-frame".to_vec()),
        ],
        "post-heal append resumed on the boundary; second replay is clean",
    );
}

/// (c) Torn tail on a CLOSED (non-newest) segment: build two
/// segments, truncate the OLDER one mid-frame, reopen + replay.
/// A short read on a closed segment is RFC0008.5
/// `TornOnClosedSegment` corruption (its rotation fsync should
/// have completed), naming the segment UUID + byte — not the
/// RFC0008.4 clean-truncation treatment.
#[test]
fn rfc0008_4_partial_tail_on_older_segment_is_rfc0008_5_corruption() {
    let tmp = tempfile::TempDir::new().expect("temp");
    build_closed_segment(tmp.path(), &[b"older-one", b"older-two"]);
    build_closed_segment(tmp.path(), &[b"newer-one"]);
    // `Wal::open` reopens the lexicographically-greatest existing
    // segment (`newer-one`) as the append target; the earlier
    // `older-*` segment is closed. We corrupt that closed one.
    let older = segment_files(tmp.path())
        .into_iter()
        .next()
        .expect("the oldest segment");
    let older_uuid: uuid::Uuid = older
        .file_stem()
        .and_then(|s| s.to_str())
        .expect("stem")
        .parse()
        .expect("segment filename is its UUID");
    // Boundary after the older segment's first frame; lop off a
    // few bytes past it so the second frame is a torn tail.
    let first_boundary = (SEGMENT_HEADER_LEN + FRAME_HEADER_LEN + b"older-one".len()) as u64;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&older)
        .expect("open older seg")
        .set_len(first_boundary + 4)
        .expect("truncate older mid-frame");

    let mut sink = CollectingSink::default();
    let result = open(tmp.path()).replay(&mut sink);
    match result {
        Err(RecoveryError::Corrupt {
            segment,
            byte,
            reason,
        }) => {
            assert_eq!(
                reason,
                CorruptionReason::TornOnClosedSegment,
                "a torn tail on a closed segment is corruption, not clean truncation",
            );
            assert_eq!(
                segment, older_uuid,
                "Corrupt names the offending older segment"
            );
            assert_eq!(
                byte, first_boundary,
                "byte is the start of the torn frame (the last valid boundary)",
            );
        }
        other => panic!("expected RecoveryError::Corrupt (TornOnClosedSegment), got {other:?}"),
    }
}
