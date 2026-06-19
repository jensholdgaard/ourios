//! Frame-on-disk format per RFC 0008 §6.2.2.
//!
//! ```text
//! | len:   u32_le   (payload length, excluding header + CRC)              |
//! | kind:  u8       (0x01 = OtlpBatch, 0x02 = AuditEvent; reserved >0x02) |
//! | _pad:  [u8; 3]  (reserved, MUST be zero, validated on read)           |
//! | crc32: u32_le   (CRC32-C over kind || pad || payload, Castagnoli)     |
//! | payload: [u8; len]                                                    |
//! ```
//!
//! Per-frame header = **12 B** (4 + 1 + 3 + 4). The CRC covers
//! `kind || _pad || payload` (not `len`, not its own bytes) —
//! same shape Kafka uses on record batches. The write helpers
//! (`write_frame`) treat already-checked input; `read_frame`
//! validates the on-disk length against [`MAX_FRAME_BYTES`]
//! itself, since it parses untrusted bytes.
//!
//! This module is `pub(crate)` by default — `Wal::append` (write
//! side) and `Wal::replay` (read side) compose against it without
//! needing the byte layout. The `fuzzing` feature re-exports it as
//! `pub` so the `fuzz/` `wal_frame` target can drive `read_frame`
//! directly (RFC 0015); that is not a stable public API.

use std::io::{Read, Write};

use crate::{FrameKind, MAX_FRAME_BYTES};

/// Exact on-disk length of the per-frame header per §6.2.2
/// (`4 + 1 + 3 + 4 = 12`). The payload starts at this offset
/// relative to the frame's [`WalOffset`](crate::WalOffset).
pub(crate) const FRAME_HEADER_LEN: usize = 12;

/// Reserved frame `_pad` bytes — three zero bytes between
/// `kind` and `crc32`. A non-zero value surfaces as
/// `FrameError::NonZeroPad` per §6.2.2's explicit "reserved
/// means reserved" rule.
const FRAME_PAD_ZEROS: [u8; 3] = [0, 0, 0];

