#![no_main]

//! RFC0015.2 — OTLP/JSON decode is a panic oracle on untrusted input.
//! `decode_json` must reject garbage with a typed `DecodeError`, never
//! panic, abort, or exhibit UB.
//!
//! RFC0015.9 — and a request it accepts (on the strict or the lenient
//! path) must survive the protobuf round trip the WAL depends on
//! (`common/otlp.rs`). JSON re-encoding is not asserted: the proto3-JSON
//! mapping is not canonical (`1` vs `"1"` for 64-bit ints, base64
//! padding, enum names vs numbers), so equal JSON is not a contract.

use libfuzzer_sys::fuzz_target;
use ourios_ingester::receiver::decode_json;

#[path = "../common/otlp.rs"]
mod otlp;
#[path = "../common/telemetry.rs"]
mod telemetry;

fuzz_target!(init: telemetry::init(), |data: &[u8]| {
    if let Ok((request, _lenient)) = decode_json(data) {
        otlp::assert_encode_fixpoint("RFC0015.9 (otlp_json)", &request);
    }
});
