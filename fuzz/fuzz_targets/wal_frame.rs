#![no_main]

//! RFC0015.4 — WAL frame decode is a panic oracle on untrusted bytes.
//! `read_frame` (exposed via `ourios-wal`'s `fuzzing` feature) must
//! return `Ok` or a typed `FrameError` (bad CRC, oversize length,
//! unknown kind, non-zero pad, short read), never panic or hit UB —
//! including on truncated headers and length fields that overrun.

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use ourios_wal::frame::read_frame;

fuzz_target!(|data: &[u8]| {
    let mut cursor = Cursor::new(data);
    let _ = read_frame(&mut cursor);
});
