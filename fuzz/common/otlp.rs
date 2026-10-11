//! The OTLP round-trip oracle both decoder targets assert, shared as a source
//! file (`#[path]`, like `telemetry.rs`) so the fuzz package keeps no library
//! API (RFC 0015 §3.1).

#![deny(unsafe_code)]

use prost::Message;

/// RFC0015.9 — a request the decoder accepts must re-encode to a fixpoint:
/// the bytes of `encode(decode(encode(request)))` equal `encode(request)`,
/// and the intermediate decode must succeed. Bytes are compared rather than
/// messages: a `NaN` double is a legal wire value that is unequal to itself
/// under `PartialEq`, and prost writes float payload bits verbatim, so the
/// byte comparison is both `NaN`-safe and stricter.
///
/// # Panics
///
/// When the encoder's bytes are rejected by the decoder, or when the second
/// encoding differs from the first.
pub fn assert_encode_fixpoint<M: Message + Default>(scenario: &str, request: &M) {
    let encoded = request.encode_to_vec();
    let decoded = M::decode(encoded.as_slice()).unwrap_or_else(|e| {
        panic!("{scenario}: the decoder rejects what the encoder produced ({e})")
    });
    assert!(
        decoded.encode_to_vec() == encoded,
        "{scenario}: decode → encode is not a fixpoint ({} bytes in)",
        encoded.len(),
    );
}
