//! RFC0008.7 — Checkpoint-driven truncation + durable sidecar.
//! See `docs/rfcs/0008-wal.md` §5.
//!
//! Red gate. Three arms: normal-flow truncation, crash-
//! between-`checkpoint(X)`-and-housekeeping (the durable-
//! sidecar dedup-gap arm — without it, replay would
//! duplicate already-published records), and surviving-
//! segments offset reconstruction (proves the `(segment,
//! byte)` `WalOffset` semantics is enough without a synthetic
//! global counter).

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.7)"]
#[test]
fn rfc0008_7_normal_flow_unlinks_segments_below_checkpoint() {
    unimplemented!(
        "RFC0008.7 arm 1 — checkpoint(X) + housekeeping pass unlinks segments \
         wholly below X, keeps segments straddling it; wal_disk_bytes drops"
    );
}

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.7)"]
#[test]
fn rfc0008_7_crash_between_checkpoint_and_housekeeping_does_not_duplicate() {
    unimplemented!(
        "RFC0008.7 arm 2 — SIGKILL between checkpoint(X) and the housekeeping \
         unlink; restart asserts the CHECKPOINT sidecar survived AND replay \
         skips frames ≤ X (no data-side dup). Without this arm a non-durable \
         CHECKPOINT slips through unobserved."
    );
}

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.7)"]
#[test]
fn rfc0008_7_surviving_segments_after_housekeeping_have_well_defined_offsets() {
    unimplemented!(
        "RFC0008.7 arm 3 — checkpoint advanced past older segments + \
         housekeeping deleted them; fresh Wal::open + new checkpoint(Y > X) \
         proceeds against the surviving UUID-named files without a global \
         counter (pins the §6.1 (segment, byte) WalOffset semantics)"
    );
}
