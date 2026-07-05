//! RFC 0026 §5 — the ingest-owned scenarios: authentication (`.2`),
//! whole-batch tenant binding before the WAL ack (`.3`), the ingest
//! half of wildcard binding (`.5`), and rejection telemetry/audit
//! (`.7`). The server-owned scenarios (`.1`/`.4`, the query half of
//! `.5`, `.6`) live in `crates/ourios-server/tests/rfc0026_auth.rs`.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.

/// Scenario RFC0026.2 — ingest authentication.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[test]
#[ignore = "RFC0026.2 stub — implemented in the ingest green slice"]
fn rfc0026_2_ingest_authentication() {
    todo!(
        "RFC0026.2 — missing/unknown bearer on gRPC and HTTP is rejected \
         (UNAUTHENTICATED / 401) before wire decode; nothing reaches the \
         WAL; no ack"
    );
}

/// Scenario RFC0026.3 — ingest tenant binding.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[test]
#[ignore = "RFC0026.3 stub — implemented in the ingest green slice"]
fn rfc0026_3_ingest_tenant_binding() {
    todo!(
        "RFC0026.3 — a batch within the token's tenant set acks normally; \
         any out-of-set ResourceLogs group rejects the whole batch \
         (PERMISSION_DENIED / 403) with no WAL append and no partial \
         success"
    );
}

/// Scenario RFC0026.5 (ingest half) — wildcard binding.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[test]
#[ignore = "RFC0026.5 stub — implemented in the ingest green slice"]
fn rfc0026_5_wildcard_binding_ingest() {
    todo!(
        "RFC0026.5 — a tenants: [\"*\"] token ingests to arbitrary tenants \
         as if every tenant were listed"
    );
}

/// Scenario RFC0026.7 — rejection telemetry and audit.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[test]
#[ignore = "RFC0026.7 stub — implemented in the telemetry green slice"]
fn rfc0026_7_rejection_telemetry_and_audit() {
    todo!(
        "RFC0026.7 — rejections increment existing counters with \
         error.type (unauthenticated | permission_denied); ingest authz \
         rejection emits an audit event with the token name and offending \
         tenant; token values never appear on any surface"
    );
}
