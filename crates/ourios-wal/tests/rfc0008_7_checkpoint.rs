//! RFC0008.7 — Checkpoint-driven truncation + durable sidecar.
//! See `docs/rfcs/0008-wal.md` §5.
//!
//! Four arms: normal-flow truncation, crash-between-
//! `checkpoint(X)`-and-housekeeping (the durable-sidecar
//! dedup-gap arm — without it, the recovery driver's Parquet
//! path would lose its suppression horizon and re-feed
//! already-published records), surviving-segments offset
//! reconstruction (proves the `(segment, byte)` `WalOffset`
//! semantics is enough without a synthetic global counter),
//! and the retain floor (2026-06-12 amendment — a lagging
//! snapshot's frames are held back from truncation until the
//! floor advances).
//!
//! Rotation (RFC0008.6) is not implemented yet, so multi-
//! segment roots are built by minting closed segments in
//! scratch roots and moving them in — public API only; the
//! `UUIDv7` filenames mint monotonically, so construction
//! order is chronological order.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// Mint a closed segment holding one `OtlpBatch` frame per
/// payload: build it in a scratch root through the public API,
/// then move the file into `dest_root`. Returns the per-frame
/// append offsets.
fn build_closed_segment(dest_root: &Path, payloads: &[&[u8]]) -> Vec<WalOffset> {
    let scratch = tempfile::TempDir::new().expect("scratch root");
    let mut wal = Wal::open(default_config(scratch.path())).expect("open scratch");
    let offsets = payloads
        .iter()
        .map(|p| wal.append(FrameKind::OtlpBatch, p).expect("append"))
        .collect();
    wal.sync().expect("sync");
    drop(wal);
    let seg = segment_files(scratch.path())
        .into_iter()
        .next()
        .expect("scratch holds one segment");
    let dest = dest_root.join(seg.file_name().expect("segment file name"));
    std::fs::rename(&seg, &dest).expect("move segment into dest root");
    offsets
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

#[derive(Default)]
struct OffsetSink {
    frames: Vec<(WalOffset, FrameKind, Vec<u8>)>,
}

impl FrameSink for OffsetSink {
    fn consume(
        &mut self,
        offset: WalOffset,
        kind: FrameKind,
        payload: &[u8],
    ) -> Result<(), RecoveryError> {
        self.frames.push((offset, kind, payload.to_vec()));
        Ok(())
    }
}

/// Arm 1 — `checkpoint(X)` + a housekeeping pass unlinks
/// segments wholly below `X`, keeps segments straddling it, and
/// `wal_disk_bytes` drops by exactly the unlinked bytes.
#[test]
fn rfc0008_7_normal_flow_unlinks_segments_below_checkpoint() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let _a = build_closed_segment(tmp.path(), &[b"a1", b"a2"]);
    let b = build_closed_segment(tmp.path(), &[b"b1", b"b2"]);
    let _c = build_closed_segment(tmp.path(), &[b"c1"]);
    let before = segment_files(tmp.path());
    assert_eq!(before.len(), 3, "fixture: three segments");
    let size_a = std::fs::metadata(&before[0]).expect("stat A").len();

    // Opens the newest (C) as the current append segment.
    let mut wal = Wal::open(default_config(tmp.path())).expect("open");

    // X = mid-B (B's first frame): A is wholly below, B straddles.
    // disk_bytes is sampled after the checkpoint so the delta
    // isolates the unlink (the checkpoint itself adds the 32 B
    // sidecar).
    wal.checkpoint(b[0]).expect("checkpoint mid-B");
    let disk_before = wal.metrics().disk_bytes;
    wal.housekeeping(None).expect("housekeeping");

    let after = segment_files(tmp.path());
    assert_eq!(
        after,
        before[1..].to_vec(),
        "A (wholly ≤ X) is unlinked; B (straddling) and C survive",
    );
    assert_eq!(
        wal.metrics().disk_bytes,
        disk_before - size_a,
        "wal_disk_bytes drops by the deleted segment's size",
    );

    // Advancing the checkpoint past B's last frame reclaims B too.
    wal.checkpoint(b[1]).expect("checkpoint end-B");
    wal.housekeeping(None).expect("housekeeping");
    assert_eq!(
        segment_files(tmp.path()),
        before[2..].to_vec(),
        "B is reclaimed once the checkpoint passes its highest frame",
    );
}

