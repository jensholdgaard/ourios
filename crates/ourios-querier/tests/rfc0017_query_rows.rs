//! RFC 0017 — read-time template registry & query-row rendering, the
//! typed-row-payload scenarios (`.6`, `.7`, `.8`).
//!
//! **Status: `red`.** Failing stubs driving the `green` implementation: they
//! encode RFC 0017 §5 scenarios .6 (the typed-row payload is returned,
//! B1/B2-compatible), .7 (no engine internals leak — H6), and .8 (every
//! persisted OTLP field round-trips on read) and currently `todo!()`. They are
//! `#[ignore]`d so the default `cargo test` (and CI) stays green until the
//! row-returning `execute` path and `LogRow` land; `green` replaces the bodies
//! with the real assertions and removes the `#[ignore]`s.
//!
//! See `docs/rfcs/0017-template-registry-query-rendering.md` §3.4 / §5 / §6.

/// Scenario RFC0017.6 — for a query with a `limit`, `QueryResult.records` holds
/// up to `limit` `LogRow`s (body + marker + the OTLP fields per §3.4), while
/// `QueryResult.rows` (the count) and `stats` are unchanged so B1/B2 and
/// existing tests still pass; `QueryResult` is marked `#[non_exhaustive]`.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[test]
#[ignore = "RFC0017.6 — red until QueryResult.records is populated, B1/B2-compatible (green)"]
fn rfc0017_6_typed_row_payload_returned_b1b2_compatible() {
    todo!(
        "RFC0017.6: records holds <= limit LogRows; rows + stats unchanged; QueryResult #[non_exhaustive]"
    )
}

/// Scenario RFC0017.7 — the public `LogRow` / `QueryResult` surface exposes no
/// `arrow` / `DataFusion` / SQL type or text; all fields are Ourios-owned (H6).
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[test]
#[ignore = "RFC0017.7 — red until the LogRow / QueryResult surface is fully Ourios-owned (green)"]
fn rfc0017_7_no_engine_internals_leak() {
    todo!(
        "RFC0017.7: no arrow/datafusion/SQL type appears on the public LogRow / QueryResult surface (H6)"
    )
}

/// Scenario RFC0017.8 — a stored row whose ingest carried the full OTLP
/// LogRecord field set is returned as a `LogRow` where each field equals what
/// the schema stored (RFC 0005 §3.2), `attributes` / `resource_attributes` are
/// decoded to structured key/values (not an opaque JSON blob), and no stored
/// OTLP field is dropped on the read path.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[test]
#[ignore = "RFC0017.8 — red until every persisted OTLP field round-trips on read (green)"]
fn rfc0017_8_every_persisted_otlp_field_round_trips() {
    todo!(
        "RFC0017.8: each stored OTLP field equals the ingested value on read; attributes decoded to structured KVs"
    )
}
