//! RFC0008.9 — `wal_unflushed_bytes` is bounded `[H3 detection]`.
//! See `docs/rfcs/0008-wal.md` §5.
//!
//! `proptest`-driven mix of frame sizes; the metric never exceeds
//! `2 × wal_segment_size_bytes`. The bound is achievable because
//! the §6.9 lower bound on `wal_segment_size_bytes`
//! (≥ `MAX_FRAME_BYTES` + headers) guarantees a max-sized frame
//! fits inside a single segment — and `append` rotates *before* a
//! write that would overflow the segment, resetting
//! `unflushed_bytes` on the closing fdatasync. A configuration
//! below the lower bound is rejected at `Wal::open` time.

use ourios_wal::{FrameKind, MAX_FRAME_BYTES, MIN_SEGMENT_SIZE_BYTES, Wal, WalConfig};
use proptest::prelude::*;

fn config(root: &std::path::Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        // The §6.9 lower bound: 17 MiB, the smallest segment that
        // fits a MAX_FRAME_BYTES (16 MiB) frame. Exercising the
        // tightest legal segment makes the 2× bound the hardest
        // to hold.
        segment_size_bytes: MIN_SEGMENT_SIZE_BYTES,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    }
}

/// One step of the randomised arrival pattern: append a frame of
/// the given size, or sync.
#[derive(Debug, Clone)]
enum Step {
    Append(usize),
    Sync,
}

/// A frame size mixing small (1..=256 B) and large (up to
/// `MAX_FRAME_BYTES`) frames — the large frames are what make the
/// bound non-trivial, so they must appear, but they are capped in
/// count so the 16 MiB allocations don't make the test slow.
fn frame_size() -> impl Strategy<Value = usize> {
    prop_oneof![
        4 => 1usize..=256,
        1 => (MAX_FRAME_BYTES - 64)..=MAX_FRAME_BYTES,
    ]
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => frame_size().prop_map(Step::Append),
        1 => Just(Step::Sync),
    ]
}

proptest! {
    // Modest case + length caps: large frames are 16 MiB each, so
    // keep the per-case work small.
    #![proptest_config(ProptestConfig::with_cases(12))]
    #[test]
    fn rfc0008_9_unflushed_bytes_bounded_under_random_arrival(
        steps in proptest::collection::vec(step(), 1..=20),
    ) {
        let tmp = tempfile::TempDir::new().expect("temp");
        let mut wal = Wal::open(config(tmp.path())).expect("open");
        let bound = 2 * MIN_SEGMENT_SIZE_BYTES;

        // One reusable max-sized buffer, sliced per append — a fresh
        // `vec![…; n]` per step would repeatedly allocate + zero
        // ~16 MiB and dominate the runtime. Content is irrelevant to
        // the byte-accounting bound.
        let scratch = vec![0xA5u8; MAX_FRAME_BYTES];

        // Sample at the start (must be zero) and after every
        // append/sync boundary.
        prop_assert!(wal.metrics().unflushed_bytes <= bound);
        for s in steps {
            match s {
                Step::Append(n) => {
                    wal.append(FrameKind::OtlpBatch, &scratch[..n]).expect("append");
                }
                Step::Sync => {
                    wal.sync().expect("sync");
                }
            }
            prop_assert!(
                wal.metrics().unflushed_bytes <= bound,
                "unflushed_bytes {} exceeded 2 × segment_size {}",
                wal.metrics().unflushed_bytes,
                bound,
            );
        }
    }
}

/// Live with PR-M4 (`Wal::open` + segment header). The other
/// RFC0008.9 arm — bounded `wal_unflushed_bytes` under random
/// arrival — is the proptest above.
#[test]
fn rfc0008_9_undersized_segment_config_is_rejected_at_open() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let bad = ourios_wal::WalConfig {
        root: tmp.path().to_path_buf(),
        batch_window_ms: 100,
        // Below MIN_SEGMENT_SIZE_BYTES (17 MiB). A segment
        // this small can't fit a MAX_FRAME_BYTES-sized frame,
        // which would violate RFC0008.9's bound on
        // `wal_unflushed_bytes`.
        segment_size_bytes: 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    };
    let err = ourios_wal::Wal::open(bad).expect_err("undersized segment must be rejected");
    match err {
        ourios_wal::OpenError::InvalidConfig { field, detail } => {
            assert_eq!(field, "segment_size_bytes");
            assert!(
                detail.contains("below §6.9 lower bound"),
                "error must explain the §6.9 lower-bound violation; got {detail:?}",
            );
        }
        other => panic!("expected InvalidConfig, got {other:?}"),
    }
}
