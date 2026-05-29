//! RFC0008.8 — Batched-fsync knob.
//! See `docs/rfcs/0008-wal.md` §5.
//!
//! Red gate. Three settings (`10` / `100` / `1000` ms);
//! P99 ack latency tracks the configured window within
//! ±30 %; `wal_syncs_total` advances per-batch not per-record;
//! the §3.4 invariant holds across all three.

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.8)"]
#[test]
fn rfc0008_8_p99_latency_tracks_batch_window() {
    unimplemented!(
        "RFC0008.8 — exercise the receiver at wal_batch_window_ms ∈ {{10, 100, \
         1000}}; assert P99 ack latency tracks the configured window within \
         ±30 % and is dominated by the window, not per-record fsync"
    );
}

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.8)"]
#[test]
fn rfc0008_8_syncs_advance_per_batch_not_per_record() {
    unimplemented!(
        "RFC0008.8 — wal_syncs_total advances at ~(arrival_rate / per-window \
         batch size), not per-record; appends_per_sync >> 1 in steady state"
    );
}
