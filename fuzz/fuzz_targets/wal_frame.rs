#![no_main]

//! RFC0015.4 — WAL frame decode is a panic oracle on untrusted bytes.
//! `read_frame` (exposed via `ourios-wal`'s `fuzzing` feature) must
//! return `Ok` or a typed `FrameError` (bad CRC, oversize length,
//! unknown kind, non-zero pad, short read), never panic or hit UB —
//! including on truncated headers and length fields that overrun.
//!
//! RFC0015.9 — and the writer's frames are the reader's: `read_frame` of
//! `write_frame(kind, payload)` yields exactly `(kind, payload)` and
//! consumes exactly the frame. Both paths run on every input, in one
//! target: the whole input is fed to `read_frame` raw, and its first byte
//! picks the `FrameKind` for a round trip of the remaining bytes as the
//! payload. One binary rather than two keeps the `ClusterFuzzLite` target
//! count (and the sanitizer build) down; both paths live in the same
//! `ourios_wal::frame` module, so per-target coverage attribution loses
//! nothing.

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use ourios_wal::frame::{read_frame, write_frame};
use ourios_wal::{FrameKind, MAX_FRAME_BYTES};

#[path = "../common/telemetry.rs"]
mod telemetry;

fuzz_target!(init: telemetry::init(), |data: &[u8]| {
    let _ = read_frame(&mut Cursor::new(data));

    let Some((&selector, payload)) = data.split_first() else {
        return;
    };
    // `write_frame`'s precondition (`Wal::append` checks it); a payload past
    // the cap is not a frame the writer would ever produce.
    if payload.len() > MAX_FRAME_BYTES {
        return;
    }
    let kind = match selector % 3 {
        0 => FrameKind::OtlpBatch,
        1 => FrameKind::AuditEvent,
        _ => FrameKind::TenantOtlpBatch,
    };

    let mut frame = Vec::with_capacity(12 + payload.len());
    write_frame(&mut frame, kind, payload)
        .unwrap_or_else(|e| panic!("RFC0015.9: write_frame into a Vec failed: {e}"));
    let mut cursor = Cursor::new(frame.as_slice());
    match read_frame(&mut cursor) {
        Ok((read_kind, read_payload)) => {
            assert_eq!(read_kind, kind, "RFC0015.9: frame kind did not round-trip");
            assert!(
                read_payload == payload,
                "RFC0015.9: a {}-byte {kind:?} payload did not round-trip",
                payload.len(),
            );
            assert_eq!(
                usize::try_from(cursor.position()).ok(),
                Some(frame.len()),
                "RFC0015.9: read_frame did not consume exactly the frame it was given",
            );
        }
        Err(e) => panic!(
            "RFC0015.9: read_frame rejected a frame write_frame produced ({kind:?}, {} bytes): {e}",
            payload.len()
        ),
    }
});
