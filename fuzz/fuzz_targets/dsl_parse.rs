#![no_main]

//! The RFC 0002 DSL parsers are panic oracles on untrusted query text:
//! `/v1/query` (RFC 0016) and the MCP surface (RFC 0027) hand them raw
//! client bytes, so garbage must come back as a typed `DslError` —
//! never a panic, abort, or UB. Both grammars share the oracle: the
//! pipe-text parser and the structured-JSON variant get the same input
//! (each rejects the other's happy path as garbage, which is exactly
//! the point).

use libfuzzer_sys::fuzz_target;
use ourios_querier::dsl::{parse, parse_structured};

// A typed `&str` target: libFuzzer produces valid UTF-8 directly, so no
// execution is wasted on byte sequences the real boundary cannot deliver
// (axum/MCP hand the parsers `String`s, so invalid UTF-8 is rejected
// upstream of them in production).
fuzz_target!(|text: &str| {
    let _ = parse(text);
    let _ = parse_structured(text);
});
