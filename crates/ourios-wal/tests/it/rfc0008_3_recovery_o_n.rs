//! RFC0008.3 — Crash-recovery non-amplification `[H3]`.
//! See `docs/rfcs/0008-wal.md` §5.
//!
//! Recovery wall time scales O(N) in segment count; per-record
//! work is dominated by the miner's tokenize cost. The wall-clock
//! evidence lives in a `criterion` benchmark
//! (`crates/ourios-bench/benches/recovery.rs`, N ∈ {1, 4, 16}
//! segments). These two integration tests are the smoke-test
//! sentinels: replay over a clean multi-segment fixture emits no
//! per-record `fsync` (`wal_syncs_total` does not advance) and no
//! side-channel beyond the delivered frames.
//!
//! The RFC0008.5 corruption-path audit event is **deferred** per
//! the 2026-06-13 §5 amendment (WAL corruption is a system event
//! with no tenant; the durable forensic record needs a
//! system-scoped-audit design that does not yet exist). So the
//! happy path emits zero audit events trivially and permanently:
//! `replay` has no audit sink at all, and the only side-channel
//! `FrameSink` exposes is `consume`. The second test pins that by
//! asserting the recovered sequence is *exactly* the appended
//! frames — no extra deliveries, in order — and that
//! `syncs_total` did not move.

use std::path::{Path, PathBuf};

use ourios_wal::{FrameKind, FrameSink, RecoveryError, Wal, WalConfig, WalOffset};

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

/// Sorted `*.wal` paths under `root`.
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
/// payload: build it in a scratch root through the public API,
/// then move the file into `dest_root`. (The `build_closed_segment`
/// pattern from `rfc0008_7_checkpoint.rs`.)
fn build_closed_segment(dest_root: &Path, payloads: &[&[u8]]) {
    let scratch = tempfile::TempDir::new().expect("scratch root");
    let mut wal = Wal::open(default_config(scratch.path())).expect("open scratch");
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

/// Replay over a clean multi-segment fixture is read-only on the
/// closed segments: `wal_syncs_total` does not advance. The §6.6
/// step-4 heal fsync is the only exception, and only on a
/// torn-tail newest segment — this fixture has no torn tail, so
/// the count stays put.
#[test]
fn rfc0008_3_recovery_emits_no_per_record_fsync() {
    let tmp = tempfile::TempDir::new().expect("temp");
    build_closed_segment(tmp.path(), &[b"a1", b"a2", b"a3"]);
    build_closed_segment(tmp.path(), &[b"b1", b"b2"]);
    build_closed_segment(tmp.path(), &[b"c1", b"c2", b"c3", b"c4"]);
    // `Wal::open` reopens the lexicographically-greatest existing
    // segment (the third) as the append target; the two earlier
    // ones are closed. Replay walks all three.
    let mut wal = Wal::open(default_config(tmp.path())).expect("open");
    let syncs_before = wal.metrics().syncs_total;

    let mut sink = CollectingSink::default();
    wal.replay(&mut sink)
        .expect("replay over clean multi-segment");

    assert_eq!(
        wal.metrics().syncs_total,
        syncs_before,
        "replay over clean closed segments performs no per-record (nor any) fsync",
    );
    assert_eq!(
        sink.frames.len(),
        9,
        "every frame across the three closed segments is delivered exactly once",
    );
}

/// The happy path emits zero audit events. The corruption-path
/// audit event is deferred (2026-06-13 §5 amendment), and `replay`
/// has no audit sink — `FrameSink::consume` is the only callback.
/// So "zero audit events" is observable as: replay returns `Ok`
/// and delivers *exactly* the appended frames, in order, with no
/// extra side-channel delivery, and `syncs_total` is unchanged.
#[test]
fn rfc0008_3_recovery_emits_no_per_record_audit_event() {
    let tmp = tempfile::TempDir::new().expect("temp");
    build_closed_segment(tmp.path(), &[b"first", b"second"]);
    build_closed_segment(tmp.path(), &[b"third"]);

    let mut wal = Wal::open(default_config(tmp.path())).expect("open");
    let syncs_before = wal.metrics().syncs_total;

    let mut sink = CollectingSink::default();
    wal.replay(&mut sink).expect("replay");

    assert_eq!(
        sink.frames,
        vec![
            (FrameKind::OtlpBatch, b"first".to_vec()),
            (FrameKind::OtlpBatch, b"second".to_vec()),
            (FrameKind::OtlpBatch, b"third".to_vec()),
        ],
        "replay delivers exactly the appended frames, in order — no side-channel",
    );
    assert_eq!(
        wal.metrics().syncs_total,
        syncs_before,
        "no fsync on the happy path either",
    );
}
