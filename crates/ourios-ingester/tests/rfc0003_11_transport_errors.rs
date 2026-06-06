//! RFC0003.11 — Transport-level errors are controlled, not panics.
//!
//! Red gate (`specified → red`): `#[ignore]`'d until the receiver
//! lands.

/// Scenario RFC0003.11 — Transport-level errors are controlled, not panics.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[ignore = "RFC 0003 red gate — implementation pending (RFC0003.11)"]
#[test]
fn rfc0003_11_transport_errors_are_controlled() {
    unimplemented!(
        "RFC0003.11 — malformed protobuf, oversize body, unrecognised \
         Content-Type, wrong path, and mid-decode gRPC cancellation each yield a \
         controlled transport error (gRPC INVALID_ARGUMENT / RESOURCE_EXHAUSTED / \
         CANCELLED; HTTP 400 / 413 / 415 / 404). No panic, process stays alive, \
         and no OtlpBatch frame is appended."
    );
}
