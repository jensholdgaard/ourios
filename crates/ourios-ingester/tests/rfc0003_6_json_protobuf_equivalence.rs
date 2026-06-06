//! RFC0003.6 — HTTP/JSON ↔ gRPC/protobuf equivalence with OTLP-JSON encoding rules.
//! See `docs/rfcs/0003-otlp-receiver.md` §5.
//!
//! Red gate (`specified → red`): `#[ignore]`'d until the receiver
//! lands.

/// Scenario RFC0003.6 — HTTP/JSON ↔ gRPC/protobuf equivalence with OTLP-JSON encoding rules.
#[ignore = "RFC 0003 red gate — implementation pending (RFC0003.6)"]
#[test]
fn rfc0003_6_json_decodes_to_the_same_anyvalue_tree_as_protobuf() {
    unimplemented!(
        "RFC0003.6 — a valid OTLP/JSON request (whitespace + field-ordering \
         variation; hex trace/span IDs, base64 bytes, integer-valued enums, \
         unknown fields ignored) decodes to the same in-memory AnyValue tree the \
         same logical record produces over gRPC + protobuf. Equivalence asserted \
         at the AnyValue level (canonicalisation is the storage layer's, per §6.4)."
    );
}
