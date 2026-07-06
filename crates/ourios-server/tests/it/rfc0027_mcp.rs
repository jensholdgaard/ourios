//! RFC 0027 §5 — the MCP query surface, all seven scenarios.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.

/// Scenario RFC0027.1 — gating and placement.
/// See `docs/rfcs/0027-mcp-query-surface.md` §5.
#[test]
#[ignore = "RFC0027.1 stub — implemented in the transport green slice"]
fn rfc0027_1_gating_and_placement() {
    todo!(
        "RFC0027.1 — querier.mcp.enabled off: /mcp is 404 and the JSON \
         API endpoints are behaviorally unchanged; on: /mcp speaks MCP \
         streamable HTTP on the same listener; no new crate"
    );
}

/// Scenario RFC0027.2 — the RFC 0026 gate applies verbatim.
/// See `docs/rfcs/0027-mcp-query-surface.md` §5.
#[test]
#[ignore = "RFC0027.2 stub — implemented in the transport green slice"]
fn rfc0027_2_rfc0026_gate_applies_verbatim() {
    todo!(
        "RFC0027.2 — missing/unknown bearer rejected before tool \
         dispatch; out-of-set tenant fails tenant-denied touching no \
         data; open mode serves MCP as it serves the JSON API"
    );
}

/// Scenario RFC0027.3 — `query_logs`.
/// See `docs/rfcs/0027-mcp-query-surface.md` §5.
#[test]
#[ignore = "RFC0027.3 stub — implemented in the tools green slice"]
fn rfc0027_3_query_logs() {
    todo!(
        "RFC0027.3 — count + limited rendered rows + pruning stats, \
         equal to the JSON API's answer for the same statement; DSL \
         errors surface as tool errors, never transport failures"
    );
}

/// Scenario RFC0027.4 — `list_templates`.
/// See `docs/rfcs/0027-mcp-query-surface.md` §5.
#[test]
#[ignore = "RFC0027.4 stub — implemented in the tools green slice"]
fn rfc0027_4_list_templates() {
    todo!(
        "RFC0027.4 — (template_id, rendered_template, version) rows \
         matching the RFC 0017 registry surface for the tenant"
    );
}

/// Scenario RFC0027.5 — `template_drift`.
/// See `docs/rfcs/0027-mcp-query-surface.md` §5.
#[test]
#[ignore = "RFC0027.5 stub — implemented in the tools green slice"]
fn rfc0027_5_template_drift() {
    todo!(
        "RFC0027.5 — the analysis over [from, to) equals the RFC 0010 \
         drift surface's for the same half-open window"
    );
}

/// Scenario RFC0027.6 — the grammar resource.
/// See `docs/rfcs/0027-mcp-query-surface.md` §5.
#[test]
#[ignore = "RFC0027.6 stub — implemented in the resource green slice"]
fn rfc0027_6_grammar_resource() {
    todo!(
        "RFC0027.6 — the served resource is byte-identical to the \
         RFC 0002 §7 grammar section of docs/rfcs/0002-query-dsl.md \
         (include_str!, trimmed at startup)"
    );
}

/// Scenario RFC0027.7 — output discipline.
/// See `docs/rfcs/0027-mcp-query-surface.md` §5.
#[test]
#[ignore = "RFC0027.7 stub — implemented in the tools green slice"]
fn rfc0027_7_output_discipline() {
    todo!(
        "RFC0027.7 — tool results are the RFC 0016 JSON shapes as MCP \
         content; every tool description carries the treat-log-bodies-\
         as-data warning; no tenant enumeration, no SQL"
    );
}
