//! RFC 0032 §5 — the query-schema / cost-model resource, all six
//! scenarios.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! Placement note: all six stubs live here, in the RFC 0027 in-process
//! MCP harness area (`rfc0027_mcp.rs`'s router + JSON-RPC-over-`/mcp`
//! shape, RFC 0032 §6), matching the RFC 0033 red precedent of one
//! file per RFC for §5→stub traceability. The `.3`/`.4` green work is
//! unit tests beside the resource builder in `mcp.rs` (RFC 0032 §6);
//! their stubs stay here so the §5 map is one file.

/// Scenario RFC0032.1 — listed and readable.
/// See `docs/rfcs/0032-query-schema-cost-model-resource.md` §5.
#[test]
#[ignore = "RFC0032.1 stub — implemented in the resource + config-threading green slice"]
fn rfc0032_1_listed_and_readable() {
    todo!(
        "RFC0032.1 — querier.mcp.enabled: resources/list advertises \
         exactly two resources (the RFC 0027 grammar resource and \
         ourios://query-schema, application/json); resources/read on \
         ourios://query-schema parses as JSON with format_version: 1 \
         and the §3.2 top-level keys (fields, severity, \
         promoted_attributes, cost_model); tools/list still \
         advertises exactly the RFC 0027 §3.2 three — no new tool"
    );
}

/// Scenario RFC0032.2 — content matches the running config.
/// See `docs/rfcs/0032-query-schema-cost-model-resource.md` §5.
#[test]
#[ignore = "RFC0032.2 stub — implemented in the resource + config-threading green slice"]
fn rfc0032_2_content_matches_running_config() {
    todo!(
        "RFC0032.2 — storage.promoted_attributes configured with \
         resource and log keys: the resource's promoted_attributes \
         equals the effective PromotedAttributes set (service.name \
         first, configured keys deduplicated in order); with the \
         section omitted, .resource is [\"service.name\"] and .log is \
         empty; two servers with different promoted sets serve \
         different resource bodies"
    );
}

/// Scenario RFC0032.3 — severity scale correctness.
/// See `docs/rfcs/0032-query-schema-cost-model-resource.md` §5.
#[test]
#[ignore = "RFC0032.3 stub — implemented in the document-correctness green slice (unit tests beside the mcp.rs builder)"]
fn rfc0032_3_severity_scale_correctness() {
    todo!(
        "RFC0032.3 — the severity.names entries equal the DSL's \
         SeverityName mapping: for each of the six names, floor \
         equals SeverityName::floor and ceil equals \
         SeverityName::ceil, asserted against the ourios-querier \
         functions, not repeated literals, so the resource cannot \
         drift from the compiler"
    );
}

/// Scenario RFC0032.4 — cost-tier classification stability.
/// See `docs/rfcs/0032-query-schema-cost-model-resource.md` §5.
#[test]
#[ignore = "RFC0032.4 stub — implemented in the document-correctness green slice (unit tests beside the mcp.rs builder)"]
fn rfc0032_4_cost_tier_classification_stability() {
    todo!(
        "RFC0032.4 — every cost_model.classification entry with \
         mechanism \"bloom\" names only columns the writer actually \
         bloom-filters: the expected set derives from the writer's \
         properties for the configured PromotedAttributes \
         (template_id, trace_id, span_id, and every \
         PromotedAttributes::column_names column) and the resource's \
         index-backed equality kinds cover exactly the DSL fields \
         backed by that set; severity's entry carries mechanism \
         \"statistics\", never \"bloom\"; no classification entry \
         carries a numeric cost value"
    );
}

/// Scenario RFC0032.5 — tool-description placement.
/// See `docs/rfcs/0032-query-schema-cost-model-resource.md` §5.
#[test]
#[ignore = "RFC0032.5 stub — implemented in the tool-descriptions + read-only green slice"]
fn rfc0032_5_tool_description_placement() {
    todo!(
        "RFC0032.5 — tools/list: each of query_logs, list_templates, \
         and template_drift carries exactly one advisory sentence \
         naming ourios://query-schema, and no tool description \
         enumerates tiers, severity bands, or promoted keys (the \
         full tiering lives only in the resource)"
    );
}

/// Scenario RFC0032.6 — read-only contract preserved.
/// See `docs/rfcs/0032-query-schema-cost-model-resource.md` §5.
#[test]
#[ignore = "RFC0032.6 stub — implemented in the tool-descriptions + read-only green slice"]
fn rfc0032_6_read_only_contract_preserved() {
    todo!(
        "RFC0032.6 — with the amendment applied, the RFC 0027 §5 \
         suite passes verbatim (same tools, same outputs, grammar \
         resource byte-identical); reading ourios://query-schema \
         performs no query, touches no tenant data, and its body \
         contains no ingested-telemetry-derived content; an unknown \
         resource URI still returns the resource-not-found error"
    );
}
