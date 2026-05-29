//! RFC0008.5 — Corrupt frame `[H3]`.
//! See `docs/rfcs/0008-wal.md` §5 (now agrees with §8 — five
//! arms, one per sub-case).
//!
//! Red gate. Each arm asserts (a) the structured
//! `RecoveryError::Corrupt` names the segment `UUIDv7` + byte
//! offset, (b) a `WalRecoveryCorruption` audit event is
//! emitted, (c) recovery stops scanning *all* segments (no
//! records past the corrupt frame are replayed).

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.5)"]
#[test]
fn rfc0008_5_payload_bit_flip_is_crc_mismatch_corruption() {
    unimplemented!(
        "RFC0008.5 arm 1 — flip a random bit in a random frame's payload on a \
         closed segment; replay emits CrcMismatch corruption + audit event + \
         stops scanning all segments"
    );
}

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.5)"]
#[test]
fn rfc0008_5_unknown_kind_is_corruption() {
    unimplemented!(
        "RFC0008.5 arm 2 — frame `kind` ∈ 0x03..=0xFF (outside §6.2.2 reserved \
         range) surfaces as UnknownKind corruption"
    );
}

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.5)"]
#[test]
fn rfc0008_5_non_zero_pad_is_corruption() {
    unimplemented!(
        "RFC0008.5 arm 3 — 3 B reserved `_pad` MUST be zero per §6.2.2; any \
         non-zero byte surfaces as NonZeroPad corruption"
    );
}

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.5)"]
#[test]
fn rfc0008_5_oversize_len_is_corruption() {
    unimplemented!(
        "RFC0008.5 arm 4 — frame header declaring `len > MAX_FRAME_BYTES` is \
         rejected before reading any payload bytes (OversizeLen corruption)"
    );
}

#[ignore = "RFC 0008 red gate — implementation pending (RFC0008.5)"]
#[test]
fn rfc0008_5_torn_tail_on_closed_segment_is_corruption() {
    unimplemented!(
        "RFC0008.5 arm 5 — torn header/payload on a CLOSED segment is \
         TornOnClosedSegment corruption, not RFC0008.4 clean truncation"
    );
}
