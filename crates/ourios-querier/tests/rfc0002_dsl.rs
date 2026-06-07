//! RFC 0002 — Query DSL acceptance criteria (RFC0002.1–.11).
//!
//! Red gate (`specified → red`): `#[ignore]`'d `unimplemented!()` stubs
//! until the DSL parser + compiler land in front of the (already
//! implemented) RFC 0007 execution layer. Per `docs/verification.md` §3
//! the scenarios become ignored stubs first, implementations second; each
//! carries the §2.2 doc-comment form so the spec↔test mapping is
//! greppable.

/// Scenario RFC0002.1 — A Branch-B predicate parses and compiles to a filter.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.1)"]
#[test]
fn rfc0002_1_predicate_compiles_to_a_filter() {
    unimplemented!(
        "RFC0002.1 — a Branch-B predicate parses to the query IR and \
         compiles to an internal DataFusion Filter; predicates over the \
         RFC 0007 §4.3 pushdown keys (template_id, time_unix_nano) prune \
         the scan identically to the equivalent ourios_querier request."
    );
}

/// Scenario RFC0002.2 — String DSL and structured surface compile to the same plan.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.2)"]
#[test]
fn rfc0002_2_string_and_structured_surfaces_agree() {
    unimplemented!(
        "RFC0002.2 — a query expressed as a β string and as the structured \
         JSON surface produce the same query IR (hence the same plan)."
    );
}

/// Scenario RFC0002.3 — No `DataFusion`/arrow/SQL leakage.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.3)"]
#[test]
fn rfc0002_3_no_datafusion_leakage() {
    unimplemented!(
        "RFC0002.3 — no datafusion/arrow/SQL type or message appears in \
         any public DSL signature or error string (compile + string-level \
         boundary test, mirroring RFC0007.3)."
    );
}

/// Scenario RFC0002.4 — A query without an explicit range gets the tenant default window.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.4)"]
#[test]
fn rfc0002_4_default_time_window() {
    unimplemented!(
        "RFC0002.4 — a query with no range(...) stage compiles with a \
         time-column filter equal to the tenant default window W, never an \
         unbounded scan."
    );
}

/// Scenario RFC0002.5 — Bare-identifier severity maps to its `SeverityNumber`.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.5)"]
#[test]
fn rfc0002_5_severity_name_maps_to_severity_number() {
    unimplemented!(
        "RFC0002.5 — severity >= error (and warn/info/debug/trace/fatal) \
         map case-insensitively to the §6.1 SeverityNumber floors \
         (trace 1, debug 5, info 9, warn 13, error 17, fatal 21), \
         compiling identically to the numeric form (severity >= 17)."
    );
}

/// Scenario RFC0002.6 — First-class OTel-canonical fields resolve correctly.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.6)"]
#[test]
fn rfc0002_6_first_class_fields_resolve() {
    unimplemented!(
        "RFC0002.6 — service, trace_id, span_id, scope, ts, observed_ts \
         resolve to the RFC 0001 §6.1 columns / resource attributes \
         (service → resource[\"service.name\"], ts → time_unix_nano, …)."
    );
}

/// Scenario RFC0002.7 — Parse/serialise round-trip is idempotent.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.7)"]
#[test]
fn rfc0002_7_round_trip_idempotent() {
    unimplemented!(
        "RFC0002.7 — any well-formed query parsed → serialised → parsed \
         equals the first parse (AST idempotence; property test)."
    );
}

/// Scenario RFC0002.8 — A malformed query yields a specific, leak-free error.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.8)"]
#[test]
fn rfc0002_8_malformed_query_specific_error() {
    unimplemented!(
        "RFC0002.8 — a syntactically/semantically invalid query returns a \
         specific error citing the offending token/clause and the §7 \
         grammar — never a panic, never a DataFusion message."
    );
}

/// Scenario RFC0002.9 — Template primitives compile.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.9)"]
#[test]
fn rfc0002_9_template_primitives_compile() {
    unimplemented!(
        "RFC0002.9 — template_id == 42, resolves_to(42), lossy == true, \
         confidence < 0.7 each compile to the documented plan \
         (resolves_to expands to the RFC 0001 §6.7 alias-set membership)."
    );
}

/// Scenario RFC0002.10 — A query is a YAML-safe single-line scalar.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.10)"]
#[test]
fn rfc0002_10_yaml_safe_single_line() {
    unimplemented!(
        "RFC0002.10 — the canonical serialisation of any well-formed query \
         is a single-line scalar that survives a YAML round-trip and \
         re-parses to the same query (the Perses-embedding guarantee)."
    );
}

/// Scenario RFC0002.11 — The structured surface validates against its published schema.
/// See `docs/rfcs/0002-query-dsl.md` §5.
#[ignore = "RFC 0002 red gate — DSL parser/compiler pending (RFC0002.11)"]
#[test]
fn rfc0002_11_structured_surface_schema_validation() {
    unimplemented!(
        "RFC0002.11 — well-formed structured (MCP) requests pass the \
         published JSON Schema and compile; malformed ones are rejected by \
         the schema before reaching the planner."
    );
}
