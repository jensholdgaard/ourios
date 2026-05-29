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

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.9)"]
#[test]
fn rfc0008_9_undersized_segment_config_is_rejected_at_open() {
    unimplemented!(
        "RFC0008.9 — Wal::open(WalConfig {{ segment_size_bytes: 1 MiB, .. }}) \
         must return OpenError::InvalidConfig naming the field + range \
         (segment size below MAX_FRAME_BYTES + headers would force unbounded \
         wal_unflushed_bytes)"
    );
}
