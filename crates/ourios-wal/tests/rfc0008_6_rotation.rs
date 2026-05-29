//! RFC0008.6 — Segment rotation.
//! See `docs/rfcs/0008-wal.md` §5.
//!
//! Red gate. Size-cap and time-cap arms; close-fsync + new-
//! header + parent-dir fsync sequence; no drop/duplicate
//! across the rotation boundary; rotation-fsync-failure
//! quiesces the WAL per §6.5.

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.6)"]
#[test]
fn rfc0008_6_size_cap_rotates_without_drop_or_duplicate() {
    unimplemented!(
        "RFC0008.6 size-cap arm — appends push past wal_segment_size_bytes; \
         the rotation closes the segment, opens a new UUIDv7 segment, the \
         next append lands in the new one, no frame is dropped or duplicated"
    );
}

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.6)"]
#[test]
fn rfc0008_6_time_cap_rotates_without_drop_or_duplicate() {
    unimplemented!("RFC0008.6 time-cap arm — same expectation when wal_segment_age_secs elapses");
}

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.6)"]
#[test]
fn rfc0008_6_rotation_fsync_failure_quiesces_the_wal() {
    unimplemented!(
        "RFC0008.6 — a rotation whose final fsync returns Err surfaces as \
         hard AppendError; subsequent appends return the same error \
         (WAL quiesced per §6.5 until operator intervention)"
    );
}
