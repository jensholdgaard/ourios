//! RFC0003.15 — Concurrent `Export` calls each obey WAL-before-ack independently `[§3.4]`.
//!
//! Red gate (`specified → red`): `#[ignore]`'d until the receiver
//! lands.

/// Scenario RFC0003.15 — Concurrent `Export` calls each obey WAL-before-ack independently.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[ignore = "RFC 0003 red gate — implementation pending (RFC0003.15)"]
#[test]
fn rfc0003_15_concurrent_exports_each_ack_after_their_own_sync() {
    unimplemented!(
        "RFC0003.15 — N >= 2 concurrent gRPC Export calls from independent \
         connections each emit their ack only after their OWN batch's Wal::sync \
         returns Ok and their own batch's records are durable. A per-call ordering \
         probe checks the invariant independently per in-flight call."
    );
}
