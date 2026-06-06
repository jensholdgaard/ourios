//! RFC0003.7 — `Body::Structured` carries the decoded `AnyValue` verbatim.
//! See `docs/rfcs/0003-otlp-receiver.md` §5.
//!
//! Red gate (`specified → red`): `#[ignore]`'d until the receiver
//! lands.

/// Scenario RFC0003.7 — `Body::Structured` carries the decoded `AnyValue` verbatim.
#[ignore = "RFC 0003 red gate — implementation pending (RFC0003.7)"]
#[test]
fn rfc0003_7_structured_body_reaches_miner_as_verbatim_anyvalue() {
    unimplemented!(
        "RFC0003.7 — a structured body reaches the miner as \
         Body::Structured(AnyValue) structurally equal to the wire AnyValue (no \
         canonicalisation, no reshape, no dropped fields), and the same equality \
         holds across all three transports."
    );
}
