//! RFC0003.3 — Tenant fan-out `[§3.7]`.
//!
//! Red gate (`specified → red`): `#[ignore]`'d until the receiver
//! lands.

/// Scenario RFC0003.3 — Tenant fan-out.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[ignore = "RFC 0003 red gate — implementation pending (RFC0003.3)"]
#[test]
fn rfc0003_3_two_resources_fan_out_without_cross_contamination() {
    unimplemented!(
        "RFC0003.3 — a single export with two ResourceLogs from different sources \
         produces two distinct per-tenant streams; no record from Resource A \
         appears in tenant B's stream. Asserted via an instrumented MinerCluster \
         stub recording every accepted (tenant_id, OtlpLogRecord) pair."
    );
}
