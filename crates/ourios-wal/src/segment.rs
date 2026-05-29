//! Segment-on-disk format per RFC 0008 §6.2.
//!
//! A segment is the unit of WAL rotation: append-only, named
//! `<UUIDv7>.wal` under `wal_root` (the angle-bracketed name
//! placeholder; the `UUIDv7` term is detailed in §6.2.1),
//! opened by exactly one
//! writer at a time. The first 24 bytes of every segment are
//! the **header** defined by §6.2.1; everything after is a
//! sequence of frames (§6.2.2 — that format lands with the
//! `append` implementation in a follow-up slice).
//!
//! This module is the boundary between "bytes on disk" and
//! "Rust values"; the public-API `Wal` (`lib.rs`) composes
//! against it without needing to know the byte layout.

use std::io::{Read, Write};

use uuid::Uuid;

/// 4 B magic prefix that opens every segment file. Lets the
/// recovery scanner reject foreign files (e.g. a sibling
/// `*.lock`) without consulting the filename. ASCII `"OWAL"`.
pub(crate) const SEGMENT_MAGIC: [u8; 4] = *b"OWAL";

/// Format version of [`SegmentHeader`]. A future migration
/// bumps this and the reader either decodes or rejects per the
/// RFC 0005 §3.5 schema-evolution rules (this file format is
/// independent of the Parquet schema but follows the same
/// "forward-compat readers, explicit migration" discipline).
pub(crate) const SEGMENT_VERSION: u16 = 1;

/// Reserved flags field; MUST be zero today. A future read MAY
/// reject non-zero flags as RFC0008.5 corruption rather than
/// silently ignoring them — that's the explicit `_pad`-style
/// "reserved means reserved" rule §6.2.2 applies to frames.
pub(crate) const SEGMENT_FLAGS_RESERVED: u16 = 0;

/// Exact on-disk length of a [`SegmentHeader`] per §6.2.1
/// (`4 + 2 + 2 + 16 = 24`). Implementations index into the
/// segment file past this offset to find the first frame.
pub(crate) const SEGMENT_HEADER_LEN: usize = 24;

/// In-memory view of the 24 B header per RFC 0008 §6.2.1.
///
/// `segment_uuid` is the same `UUIDv7` that appears in the
/// filename; carrying it inside the file too means a `mv`
/// that mangles the name still leaves the file readable (the
/// reader can cross-check name vs header in a future slice).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SegmentHeader {
    pub(crate) version: u16,
    pub(crate) flags: u16,
    pub(crate) segment_uuid: Uuid,
}

/// Errors returned by [`read_header`]. Distinct variants so
/// the RFC0008.5 corruption tests can match on the specific
/// reason (`BadMagic` vs `UnknownVersion`) without
/// string-matching an error message.
#[derive(Debug)]
pub(crate) enum HeaderError {
    /// The first 4 bytes weren't [`SEGMENT_MAGIC`]. Treat as
    /// "not an ourios WAL segment" — could be a stray file
    /// the operator left behind.
    BadMagic { found: [u8; 4] },
    /// The version field is something this build doesn't
    /// understand. Per RFC 0008 §6.2.1 a future migration
    /// bumps the version; older readers refuse the file
    /// rather than guess.
    UnknownVersion { found: u16 },
    /// I/O failure (EOF inside the header, read error).
    /// EOF on a fresh segment file would be RFC0008.5
    /// `TornOnClosedSegment` corruption; on a brand-new
    /// file the writer is supposed to have flushed the
    /// header before any other code reads it.
    Io(std::io::Error),
}

impl std::fmt::Display for HeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic { found } => write!(
                f,
                "segment magic mismatch: expected {SEGMENT_MAGIC:?}, found {found:?}",
            ),
            Self::UnknownVersion { found } => write!(
                f,
                "unknown segment version {found} (this build supports v{SEGMENT_VERSION})",
            ),
            Self::Io(e) => write!(f, "segment header read: {e}"),
        }
    }
}

impl std::error::Error for HeaderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl SegmentHeader {
    /// Build a fresh header for a newly-created segment. The
    /// caller mints the `UUIDv7` once and uses the same value
    /// for both the filename and the header.
    pub(crate) fn new(segment_uuid: Uuid) -> Self {
        Self {
            version: SEGMENT_VERSION,
            flags: SEGMENT_FLAGS_RESERVED,
            segment_uuid,
        }
    }
}

/// Serialise a `SegmentHeader` to its exact 24-byte on-disk
/// form per §6.2.1: `magic (4) || version_le (2) ||
/// flags_le (2) || uuid (16, big-endian per RFC 4122)`. The
/// UUID bytes follow `Uuid::as_bytes`'s order — same as
/// every other tool that reads UUIDs (no spec-incompatible
/// reordering).
pub(crate) fn write_header<W: Write>(w: &mut W, header: &SegmentHeader) -> std::io::Result<()> {
    let mut buf = [0u8; SEGMENT_HEADER_LEN];
    buf[0..4].copy_from_slice(&SEGMENT_MAGIC);
    buf[4..6].copy_from_slice(&header.version.to_le_bytes());
    buf[6..8].copy_from_slice(&header.flags.to_le_bytes());
    buf[8..24].copy_from_slice(header.segment_uuid.as_bytes());
    w.write_all(&buf)
}

