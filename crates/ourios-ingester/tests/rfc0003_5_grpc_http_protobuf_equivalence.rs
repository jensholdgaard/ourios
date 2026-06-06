//! RFC0003.5 — gRPC ≡ HTTP/protobuf decode equivalence.
//!
//! The gRPC and HTTP `application/x-protobuf` transports both deliver
//! the *same* protobuf payload, so they share one decoder
//! ([`ourios_ingester::receiver::decode_protobuf`]). This pins that the
//! decoder is transport-agnostic and faithful: a byte-equal payload
//! yields an equal `ExportLogsServiceRequest` either way, and decode
//! round-trips the original. Per §8, exercised by a proptest strategy
//! over the proto value space (shared with RFC0003.6, in
//! `tests/otlp_strategy`).

mod otlp_strategy;

use otlp_strategy::export_request;
use ourios_ingester::receiver::decode_protobuf;
use proptest::prelude::*;
use prost::Message;

/// Scenario RFC0003.5 — gRPC ≡ HTTP/protobuf decode equivalence.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_5_grpc_and_http_protobuf_decode_identically() {
    proptest!(|(req in export_request())| {
        let bytes = req.encode_to_vec();
        // The gRPC and HTTP/x-protobuf transports hand the same payload
        // bytes to the same decoder, so decoding via either path is
        // identical.
        let from_grpc = decode_protobuf(&bytes).expect("gRPC-framed payload decodes");
        let from_http = decode_protobuf(&bytes).expect("HTTP-framed payload decodes");
        prop_assert_eq!(&from_grpc, &from_http);
        // ...and the decode is faithful — it round-trips the original.
        prop_assert_eq!(from_grpc, req);
    });
}
