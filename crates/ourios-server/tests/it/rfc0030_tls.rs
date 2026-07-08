//! RFC 0030 §5 — TLS on the querier surface and the served
//! end-to-end (.3/.8; the receiver arms live in the
//! `ourios-ingester` harness per §6).
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.

/// Scenario RFC0030.3 — querier + MCP over TLS.
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[test]
#[ignore = "RFC0030.3 stub — implemented in the querier green slice"]
fn rfc0030_3_querier_and_mcp_over_tls() {
    todo!(
        "RFC0030.3 — querier http_tls enabled, static bearer \
         configured: query (valid bearer + X-Ourios-Tenant) and MCP \
         initialize (valid bearer) succeed over TLS; a plaintext \
         request to the same port fails at the transport layer"
    );
}

/// Scenario RFC0030.8 — served end-to-end (Collector-shaped client).
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[test]
#[ignore = "RFC0030.8 stub — implemented in the served green slice"]
fn rfc0030_8_served_end_to_end() {
    todo!(
        "RFC0030.8 — served ourios-server, both roles, TLS on both \
         receiver listeners + mTLS on gRPC + TLS querier: a \
         Collector-shaped gRPC exporter (ca_file + client pair) and \
         an HTTPS exporter both land batches queryable over the TLS \
         querier, no plaintext hop"
    );
}
