#![no_main]

//! RFC0015.8 — the RFC 0002 DSL parsers are panic oracles on untrusted
//! query text: `/v1/query` (RFC 0016) and the MCP surface (RFC 0027) hand
//! them raw client bytes, so garbage must come back as a typed `DslError`
//! — never a panic, abort, or UB. Both grammars share the oracle: the
//! pipe-text parser and the structured-JSON variant get the same input
//! (each rejects the other's happy path as garbage, which is exactly the
//! point). The statement entry points are used so the RFC 0010 `drift`
//! head is covered too.
//!
//! What either parser accepts must round-trip through the canonical
//! serialiser (RFC0002.7): `parse_statement(serialize_statement(s)) == s`.
//! The parsers canonicalise associative `and`/`or` on the way in, so the
//! IR they produce is exactly the shape the serialiser promises to
//! reproduce (the proptest in `dsl/structured.rs` pins the same contract
//! over generated trees).

use libfuzzer_sys::fuzz_target;
use ourios_querier::dsl::{
    Statement, parse_statement, parse_structured_statement, serialize_statement,
};

#[path = "../common/telemetry.rs"]
mod telemetry;

fn assert_round_trips(scenario: &str, statement: &Statement) {
    let canonical = serialize_statement(statement);
    let reparsed = parse_statement(&canonical).unwrap_or_else(|e| {
        panic!("{scenario} / RFC0002.7: canonical form {canonical:?} does not parse: {e}")
    });
    assert!(
        &reparsed == statement,
        "{scenario} / RFC0002.7: canonical form {canonical:?} re-parses to a different query\n  \
         parsed:   {statement:?}\n  reparsed: {reparsed:?}",
    );
}

// A typed `&str` target: libFuzzer produces valid UTF-8 directly, so no
// execution is wasted on byte sequences the real boundary cannot deliver
// (axum/MCP hand the parsers `String`s, so invalid UTF-8 is rejected
// upstream of them in production).
fuzz_target!(init: telemetry::init(), |text: &str| {
    if let Ok(statement) = parse_statement(text) {
        assert_round_trips("RFC0015.8 (pipe text)", &statement);
    }
    if let Ok(statement) = parse_structured_statement(text) {
        assert_round_trips("RFC0015.8 (structured JSON)", &statement);
    }
});
