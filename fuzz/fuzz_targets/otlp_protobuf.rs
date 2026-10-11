#![no_main]

//! RFC0015.3 — OTLP/protobuf decode is a panic oracle on untrusted
//! input. `decode_protobuf` must reject garbage with a typed
//! `DecodeError`, never panic, abort, or exhibit UB.
//!
//! RFC0015.9 — and what it accepts must survive a round trip: the decoded
//! request re-encodes, decodes again, and re-encodes to the same bytes
//! (`common/otlp.rs`). The WAL stores the re-encoded form, so a decoder
//! that accepts bytes the encoder cannot reproduce would persist something
//! other than what was acknowledged.

use libfuzzer_sys::fuzz_target;
use ourios_ingester::receiver::decode_protobuf;

#[path = "../common/otlp.rs"]
mod otlp;
#[path = "../common/telemetry.rs"]
mod telemetry;

fuzz_target!(init: telemetry::init(), |data: &[u8]| {
    if let Ok(request) = decode_protobuf(data) {
        otlp::assert_encode_fixpoint("RFC0015.9 (otlp_protobuf)", &request);
    }
});