/// Read + validate a header from `r`. Returns the parsed
/// header on success; one of the four [`HeaderError`] arms
/// on failure (the corruption arms feed RFC0008.5 audit
/// events from the recovery driver).
pub(crate) fn read_header<R: Read>(r: &mut R) -> Result<SegmentHeader, HeaderError> {
    let mut buf = [0u8; SEGMENT_HEADER_LEN];
    r.read_exact(&mut buf).map_err(HeaderError::Io)?;
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&buf[0..4]);
    if magic != SEGMENT_MAGIC {
        return Err(HeaderError::BadMagic { found: magic });
    }
    let version = u16::from_le_bytes([buf[4], buf[5]]);
    if version != SEGMENT_VERSION {
        return Err(HeaderError::UnknownVersion { found: version });
    }
    let flags = u16::from_le_bytes([buf[6], buf[7]]);
    let mut uuid_bytes = [0u8; 16];
    uuid_bytes.copy_from_slice(&buf[8..24]);
    Ok(SegmentHeader {
        version,
        flags,
        segment_uuid: Uuid::from_bytes(uuid_bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Pin the exact 24 bytes a fresh header serialises to —
    /// catches a regression that reorders fields, swaps
    /// endianness, or changes the magic. The §6.2.1 byte
    /// layout is normative; the test mirrors it character for
    /// character.
    #[test]
    fn header_byte_layout_matches_rfc_6_2_1() {
        // A fixed `UUIDv7` lets us pin the exact 16-byte tail
        // (the v7 timestamp bits are at the front).
        let uuid =
            Uuid::parse_str("01890c43-7b3d-7c01-9e00-0123456789ab").expect("parse fixed `UUIDv7`");
        let header = SegmentHeader::new(uuid);
        let mut buf = Vec::new();
        write_header(&mut buf, &header).expect("write");
        assert_eq!(buf.len(), 24, "header is exactly 24 bytes per §6.2.1");
        assert_eq!(&buf[0..4], b"OWAL", "magic prefix");
        assert_eq!(&buf[4..6], &[0x01, 0x00], "version = 1 (LE u16)");
        assert_eq!(&buf[6..8], &[0x00, 0x00], "flags = 0 (LE u16, reserved)");
        assert_eq!(&buf[8..24], uuid.as_bytes(), "UUID bytes in RFC 4122 order");
    }

    /// Round-trip: write the header → read it back → identical.
    /// Property: any segment file the writer produces is one
    /// the reader accepts without complaint.
    #[test]
    fn header_round_trips_through_write_then_read() {
        let uuid = Uuid::now_v7();
        let original = SegmentHeader::new(uuid);
        let mut buf = Vec::new();
        write_header(&mut buf, &original).expect("write");
        let parsed = read_header(&mut Cursor::new(&buf[..])).expect("read");
        assert_eq!(parsed, original);
    }

    /// `BadMagic` on a foreign file: the first four bytes
    /// aren't `b"OWAL"`. Recovery treats this as "not a
    /// segment file" rather than corruption (the file could
    /// be `*.lock` or operator-placed). Specific variant so
    /// the matcher in tests/`corruption.rs` can be exact.
    #[test]
    fn read_header_rejects_foreign_magic() {
        let mut bytes = [0u8; SEGMENT_HEADER_LEN];
        bytes[..4].copy_from_slice(b"NOPE");
        // Fill the rest with valid-looking version + flags +
        // uuid to prove the magic check fires *first*.
        bytes[4..6].copy_from_slice(&SEGMENT_VERSION.to_le_bytes());
        let err = read_header(&mut Cursor::new(&bytes[..])).expect_err("must reject");
        match err {
            HeaderError::BadMagic { found } => assert_eq!(&found, b"NOPE"),
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    /// `UnknownVersion` on a future-format file: the magic
    /// matches but the version field is something this build
    /// can't decode. Refuse cleanly per §6.2.1's "explicit
    /// migration" discipline — older readers don't guess.
    #[test]
    fn read_header_rejects_unknown_version() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&SEGMENT_MAGIC);
        buf.extend_from_slice(&99_u16.to_le_bytes()); // future version
        buf.extend_from_slice(&0_u16.to_le_bytes()); // flags
        buf.extend_from_slice(Uuid::nil().as_bytes()); // UUID
        let err = read_header(&mut Cursor::new(&buf[..])).expect_err("must reject");
        match err {
            HeaderError::UnknownVersion { found } => assert_eq!(found, 99),
            other => panic!("expected UnknownVersion, got {other:?}"),
        }
    }

    /// EOF inside the 24 B header surfaces as `HeaderError::Io`.
    /// A producer that crashed during the first-segment-header
    /// write is the realistic path (the receiver hasn't started
    /// taking traffic yet). The reader doesn't pretend the
    /// truncated bytes form a valid header.
    #[test]
    fn read_header_rejects_truncated_input() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&SEGMENT_MAGIC); // only 4 bytes; need 24
        let err = read_header(&mut Cursor::new(&buf[..])).expect_err("must reject");
        match err {
            HeaderError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
            other => panic!("expected Io(UnexpectedEof), got {other:?}"),
        }
    }
}
