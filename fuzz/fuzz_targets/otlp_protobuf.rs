#![no_main]

//! RFC0015.3 — OTLP/protobuf decode is a panic oracle on untrusted
//! input. `decode_protobuf` must reject garbage with a typed
//! `DecodeError`, never panic, abort, or exhibit UB.

use libfuzzer_sys::fuzz_target;
use ourios_ingester::receiver::decode_protobuf;

fuzz_target!(|data: &[u8]| {
    let _ = decode_protobuf(data);
});
