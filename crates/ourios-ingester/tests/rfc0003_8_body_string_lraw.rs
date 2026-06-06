//! RFC0003.8 — `Body::String` reaches the miner as the unwrapped `L_raw`.
//!
//! Red gate (`specified → red`): `#[ignore]`'d until the receiver
//! lands.

/// Scenario RFC0003.8 — `Body::String` reaches the miner as the unwrapped `L_raw`.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[ignore = "RFC 0003 red gate — implementation pending (RFC0003.8)"]
#[test]
fn rfc0003_8_string_body_reaches_miner_unwrapped() {
    unimplemented!(
        "RFC0003.8 — a string body becomes OtlpLogRecord.body = \
         Some(Body::String(s)) where s is the original UTF-8 string (no wrapping, \
         quoting, or escaping); the value handed to MinerCluster::ingest equals s \
         byte-for-byte (instrumented MinerCluster stub records the body argument)."
    );
}
