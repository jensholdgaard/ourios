//! RFC0046.6 — the RFC 0008 invariants hold for the new `TenantOtlpBatch`
//! (`0x03`) frame kind: they are payload-agnostic, so this file adds the
//! frame-kind dimension the RFC 0008 harnesses did not need — round trip
//! (RFC0008.1/.2 shape), torn-tail healing on the newest segment
//! (RFC0008.4 shape) and a payload bit-flip on a closed segment as
//! corruption (RFC0008.5 shape) — without editing those criteria.
//! See `docs/rfcs/0046-out-of-band-tenancy.md` §5.

use std::path::{Path, PathBuf};

use ourios_wal::{
    FrameKind, FrameSink, MIN_SEGMENT_SIZE_BYTES, RecoveryError, TenantBatch, Wal, WalConfig,
    WalOffset,
};

fn config(root: &Path, segment_size_bytes: u64) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes,
        segment_age_secs: 600,
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

#[derive(Default, Debug)]
struct Collect(Vec<(FrameKind, String, Vec<u8>)>);
impl FrameSink for Collect {
    fn consume(
        &mut self,
        _offset: WalOffset,
        kind: FrameKind,
        payload: &[u8],
    ) -> Result<(), RecoveryError> {
        let batch = TenantBatch::decode(payload).expect("0x03 payload");
        self.0
            .push((kind, batch.tenant.to_owned(), batch.protobuf.to_vec()));
        Ok(())
    }
}

fn frame(tenant: &str, body: &[u8]) -> Vec<u8> {
    TenantBatch::encode(tenant, body).expect("frame")
}

fn replay(root: &Path, segment_size_bytes: u64) -> Result<Collect, RecoveryError> {
    let mut sink = Collect::default();
    Wal::open(config(root, segment_size_bytes))
        .expect("open")
        .replay(&mut sink)?;
    Ok(sink)
}

/// RFC0008.1/.2 shape: fsynced 0x03 frames replay in order with kind,
/// tenant and payload intact.
#[test]
fn rfc0046_6_tenant_frames_round_trip_through_replay() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    {
        let mut wal = Wal::open(config(root, MIN_SEGMENT_SIZE_BYTES)).expect("open");
        for (t, b) in [
            ("acme", b"one" as &[u8]),
            ("globex", b"two"),
            ("acme", b"three"),
        ] {
            wal.append(FrameKind::TenantOtlpBatch, &frame(t, b))
                .expect("append");
        }
        wal.sync().expect("sync");
    }
    let got = replay(root, MIN_SEGMENT_SIZE_BYTES).expect("replay").0;
    assert_eq!(
        got,
        vec![
            (FrameKind::TenantOtlpBatch, "acme".into(), b"one".to_vec()),
            (FrameKind::TenantOtlpBatch, "globex".into(), b"two".to_vec()),
            (FrameKind::TenantOtlpBatch, "acme".into(), b"three".to_vec()),
        ]
    );
}

/// RFC0008.4 shape: a torn tail on the newest segment heals — the complete
/// 0x03 frames before it replay, the torn one is dropped, and the next
/// append lands on a frame boundary.
#[test]
fn rfc0046_6_torn_tail_heals_for_tenant_frames() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    {
        let mut wal = Wal::open(config(root, MIN_SEGMENT_SIZE_BYTES)).expect("open");
        wal.append(FrameKind::TenantOtlpBatch, &frame("acme", b"kept"))
            .expect("append");
        wal.append(FrameKind::TenantOtlpBatch, &frame("acme", b"torn-away"))
            .expect("append");
        wal.sync().expect("sync");
    }
    let newest = segment_files(root).pop().expect("a segment");
    let len = std::fs::metadata(&newest).expect("meta").len();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&newest)
        .expect("open segment");
    file.set_len(len - 3).expect("truncate mid-frame");
    drop(file);

    let got = replay(root, MIN_SEGMENT_SIZE_BYTES)
        .expect("torn tail heals")
        .0;
    assert_eq!(
        got.len(),
        1,
        "the complete frame survives, the torn one is dropped"
    );
    assert_eq!(got[0].2, b"kept");

    // The healed segment accepts a new frame on the boundary and replays all.
    {
        let mut wal = Wal::open(config(root, MIN_SEGMENT_SIZE_BYTES)).expect("reopen");
        wal.append(FrameKind::TenantOtlpBatch, &frame("globex", b"after"))
            .expect("append after heal");
        wal.sync().expect("sync");
    }
    let got = replay(root, MIN_SEGMENT_SIZE_BYTES).expect("replay").0;
    assert_eq!(got.len(), 2);
    assert_eq!(got[1].1, "globex");
}

/// RFC0008.5 shape: a payload bit-flip inside a closed segment's 0x03 frame
/// is CRC-mismatch corruption — the frame's own integrity check, not the
/// tenant prefix, is what fails.
#[test]
fn rfc0046_6_payload_bit_flip_in_tenant_frame_is_corruption() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    {
        // A tiny segment size closes the first segment after one frame.
        let mut wal = Wal::open(config(root, MIN_SEGMENT_SIZE_BYTES)).expect("open");
        let big = vec![b'x'; usize::try_from(MIN_SEGMENT_SIZE_BYTES).expect("usize") / 2];
        wal.append(FrameKind::TenantOtlpBatch, &frame("acme", &big))
            .expect("append 1");
        wal.sync().expect("sync");
        wal.append(FrameKind::TenantOtlpBatch, &frame("acme", &big))
            .expect("append 2 rotates");
        wal.sync().expect("sync");
    }
    let segments = segment_files(root);
    assert!(segments.len() >= 2, "rotation produced a closed segment");
    let closed = &segments[0];
    let mut bytes = std::fs::read(closed).expect("read");
    let flip_at = bytes.len() - 8;
    bytes[flip_at] ^= 0x01;
    std::fs::write(closed, &bytes).expect("write back");

    let err = replay(root, MIN_SEGMENT_SIZE_BYTES).expect_err("corruption halts replay");
    assert!(matches!(err, RecoveryError::Corrupt { .. }), "{err:?}");
}
