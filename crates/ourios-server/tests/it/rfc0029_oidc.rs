//! RFC 0029 §5 — the OIDC bearer layer, all seven scenarios.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.

/// Scenario RFC0029.1 — config resolution.
/// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
#[test]
#[ignore = "RFC0029.1 stub — implemented in the config green slice"]
fn rfc0029_1_config_resolution() {
    todo!(
        "RFC0029.1 — auth.oidc resolves through ${{env:VAR}}; missing \
         audience is a startup error; neither tokens nor oidc is a \
         startup error; explicit tokens: [] is a startup error even \
         with oidc present; oidc-only serves; missing auth stays open \
         mode with the RFC 0026 warning"
    );
}

/// Scenario RFC0029.2 — verification matrix.
/// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
#[test]
#[ignore = "RFC0029.2 stub — implemented in the verifier green slice"]
fn rfc0029_2_verification_matrix() {
    todo!(
        "RFC0029.2 — fixture-issuer valid token accepted; expired / \
         nbf-beyond-skew / wrong-aud / wrong-iss / bad-sig / alg:none \
         / HMAC-downgrade / non-JWT all one undifferentiated 401 \
         before wire decode, nothing reaching the WAL"
    );
}

/// Scenario RFC0029.3 — claim binding drives unchanged enforcement.
/// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
#[test]
#[ignore = "RFC0029.3 stub — implemented in the binding green slice"]
fn rfc0029_3_claim_binding_enforcement() {
    todo!(
        "RFC0029.3 — tenant_claim [a, b]: RFC 0026 §5.3/§5.4 verbatim \
         with the OIDC-resolved binding — in-set ingest acks, any \
         out-of-set batch whole-batch 403 with no WAL append, \
         401→400→403 on query + MCP, name_claim as the name label"
    );
}

/// Scenario RFC0029.4 — wildcard claim.
/// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
#[test]
#[ignore = "RFC0029.4 stub — implemented in the binding green slice"]
fn rfc0029_4_wildcard_claim() {
    todo!(
        "RFC0029.4 — tenant_claim [\"*\"]: ingest and query to \
         arbitrary tenants as if every tenant were listed \
         (RFC 0026 §5.5 parity)"
    );
}

/// Scenario RFC0029.5 — coexistence and resolution order.
/// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
#[test]
#[ignore = "RFC0029.5 stub — implemented in the binding green slice"]
fn rfc0029_5_coexistence_and_resolution_order() {
    todo!(
        "RFC0029.5 — static + oidc side by side, each with its own \
         binding; static-only and oidc-only both serve; no auth \
         section passes the RFC 0026 §5.6 open-mode parity arm \
         unchanged"
    );
}

/// Scenario RFC0029.6 — JWKS rotation.
/// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
#[test]
#[ignore = "RFC0029.6 stub — implemented in the verifier green slice"]
fn rfc0029_6_jwks_rotation() {
    todo!(
        "RFC0029.6 — issuer rotates mid-run: unseen kid triggers a \
         JWKS re-fetch and the new-key token verifies without \
         restart; the withdrawn key's tokens are rejected once the \
         refreshed set drops it"
    );
}

/// Scenario RFC0029.7 — Dex end-to-end with telemetry parity.
/// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
#[test]
#[ignore = "RFC0029.7 stub — implemented in the dex acceptance green slice"]
fn rfc0029_7_dex_end_to_end() {
    todo!(
        "RFC0029.7 — real Dex container (testcontainers, CI-gated): \
         client-credentials mint drives ingest/query/MCP; short-TTL \
         expiry rejected as the undifferentiated 401; unchanged \
         error.type values; ingest_denied carries the name_claim \
         value; no JWT material on any surface"
    );
}
