//! RFC0003.5 — gRPC ≡ HTTP/protobuf decode equivalence.
//!
//! Red gate (`specified → red`): `#[ignore]`'d until the receiver
//! lands.

/// Scenario RFC0003.5 — gRPC ≡ HTTP/protobuf decode equivalence.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[ignore = "RFC 0003 red gate — implementation pending (RFC0003.5)"]
#[test]
fn rfc0003_5_grpc_and_http_protobuf_decode_identically() {
    unimplemented!(
        "RFC0003.5 — a byte-equal protobuf payload delivered over gRPC and over \
         HTTP/x-protobuf produces the identical in-memory ExportLogsServiceRequest \
         (per §8, exercised by a proptest strategy over the proto value space)."
    );
}
