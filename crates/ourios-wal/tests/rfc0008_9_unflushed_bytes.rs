//! RFC0008.9 — `wal_unflushed_bytes` is bounded `[H3 detection]`.
//! See `docs/rfcs/0008-wal.md` §5.
//!
//! Red gate. `proptest`-driven mix of frame sizes; the metric
//! never exceeds `2 × wal_segment_size_bytes`. The bound is
//! achievable because the §6.9 lower bound on
//! `wal_segment_size_bytes` (≥ `MAX_FRAME_BYTES` + headers)
//! guarantees a max-sized frame fits inside a single segment.
//! A configuration below the lower bound is rejected at
//! `Wal::open` time.

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.9)"]
#[test]
fn rfc0008_9_unflushed_bytes_bounded_under_random_arrival() {
    unimplemented!(
        "RFC0008.9 — proptest a random arrival pattern; sample \
         wal_unflushed_bytes at every append/sync boundary; assert ≤ \
         2 × wal_segment_size_bytes throughout"
    );
}

/// Live with PR-M4 (`Wal::open` + segment header). The other
/// RFC0008.9 arm — bounded `wal_unflushed_bytes` under random
/// arrival — stays `#[ignore]`'d until `append` / `sync` ship.
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
