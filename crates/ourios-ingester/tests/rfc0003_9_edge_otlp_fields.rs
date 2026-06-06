//! RFC0003.9 — Edge OTLP fields pass through unchanged.
//! See `docs/rfcs/0003-otlp-receiver.md` §5.
//!
//! Red gate (`specified → red`): `#[ignore]`'d until the receiver
//! lands.

/// Scenario RFC0003.9 — Edge OTLP fields pass through unchanged.
#[ignore = "RFC 0003 red gate — implementation pending (RFC0003.9)"]
#[test]
fn rfc0003_9_edge_otlp_fields_are_not_coalesced() {
    unimplemented!(
        "RFC0003.9 — severity_number = 0 (UNSPECIFIED) is kept as 0, scope_name = \
         None, and wire observed_time_unix_nano = 0 maps to None (the Option<u64> \
         conversion this scenario owns). The record is accepted by \
         MinerCluster::ingest without rejection, coalescing, substitution, or \
         downcast to a default."
    );
}
