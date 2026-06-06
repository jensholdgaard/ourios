//! RFC0003.1 — WAL-before-ack `[§3.4]`.
//! See `docs/rfcs/0003-otlp-receiver.md` §5.
//!
//! Red gate (`specified → red`): this acceptance test enumerates
//! the §5 contract and is `#[ignore]`'d until the OTLP receiver
//! lands. The implementing PR removes the `#[ignore]` and the
//! `unimplemented!()` body together.

/// Scenario RFC0003.1 — WAL-before-ack.
#[ignore = "RFC 0003 red gate — implementation pending (RFC0003.1)"]
#[test]
fn rfc0003_1_no_ack_before_every_record_is_durable() {
    unimplemented!(
        "RFC0003.1 — a request receives a 2xx / gRPC OK only after every record \
         in the batch is durably WAL-written (Wal::sync returns Ok). An ordering \
         probe set after sync returns is asserted true by the response writer and \
         false by every pre-sync stage (mirrors the RFC0008.1 probe)."
    );
}
