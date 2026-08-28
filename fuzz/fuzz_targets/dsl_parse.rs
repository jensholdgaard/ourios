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

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = parse(text);
        let _ = parse_structured(text);
    }
});
