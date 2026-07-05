//! RFC 0026 §5 — the server-owned scenarios: token-store
//! configuration (`.1`), the query-path status contract (`.4`), the
//! query half of wildcard binding (`.5`), and open-mode parity
//! (`.6`). The ingest-side scenarios (`.2`/`.3`, the ingest half of
//! `.5`, and `.7`) live in
//! `crates/ourios-ingester/tests/rfc0026_auth.rs`.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.

/// Scenario RFC0026.1 — token store configuration.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[test]
#[ignore = "RFC0026.1 stub — implemented in the config green slice"]
fn rfc0026_1_token_store_configuration() {
    todo!(
        "RFC0026.1 — auth.tokens resolves env-var indirection (the RFC 0020 \
         substitution syntax) at startup; empty tokens list is a startup \
         error; a missing auth section starts open with a structured warning"
    );
}

/// Scenario RFC0026.4 — query enforcement and status contract.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[test]
#[ignore = "RFC0026.4 stub — implemented in the query green slice"]
fn rfc0026_4_query_status_contract() {
    todo!(
        "RFC0026.4 — 401 missing/unknown bearer; 400 missing/empty \
         x-ourios-tenant (unchanged); 403 out-of-set tenant; 200 with \
         correct results in-set; drift endpoint under the same gate"
    );
}

/// Scenario RFC0026.5 (query half) — wildcard binding.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[test]
#[ignore = "RFC0026.5 stub — implemented in the query green slice"]
fn rfc0026_5_wildcard_binding_query() {
    todo!(
        "RFC0026.5 — a tenants: [\"*\"] token queries arbitrary tenants as \
         if every tenant were listed"
    );
}

/// Scenario RFC0026.6 — open-mode parity.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[test]
#[ignore = "RFC0026.6 stub — implemented in the query green slice (parity is asserted by the existing suites plus this warning check)"]
fn rfc0026_6_open_mode_parity() {
    todo!(
        "RFC0026.6 — with no auth section the existing ingest + query \
         acceptance suites pass unchanged and the startup warning is \
         emitted exactly once"
    );
}
