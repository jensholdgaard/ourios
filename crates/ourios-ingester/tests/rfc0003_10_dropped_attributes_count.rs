//! RFC0003.10 — `dropped_attributes_count` preserved verbatim.
//! See `docs/rfcs/0003-otlp-receiver.md` §5.
//!
//! Red gate (`specified → red`): `#[ignore]`'d until the receiver
//! lands.

/// Scenario RFC0003.10 — `dropped_attributes_count` preserved verbatim.
#[ignore = "RFC 0003 red gate — implementation pending (RFC0003.10)"]
#[test]
fn rfc0003_10_dropped_attributes_count_is_reflected_not_recomputed() {
    unimplemented!(
        "RFC0003.10 — a wire dropped_attributes_count of 42 yields \
         OtlpLogRecord.dropped_attributes_count == 42 exactly; the receiver \
         reflects the wire claim and never recomputes it."
    );
}