/// Errors returned by [`read_frame`]. One variant per
/// RFC0008.5 sub-case so the recovery driver's audit-event
/// emission, and the integration tests, can match on the
/// specific reason rather than string-search a message.
// `non_exhaustive` because the `fuzzing` feature exposes this publicly
// (RFC 0015) and the RFC0008.5 sub-cases may grow; in-crate matches stay
// exhaustive without a wildcard.
#[derive(Debug)]
#[non_exhaustive]
pub enum FrameError {
    /// The 4-byte CRC field didn't match the recomputed
    /// CRC32-C over `kind || pad || payload`. The frame's
    /// contents differ from what was written — typical cause
    /// is bit-rot mid-payload or a torn write where the
    /// payload bytes landed partially.
    CrcMismatch { stored: u32, computed: u32 },
    /// `kind` byte was outside the defined range
    /// (`0x01..=0x02` today). The reserved range is reserved
    /// for future kinds; current readers MUST refuse rather
    /// than guess.
    UnknownKind { found: u8 },
    /// One or more `_pad` bytes were non-zero. Catches a
    /// future-format file the current build can't safely
    /// interpret.
    NonZeroPad { found: [u8; 3] },
    /// `len` field exceeds [`MAX_FRAME_BYTES`]. Always
    /// corruption — the writer rejects oversize payloads at
    /// `append` time, so an on-disk frame with `len >
    /// MAX_FRAME_BYTES` can only come from disk damage or a
    /// non-Ourios producer.
    OversizeLen { found: u32 },
    /// I/O failure (EOF inside header or payload, read error).
    /// EOF mid-frame is RFC0008.4 (torn last frame) on the
    /// newest segment and RFC0008.5 (torn-on-closed) on any
    /// other; the recovery driver disambiguates by segment.
    Io(std::io::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CrcMismatch { stored, computed } => write!(
                f,
                "frame CRC mismatch: stored 0x{stored:08x}, computed 0x{computed:08x}",
            ),
            Self::UnknownKind { found } => write!(
                f,
                "unknown frame kind 0x{found:02x} (this build understands 0x01, 0x02)",
            ),
            Self::NonZeroPad { found } => {
                write!(f, "frame `_pad` bytes MUST be zero; found {found:?}")
            }
            Self::OversizeLen { found } => write!(
                f,
                "frame `len` {found} exceeds MAX_FRAME_BYTES {MAX_FRAME_BYTES}",
            ),
            Self::Io(e) => write!(f, "frame read: {e}"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// CRC32-C input shape per §6.2.2: `kind || _pad || payload`.
/// Computed incrementally — a 4 B stack buffer holds the
/// `kind || _pad` prefix, then `crc32c_append` folds in the
/// payload slice without copying. Equivalent to one
/// `crc32c(kind || _pad || payload)` call without paying the
/// extra ~16 MiB allocation that concatenating max-sized
/// payloads would cost.
fn checksum_input(kind: u8, payload: &[u8]) -> u32 {
    let mut prefix = [0u8; 4];
    prefix[0] = kind;
    // bytes 1..4 stay zero — that's `_pad`.
    let h = crc32c::crc32c(&prefix);
    crc32c::crc32c_append(h, payload)
}

/// Serialise a frame to its exact 12 + payload-length on-disk
/// form per §6.2.2. The caller MUST have validated
/// `payload.len() ≤ MAX_FRAME_BYTES` (a `debug_assert!` here
/// guards against a future caller forgetting; release builds
/// would write a valid-but-corruption-on-read frame, which
/// `read_frame` surfaces as `OversizeLen`).
pub(crate) fn write_frame<W: Write>(
    w: &mut W,
    kind: FrameKind,
    payload: &[u8],
) -> std::io::Result<()> {
    debug_assert!(
        payload.len() <= MAX_FRAME_BYTES,
        "write_frame called with oversize payload — caller must check first",
    );
    let kind_byte = kind as u8;
    // Build the 12 B header in a single stack buffer before
    // the first `write_all`. The header + payload still take
    // two `write_all` calls (header then payload) rather than
    // one vectored write — `Wal::append`'s caller-side
    // rollback (`set_len(pre_write_byte)`) handles any partial
    // failure between or within those two calls, so we don't
    // need the vectored-write atomicity guarantee. Buffering
    // the header into one slice does mean each individual
    // `write_all` is one logical chunk.
    let mut header = [0u8; FRAME_HEADER_LEN];
    // `len` is u32_le — payload's length only fits because the
    // caller validated against MAX_FRAME_BYTES < u32::MAX.
    let len_u32 = u32::try_from(payload.len()).expect("payload.len() fits u32 (≤ MAX_FRAME_BYTES)");
    header[0..4].copy_from_slice(&len_u32.to_le_bytes());
    header[4] = kind_byte;
    header[5..8].copy_from_slice(&FRAME_PAD_ZEROS);
    let crc = checksum_input(kind_byte, payload);
    header[8..12].copy_from_slice(&crc.to_le_bytes());
    w.write_all(&header)?;
    w.write_all(payload)?;
    Ok(())
}

/// Read + validate one frame from `r`. Returns the decoded
/// `(kind, payload)` on success; one of the [`FrameError`]
/// arms on failure. Each error variant maps to one §6.2.2
/// sub-case — recovery uses the variant to populate the
/// matching `CorruptionReason` for the audit event.
///
/// # Errors
///
/// Returns [`FrameError::Io`] if the reader is short or errors,
/// [`FrameError::OversizeLen`] if the length prefix exceeds the
/// maximum frame size, [`FrameError::NonZeroPad`] if a reserved
/// pad byte is set, [`FrameError::UnknownKind`] for an unrecognised
/// frame kind, and [`FrameError::CrcMismatch`] if the payload CRC
/// does not match.
pub fn read_frame<R: Read>(r: &mut R) -> Result<(FrameKind, Vec<u8>), FrameError> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    r.read_exact(&mut header).map_err(FrameError::Io)?;
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    if (len as usize) > MAX_FRAME_BYTES {
        return Err(FrameError::OversizeLen { found: len });
    }
    let kind_byte = header[4];
    let kind = match kind_byte {
        0x01 => FrameKind::OtlpBatch,
        0x02 => FrameKind::AuditEvent,
        found => return Err(FrameError::UnknownKind { found }),
    };
    let pad = [header[5], header[6], header[7]];
    if pad != FRAME_PAD_ZEROS {
        return Err(FrameError::NonZeroPad { found: pad });
    }
    let stored_crc = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).map_err(FrameError::Io)?;
    let computed_crc = checksum_input(kind_byte, &payload);
    if stored_crc != computed_crc {
        return Err(FrameError::CrcMismatch {
            stored: stored_crc,
            computed: computed_crc,
        });
    }
    Ok((kind, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Pin the exact 12 B header layout a fresh frame
    /// serialises to. Catches a regression that reorders
    /// fields, swaps endianness, or changes which bytes the
    /// CRC covers. The §6.2.2 layout is normative; the test
    /// mirrors it byte for byte.
    #[test]
    fn frame_byte_layout_matches_rfc_6_2_2() {
        let payload = b"hello";
        let mut buf = Vec::new();
        write_frame(&mut buf, FrameKind::OtlpBatch, payload).expect("write");
        assert_eq!(buf.len(), FRAME_HEADER_LEN + payload.len());
        assert_eq!(&buf[0..4], &5u32.to_le_bytes(), "len = 5 (LE u32)");
        assert_eq!(buf[4], 0x01, "kind = OtlpBatch");
        assert_eq!(&buf[5..8], &[0, 0, 0], "_pad = three zero bytes");
        let expected_crc = checksum_input(0x01, payload);
        assert_eq!(
            &buf[8..12],
            &expected_crc.to_le_bytes(),
            "CRC32-C over `kind || pad || payload` (LE u32)",
        );
        assert_eq!(&buf[12..], payload, "payload follows header verbatim");
    }

    /// Round-trip: write → read → identical kind + payload.
    /// Property: any frame the writer produces is one the
    /// reader accepts without complaint.
    #[test]
    fn frame_round_trips_through_write_then_read() {
        for kind in [FrameKind::OtlpBatch, FrameKind::AuditEvent] {
            for payload in [
                &b""[..],
                &b"x"[..],
                &b"some longer payload bytes for the test"[..],
            ] {
                let mut buf = Vec::new();
                write_frame(&mut buf, kind, payload).expect("write");
                let (read_kind, read_payload) =
                    read_frame(&mut Cursor::new(&buf[..])).expect("read");
                assert_eq!(read_kind, kind, "kind round-trips for {kind:?}");
                assert_eq!(read_payload, payload, "payload round-trips for {kind:?}");
            }
        }
    }

    /// Empty-payload frame: `len = 0`, header-only on disk.
    /// Pinned specifically because a future micro-optimisation
    /// that skips the payload `write_all` on empty would still
    /// need the CRC over an empty buffer to round-trip.
    #[test]
    fn empty_payload_frame_is_header_only() {
        let mut buf = Vec::new();
        write_frame(&mut buf, FrameKind::AuditEvent, &[]).expect("write");
        assert_eq!(
            buf.len(),
            FRAME_HEADER_LEN,
            "empty payload frame is exactly the header",
        );
        let (kind, payload) = read_frame(&mut Cursor::new(&buf[..])).expect("read");
        assert_eq!(kind, FrameKind::AuditEvent);
        assert!(payload.is_empty());
    }

    /// `CrcMismatch` on a corrupted payload byte: write a
    /// valid frame, flip one bit of payload, the read MUST
    /// detect the mismatch (RFC0008.5 sub-case 1).
    #[test]
    fn read_frame_rejects_crc_mismatch() {
        let mut buf = Vec::new();
        write_frame(&mut buf, FrameKind::OtlpBatch, b"payload").expect("write");
        // Corrupt one payload byte (well past the 12 B header).
        buf[FRAME_HEADER_LEN] ^= 0x01;
        match read_frame(&mut Cursor::new(&buf[..])).expect_err("must reject") {
            FrameError::CrcMismatch { stored, computed } => assert_ne!(
                stored, computed,
                "CrcMismatch surfaces stored vs computed CRC",
            ),
            other => panic!("expected CrcMismatch, got {other:?}"),
        }
    }

    /// `UnknownKind` on a reserved `kind` byte (RFC0008.5
    /// sub-case 2). We hand-craft the on-disk bytes — the
    /// writer doesn't expose a way to emit a reserved kind, by
    /// design — so the test directly proves the read path's
    /// rejection.
    #[test]
    fn read_frame_rejects_unknown_kind() {
        let mut buf = Vec::new();
        // len = 0, kind = 0xFE (reserved range), pad = 0, CRC
        // matches empty payload + 0xFE prefix. We have to
        // compute the right CRC so the read fails on
        // `UnknownKind` rather than `CrcMismatch` first.
        buf.extend_from_slice(&0u32.to_le_bytes()); // len
        buf.push(0xFE); // kind
        buf.extend_from_slice(&FRAME_PAD_ZEROS); // pad
        let crc = {
            let mut prefix = [0u8; 4];
            prefix[0] = 0xFE;
            crc32c::crc32c(&prefix)
        };
        buf.extend_from_slice(&crc.to_le_bytes());
        match read_frame(&mut Cursor::new(&buf[..])).expect_err("must reject") {
            FrameError::UnknownKind { found } => assert_eq!(found, 0xFE),
            other => panic!("expected UnknownKind, got {other:?}"),
        }
    }

    /// `NonZeroPad` on a non-zero `_pad` (RFC0008.5 sub-case
    /// 3). Same hand-crafted approach — the writer can't emit
    /// this from safe Rust.
    #[test]
    fn read_frame_rejects_non_zero_pad() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes()); // len
        buf.push(0x01); // kind = OtlpBatch
        buf.extend_from_slice(&[0, 0xAA, 0]); // pad — middle byte non-zero
        buf.extend_from_slice(&0u32.to_le_bytes()); // arbitrary CRC; pad check fires first
        match read_frame(&mut Cursor::new(&buf[..])).expect_err("must reject") {
            FrameError::NonZeroPad { found } => assert_eq!(found, [0, 0xAA, 0]),
            other => panic!("expected NonZeroPad, got {other:?}"),
        }
    }

    /// `OversizeLen` on a `len` field claiming a payload
    /// larger than [`MAX_FRAME_BYTES`] (RFC0008.5 sub-case 4).
    /// The check fires *before* the payload read so a
    /// corrupt-but-huge `len` doesn't trigger a multi-GiB
    /// allocation.
    #[test]
    fn read_frame_rejects_oversize_len() {
        let mut buf = Vec::new();
        let oversize_len =
            u32::try_from(MAX_FRAME_BYTES + 1).expect("MAX_FRAME_BYTES + 1 fits u32");
        buf.extend_from_slice(&oversize_len.to_le_bytes()); // len > cap
        buf.push(0x01);
        buf.extend_from_slice(&FRAME_PAD_ZEROS);
        buf.extend_from_slice(&0u32.to_le_bytes());
        // Critically: do NOT append a payload. The read MUST
        // refuse on `len` alone, not after attempting to read
        // 16 MiB + 1 of bytes that aren't there.
        match read_frame(&mut Cursor::new(&buf[..])).expect_err("must reject") {
            FrameError::OversizeLen { found } => assert_eq!(found, oversize_len),
            other => panic!("expected OversizeLen, got {other:?}"),
        }
    }

    /// Truncated header surfaces as `FrameError::Io`. The
    /// recovery driver disambiguates RFC0008.4 (torn last
    /// frame, expected on the newest segment) from RFC0008.5
    /// (torn-on-closed, corruption on any other segment) by
    /// segment position, not by error variant.
    #[test]
    fn read_frame_rejects_truncated_header() {
        let buf = [0u8; 4]; // only 4 of 12 header bytes
        match read_frame(&mut Cursor::new(&buf[..])).expect_err("must reject") {
            FrameError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
            other => panic!("expected Io(UnexpectedEof), got {other:?}"),
        }
    }

    /// Truncated payload (header complete, payload short) also
    /// surfaces as `FrameError::Io`. Same RFC0008.4 / .5
    /// disambiguation rule as above.
    #[test]
    fn read_frame_rejects_truncated_payload() {
        let mut buf = Vec::new();
        write_frame(&mut buf, FrameKind::OtlpBatch, b"complete-payload").expect("write");
        // Drop the last 4 payload bytes — header still claims
        // `len = 16`, but only 12 payload bytes are available.
        buf.truncate(buf.len() - 4);
        match read_frame(&mut Cursor::new(&buf[..])).expect_err("must reject") {
            FrameError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
            other => panic!("expected Io(UnexpectedEof), got {other:?}"),
        }
    }
}
