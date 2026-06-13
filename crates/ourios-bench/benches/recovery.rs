//! RFC0008.3 — WAL crash-recovery non-amplification (`docs/rfcs/0008-wal.md`
//! §5; `docs/benchmarks.md`).
//!
//! Supportive **wall-clock** evidence that `Wal::replay` scales O(N) in
//! segment count — the recovery loop does per-record work (a frame decode +
//! CRC) and no per-record fsync or audit emission. The structural side (no
//! `wal_syncs_total` advance, exactly-the-appended-frames delivery) is pinned
//! deterministically in `ourios-wal`'s `rfc0008_3_*` integration tests; here
//! we measure the thing those can't: the recovery time as a function of N.
//!
//! `recovery/{1,4,16}` — each group element replays a root holding N closed
//! segments of `FRAMES_PER_SEGMENT` modest frames apiece. Segments are minted
//! through the public API in scratch roots and moved in (rotation fills a
//! 128 MiB segment — far too slow for a bench), so construction is fast and
//! the replay walk is the only thing timed.

use std::hint::black_box;
use std::path::{Path, PathBuf};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use ourios_wal::{FrameKind, FrameSink, RecoveryError, Wal, WalConfig, WalOffset};

/// Frames per minted segment — enough to make the decode loop the dominant
/// per-segment cost, small enough to keep fixture construction quick.
const FRAMES_PER_SEGMENT: usize = 256;
/// Representative frame payload size (a small structured-log batch).
const PAYLOAD_LEN: usize = 512;
const SEGMENT_COUNTS: [usize; 3] = [1, 4, 16];

fn config(root: &Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
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

/// Mint one closed segment of `FRAMES_PER_SEGMENT` frames in a scratch root,
/// then move it into `dest_root`.
fn build_closed_segment(dest_root: &Path) {
    let scratch = tempfile::TempDir::new().expect("scratch root");
    let mut wal = Wal::open(config(scratch.path())).expect("open scratch");
    let payload = vec![0xA5u8; PAYLOAD_LEN];
    for _ in 0..FRAMES_PER_SEGMENT {
        wal.append(FrameKind::OtlpBatch, &payload).expect("append");
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

/// A recovery sink that does the minimum real work (counts frames) without
/// retaining payloads, so the timing reflects the replay walk, not sink-side
/// allocation.
#[derive(Default)]
struct CountingSink {
    count: u64,
}

impl FrameSink for CountingSink {
    fn consume(
        &mut self,
        _offset: WalOffset,
        _kind: FrameKind,
        _payload: &[u8],
    ) -> Result<(), RecoveryError> {
        self.count += 1;
        Ok(())
    }
}

fn recovery(c: &mut Criterion) {
    let mut group = c.benchmark_group("recovery");
    for n in SEGMENT_COUNTS {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let tmp = tempfile::TempDir::new().expect("temp root");
                    for _ in 0..n {
                        build_closed_segment(tmp.path());
                    }
                    tmp
                },
                |tmp| {
                    // `Wal::open` reopens the lexicographically-greatest
                    // of the N segments as the append target; replay
                    // then walks all N.
                    let mut wal = Wal::open(config(tmp.path())).expect("open");
                    let mut sink = CountingSink::default();
                    wal.replay(&mut sink).expect("replay");
                    black_box(sink.count);
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, recovery);
criterion_main!(benches);