/// Arm 2 — SIGKILL between `checkpoint(X)` and housekeeping:
/// the `CHECKPOINT` sidecar survives the crash
/// (`last_checkpoint() == Some(X)`), `replay` still delivers
/// every frame with its offset, and partitioning the delivered
/// frames on `X` yields exactly the already-published prefix
/// (what the driver's Parquet path suppresses) and the
/// unpublished tail (what it re-feeds) — no data-side dup.
#[test]
fn rfc0008_7_crash_between_checkpoint_and_housekeeping_does_not_duplicate() {
    let tmp = tempfile::TempDir::new().expect("temp");
    // Checkpoint lands at BB's offset; CC is appended after.
    let x = run_fixture_then_sigkill(
        tmp.path(),
        &[
            "otlp:AA",
            "otlp:BB",
            "SYNC",
            "CHECKPOINT",
            "otlp:CC",
            "SYNC",
        ],
    )
    .expect("fixture checkpointed");

    let mut wal = Wal::open(default_config(tmp.path())).expect("reopen after crash");
    assert_eq!(
        wal.last_checkpoint(),
        Some(x),
        "the CHECKPOINT sidecar survives the crash (atomic write + fsync)",
    );

    let mut sink = OffsetSink::default();
    wal.replay(&mut sink).expect("replay after crash");
    let payloads: Vec<&[u8]> = sink.frames.iter().map(|(_, _, p)| p.as_slice()).collect();
    assert_eq!(
        payloads,
        vec![&[0xAA][..], &[0xBB], &[0xCC]],
        "replay delivers every surviving frame — suppression is the driver's job",
    );
    let suppressed: Vec<&[u8]> = sink
        .frames
        .iter()
        .filter(|(off, _, _)| *off <= x)
        .map(|(_, _, p)| p.as_slice())
        .collect();
    assert_eq!(
        suppressed,
        vec![&[0xAA][..], &[0xBB]],
        "frames ≤ X are exactly the already-published prefix the Parquet path suppresses",
    );
}

/// Arm 3 — after housekeeping deletes older segments, a fresh
/// `Wal::open` + a new `checkpoint(Y > X)` proceeds against the
/// surviving UUID-named files alone: the `(segment, byte)`
/// `WalOffset` representation needs no global counter rebuilt.
#[test]
fn rfc0008_7_surviving_segments_after_housekeeping_have_well_defined_offsets() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let _a = build_closed_segment(tmp.path(), &[b"a1"]);
    let b = build_closed_segment(tmp.path(), &[b"b1"]);
    let _c = build_closed_segment(tmp.path(), &[b"c1"]);

    let x = b[0];
    {
        let mut wal = Wal::open(default_config(tmp.path())).expect("open");
        wal.checkpoint(x).expect("checkpoint end-B");
        wal.housekeeping(None).expect("housekeeping");
    }
    assert_eq!(segment_files(tmp.path()).len(), 1, "only C survives");

    let mut wal = Wal::open(default_config(tmp.path())).expect("fresh open");
    assert_eq!(
        wal.last_checkpoint(),
        Some(x),
        "sidecar outlives the segments below it",
    );
    let y = wal
        .append(FrameKind::OtlpBatch, b"c2")
        .expect("append on C");
    wal.sync().expect("sync");
    assert!(
        y > x,
        "C's UUIDv7 sorts above B's, so Y > X with no global counter",
    );
    wal.checkpoint(y)
        .expect("checkpoint(Y > X) against survivors");
    let metrics = wal.metrics();
    assert_eq!(metrics.checkpoint_segment, Some(y.segment));
    assert_eq!(metrics.checkpoint_byte, y.byte);
    drop(wal);

    let wal = Wal::open(default_config(tmp.path())).expect("reopen");
    assert_eq!(
        wal.last_checkpoint(),
        Some(y),
        "the advanced checkpoint is durable",
    );
}

