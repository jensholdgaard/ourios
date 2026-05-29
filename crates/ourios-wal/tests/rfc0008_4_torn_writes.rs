//! RFC0008.4 — Torn last frame on the newest segment `[H3]`.
//! See `docs/rfcs/0008-wal.md` §5.
//!
//! Red gate. Three arms: (a) partial header on newest →
//! clean-truncate + heal via §6.6 step 4 (`ftruncate` +
//! fdatasync + parent-dir fsync), (b) partial payload on
//! newest → same heal, (c) partial header/payload on an
//! **older** (closed) segment → RFC0008.5 corruption (the
//! central newest-vs-older pin). The next `append` after
//! recovery lands at the frame boundary preceding the torn
//! write.

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.4)"]
#[test]
fn rfc0008_4_partial_header_on_newest_truncates_and_heals() {
    unimplemented!(
        "RFC0008.4 (a) — truncate newest segment mid-header; replay heals via \
         §6.6 step 4 ftruncate + fdatasync + parent-dir fsync; next append \
         lands at the frame boundary preceding the torn write"
    );
}

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.4)"]
#[test]
fn rfc0008_4_partial_payload_on_newest_truncates_and_heals() {
    unimplemented!("RFC0008.4 (b) — same heal expectation for mid-payload truncation");
}

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.4)"]
#[test]
fn rfc0008_4_partial_tail_on_older_segment_is_rfc0008_5_corruption() {
    unimplemented!(
        "RFC0008.4 (c) — torn tail on a CLOSED (non-newest) segment surfaces \
         as RFC0008.5 corruption, not clean truncation (the newest-vs-older \
         pin §6.6 introduced)"
    );
}
