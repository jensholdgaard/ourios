//! RFC0003.4 — Tenant resolution failure rejects the entire batch `[§3.7]`.
//!
//! Red gate (`specified → red`): `#[ignore]`'d until the receiver
//! lands.

/// Scenario RFC0003.4 — Tenant resolution failure rejects the entire batch.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[ignore = "RFC 0003 red gate — implementation pending (RFC0003.4)"]
#[test]
fn rfc0003_4_unresolvable_tenant_rejects_whole_batch() {
    unimplemented!(
        "RFC0003.4 — an export whose Resource attributes do not resolve to a \
         configured tenant rule is rejected with a transport-level error naming \
         the failing Resource and the missing attribute; no records are accepted \
         and no OtlpBatch frame is appended (all-or-nothing per §6.3)."
    );
}