/// Arm 4 (2026-06-12 amendment) — a retain floor `S < X` holds
/// back segments holding frames in `(S, X]`; advancing the
/// floor to `X` on a later pass reclaims them. The floor
/// delays truncation, never cancels it.
#[test]
fn rfc0008_7_retain_floor_holds_back_truncation_until_it_advances() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let a = build_closed_segment(tmp.path(), &[b"a1", b"a2"]);
    let _b = build_closed_segment(tmp.path(), &[b"b1"]);
    let before = segment_files(tmp.path());
    assert_eq!(before.len(), 2, "fixture: two segments");

    let mut wal = Wal::open(default_config(tmp.path())).expect("open");
    let x = a[1]; // Parquet has published all of A...
    let s = a[0]; // ...but the miner snapshot only covers a1.
    wal.checkpoint(x).expect("checkpoint end-A");

    wal.housekeeping(Some(s))
        .expect("housekeeping with lagging floor");
    assert_eq!(
        segment_files(tmp.path()),
        before,
        "A holds a2 in (S, X] — the floor retains it for the miner's catch-up",
    );

    wal.housekeeping(Some(x))
        .expect("housekeeping with advanced floor");
    assert_eq!(
        segment_files(tmp.path()),
        before[1..].to_vec(),
        "once the floor advances to X, A is reclaimed",
    );
}

/// The live append segment is identified by its header UUID,
/// not its path: even renamed out from under the writer — and
/// even when the checkpoint covers its every frame — it is
/// never unlinked. Unlinking it would leave the writer
/// appending into an unlinked inode no later `Wal::open` would
/// see.
#[test]
fn rfc0008_7_housekeeping_never_unlinks_a_renamed_current_segment() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let mut wal = Wal::open(default_config(tmp.path())).expect("open");
    let off = wal.append(FrameKind::OtlpBatch, b"x").expect("append");
    wal.sync().expect("sync");
    // The current segment's highest frame is exactly the
    // checkpoint — wholly ≤ bound, so only the identity guard
    // protects it.
    wal.checkpoint(off).expect("checkpoint");

    let seg = segment_files(tmp.path())
        .into_iter()
        .next()
        .expect("the one segment");
    let renamed = tmp.path().join("0-renamed.wal");
    std::fs::rename(&seg, &renamed).expect("rename live segment");

    wal.housekeeping(None).expect("housekeeping");
    assert!(
        renamed.exists(),
        "the renamed live append segment survives — identity is the header UUID, not the path",
    );
}

/// Spawn the crash fixture against `root`, applying `ops`, wait
/// for its `READY` line, then SIGKILL it. Returns the offset
/// echoed by a `CHECKPOINT` op, if any.
fn run_fixture_then_sigkill(root: &Path, ops: &[&str]) -> Option<WalOffset> {
    let exe = env!("CARGO_BIN_EXE_wal_crash_fixture");
    let mut child = Command::new(exe)
        .arg(root)
        .args(ops)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn crash fixture");
    let stdout = child.stdout.take().expect("fixture stdout piped");
    let mut reader = BufReader::new(stdout);
    let mut checkpointed = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("read fixture stdout");
        assert!(
            n > 0,
            "fixture exited without READY — it died before committing its ops",
        );
        let line = line.trim_end();
        if line == "READY" {
            break;
        }
        if let Some(rest) = line.strip_prefix("CHECKPOINTED ") {
            let (uuid, byte) = rest.split_once(' ').expect("CHECKPOINTED <uuid> <byte>");
            checkpointed = Some(WalOffset {
                segment: uuid.parse().expect("fixture-echoed uuid"),
                byte: byte.parse().expect("fixture-echoed byte"),
            });
        }
    }
    // SIGKILL: uncatchable, no graceful WAL close.
    child.kill().expect("SIGKILL fixture");
    child.wait().expect("reap fixture");
    checkpointed
}
