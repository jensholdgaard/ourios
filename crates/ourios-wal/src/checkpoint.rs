//! `CHECKPOINT` sidecar codec + atomic persistence (RFC 0008
//! §6.7).
//!
//! The sidecar starts with a fixed 32 B header: 4 B magic `b"OWCK"`,
//! 2 B version, 2 B flags (reserved, zero), 16 B segment `UUIDv7`, 8 B
//! little-endian byte-in-segment, matching the `(segment, byte)`
//! [`WalOffset`] pair. Versions 1 and 2 are the header alone. Version 3
//! (RFC 0056 §3.4, written since) follows it with the held floors: a
//! `u32` count, then per tenant a `u16` length, the tenant id's bytes, a
//! tag (`0` start of log, `1` an offset) and, for an offset, its 24 B;
//! then a CRC32-C over everything before it. Storing
//! the segment UUID rather than a synthetic global counter is
//! what keeps the checkpoint meaningful after housekeeping
//! deletes older segments.
//!
//! A present-but-invalid sidecar is a **structured corruption
//! error**, never silently `None` (§6.6 step 1): treating it
//! as absent would drop the Parquet suppression horizon and
//! re-feed every already-published record to the data side.

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;

use crate::{CheckpointError, OpenError, WalOffset, sync_parent_dir};

pub(crate) const SIDECAR_NAME: &str = "CHECKPOINT";
const TMP_NAME: &str = "CHECKPOINT.tmp";
pub(crate) const SIDECAR_LEN: usize = 32;
const MAGIC: [u8; 4] = *b"OWCK";
const VERSION: u16 = 3;
/// RFC 0052's version: the header alone, read as carrying no floors.
const VERSION_RECLAIM: u16 = 2;
const VERSION_LEGACY: u16 = 1;
/// The largest sidecar read: a floor per tenant stays far below it.
const MAX_SIDECAR_LEN: usize = 1 << 20;
const FLOOR_START_OF_LOG: u8 = 0;
const FLOOR_AT: u8 = 1;

/// Which format version a `CHECKPOINT` on disk carries. RFC 0052 §3.2
/// reads it as a witness, not as a decoding detail: a version-1 sidecar
/// beside a missing `RECLAIM` is a legitimate pre-RFC root, while a
/// version-2 one beside a missing record is a root that lost its
/// record. Pre-production, accepting both on read is the whole
/// migration (§3.8) — there is no dual-write and no tooling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidecarVersion {
    /// Written before RFC 0052. Nothing writes it now; the next
    /// checkpoint rewrites the sidecar at [`SidecarVersion::Current`].
    Legacy,
    /// RFC 0052's version 2. For §3.2's witness it is a post-RFC root, as
    /// [`SidecarVersion::Current`] is; it is still not settled, so the next
    /// checkpoint rewrites it at version 3, unchanged mark or not.
    Reclaim,
    Current,
}

/// Whether a sidecar at `version` lets housekeeping reclaim under it. A
/// witnessed version-2 checkpoint is a complete RFC 0052 one, so
/// reclamation does not wait for RFC 0056's version 3: a full disk, which
/// fails that write, would otherwise block the reclamation that frees
/// space.
pub(crate) fn reclaims_under(version: Option<SidecarVersion>) -> bool {
    matches!(
        version,
        Some(SidecarVersion::Reclaim | SidecarVersion::Current)
    )
}

/// Whether re-asserting an unchanged mark may skip the write: only a
/// `settled` sidecar already at version 3. RFC 0052's upgrade one step on,
/// so an idle node's version-2 sidecar still reaches version 3.
pub(crate) fn skips_rewrite(version: Option<SidecarVersion>, settled: bool) -> bool {
    settled && version == Some(SidecarVersion::Current)
}

/// Where a held tenant's records are durable through (RFC 0056 §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldFloor {
    /// Held before any checkpoint or snapshot: none of its records count
    /// as durable, so all of its frames replay.
    StartOfLog,
    /// Durable through this offset.
    At(WalOffset),
}

/// The held floors a `CHECKPOINT` carries, by tenant.
pub type HeldFloors = std::collections::BTreeMap<ourios_core::tenant::TenantId, HeldFloor>;

/// A `CHECKPOINT` that decoded: the mark, the held floors, and the
/// version it was written at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sidecar {
    pub(crate) offset: WalOffset,
    pub(crate) floors: HeldFloors,
    pub(crate) version: SidecarVersion,
}

/// The version-3 sidecar for `offset` and `floors`.
///
/// # Errors
///
/// A description of why the floors do not encode.
pub(crate) fn encode_with(offset: WalOffset, floors: &HeldFloors) -> Result<Vec<u8>, String> {
    let mut out = header(offset, VERSION).to_vec();
    let count = u32::try_from(floors.len())
        .map_err(|_| format!("{} held floors do not fit the table", floors.len()))?;
    out.extend_from_slice(&count.to_le_bytes());
    for (tenant, floor) in floors {
        let id = tenant.as_str().as_bytes();
        let len = u16::try_from(id.len())
            .map_err(|_| format!("a {}-byte tenant id does not fit the table", id.len()))?;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(id);
        match floor {
            HeldFloor::StartOfLog => out.push(FLOOR_START_OF_LOG),
            HeldFloor::At(at) => {
                out.push(FLOOR_AT);
                out.extend_from_slice(at.segment.as_bytes());
                out.extend_from_slice(&at.byte.to_le_bytes());
            }
        }
    }
    let crc = crc32c::crc32c(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    if out.len() > MAX_SIDECAR_LEN {
        return Err(format!(
            "{} B of held floors exceed the {MAX_SIDECAR_LEN} B sidecar",
            out.len()
        ));
    }
    Ok(out)
}

/// The 32 B header alone, as versions 1 and 2 are written.
#[cfg(test)]
pub(crate) fn encode(offset: WalOffset) -> [u8; SIDECAR_LEN] {
    header(offset, VERSION_RECLAIM)
}

fn header(offset: WalOffset, version: u16) -> [u8; SIDECAR_LEN] {
    let mut out = [0u8; SIDECAR_LEN];
    out[0..4].copy_from_slice(&MAGIC);
    out[4..6].copy_from_slice(&version.to_le_bytes());
    // [6..8] flags: reserved, zero.
    out[8..24].copy_from_slice(offset.segment.as_bytes());
    out[24..32].copy_from_slice(&offset.byte.to_le_bytes());
    out
}

fn decode(bytes: &[u8]) -> Result<Sidecar, String> {
    let head = bytes
        .get(..SIDECAR_LEN)
        .ok_or_else(|| format!("size {} B, expected at least {SIDECAR_LEN} B", bytes.len()))?;
    if head[0..4] != MAGIC {
        return Err(format!(
            "bad magic {:02x?}, expected {MAGIC:02x?}",
            &head[0..4]
        ));
    }
    let found = u16::from_le_bytes([head[4], head[5]]);
    let flags = u16::from_le_bytes([head[6], head[7]]);
    if flags != 0 {
        return Err(format!("non-zero flags {flags:#06x}"));
    }
    let offset = offset_at(&head[8..32]);
    let (version, floors) = match found {
        VERSION_LEGACY if bytes.len() == SIDECAR_LEN => (SidecarVersion::Legacy, HeldFloors::new()),
        VERSION_RECLAIM if bytes.len() == SIDECAR_LEN => {
            (SidecarVersion::Reclaim, HeldFloors::new())
        }
        VERSION_LEGACY | VERSION_RECLAIM => {
            return Err(format!("size {} B, expected {SIDECAR_LEN} B", bytes.len()));
        }
        // Version 3 counts as version 2 for RFC 0052 §3.2's witness.
        VERSION => (SidecarVersion::Current, floors_of(bytes)?),
        found => {
            return Err(format!(
                "unknown version {found}, expected {VERSION_LEGACY} to {VERSION}"
            ));
        }
    };
    Ok(Sidecar {
        offset,
        floors,
        version,
    })
}

fn offset_at(bytes: &[u8]) -> WalOffset {
    let mut segment = [0u8; 16];
    segment.copy_from_slice(&bytes[..16]);
    let mut byte = [0u8; 8];
    byte.copy_from_slice(&bytes[16..24]);
    WalOffset {
        segment: uuid::Uuid::from_bytes(segment),
        byte: u64::from_le_bytes(byte),
    }
}

/// A version-3 sidecar's floor table, its CRC checked first.
fn floors_of(bytes: &[u8]) -> Result<HeldFloors, String> {
    let body_end = bytes
        .len()
        .checked_sub(4)
        .filter(|&end| end >= SIDECAR_LEN + 4)
        .ok_or_else(|| format!("size {} B, too short for a floor table", bytes.len()))?;
    let (body, crc) = bytes.split_at(body_end);
    let stored = u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]]);
    if crc32c::crc32c(body) != stored {
        return Err("floor table CRC mismatch".to_owned());
    }
    let mut table = Table(&body[SIDECAR_LEN..]);
    let count = u32::from_le_bytes(table.take()?);
    let mut floors = HeldFloors::new();
    for _ in 0..count {
        let len = u16::from_le_bytes(table.take()?);
        let id = std::str::from_utf8(table.bytes(usize::from(len))?)
            .map_err(|_| "a tenant id is not UTF-8".to_owned())?;
        let floor = match table.take::<1>()? {
            [FLOOR_START_OF_LOG] => HeldFloor::StartOfLog,
            [FLOOR_AT] => HeldFloor::At(offset_at(table.bytes(24)?)),
            [tag] => return Err(format!("unknown floor tag {tag}")),
        };
        if floors
            .insert(ourios_core::tenant::TenantId::new(id), floor)
            .is_some()
        {
            return Err(format!("tenant {id:?} appears twice in the floor table"));
        }
    }
    if !table.0.is_empty() {
        return Err(format!("{} B after the floor table", table.0.len()));
    }
    Ok(floors)
}

/// The unread rest of a floor table.
struct Table<'a>(&'a [u8]);

impl<'a> Table<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.0.len() < n {
            return Err("the floor table ends early".to_owned());
        }
        let (taken, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(taken)
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.bytes(N)?);
        Ok(out)
    }
}

/// Read `<root>/CHECKPOINT`. Absent → `Ok(None)` (first-run /
/// pre-checkpoint); present and valid → `Ok(Some(offset))`;
/// present but invalid → [`OpenError::Corrupt`] (§6.6 step 1 —
/// the operator restores or removes the sidecar knowingly;
/// removal is an explicit acceptance of at-least-once
/// re-publish to Parquet).
pub(crate) fn read(root: &Path) -> Result<Option<Sidecar>, OpenError> {
    let path = root.join(SIDECAR_NAME);
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(OpenError::Io {
                op: "open(CHECKPOINT)",
                source,
            });
        }
    };
    // Read one byte past the largest size so an oversized sidecar
    // fails the length check instead of silently truncating.
    let mut bytes = Vec::with_capacity(SIDECAR_LEN + 1);
    Read::take(&mut file, MAX_SIDECAR_LEN as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| OpenError::Io {
            op: "read(CHECKPOINT)",
            source,
        })?;
    if bytes.len() > MAX_SIDECAR_LEN {
        return Err(OpenError::Corrupt {
            detail: format!(
                "CHECKPOINT sidecar at {}: larger than {MAX_SIDECAR_LEN} B",
                path.display()
            ),
        });
    }
    match decode(&bytes) {
        Ok(sidecar) => Ok(Some(sidecar)),
        Err(detail) => Err(OpenError::Corrupt {
            detail: format!("CHECKPOINT sidecar at {}: {detail}", path.display()),
        }),
    }
}

/// Persist `offset` to `<root>/CHECKPOINT` per §6.7 up to and
/// including the rename: write to `CHECKPOINT.tmp`, `fsync`, `rename`.
/// Durability is required, not advisory — a crash after
/// `checkpoint(X)` but before housekeeping must still find `X` on
/// restart, or the driver's Parquet-side suppression loses its
/// horizon.
///
/// The parent-directory fsync is [`sync_root`] and **not** part of
/// this call, because the two failure modes are not the same one. A
/// failure here leaves nothing visible under the final name, so the
/// caller's mark must not advance; a failure of the fsync leaves the
/// new mark already visible, so the caller's mark must. Folding them
/// together forced one answer on both, and the wrong one let a later,
/// lower mark rewrite the sidecar backwards.
pub(crate) fn write(
    root: &Path,
    offset: WalOffset,
    floors: &HeldFloors,
) -> Result<(), CheckpointError> {
    let io = |op: &'static str, source| CheckpointError::Io { op, source };
    let tmp = root.join(TMP_NAME);
    let mut file = File::create(&tmp).map_err(|e| io("create(CHECKPOINT.tmp)", e))?;
    let bytes =
        encode_with(offset, floors).map_err(|detail| CheckpointError::Unencodable { detail })?;
    file.write_all(&bytes)
        .map_err(|e| io("write(CHECKPOINT.tmp)", e))?;
    file.sync_all()
        .map_err(|e| io("fsync(CHECKPOINT.tmp)", e))?;
    std::fs::rename(&tmp, root.join(SIDECAR_NAME))
        .map_err(|e| io("rename(CHECKPOINT.tmp -> CHECKPOINT)", e))
}

/// Make the renamed `CHECKPOINT`'s directory entry durable. Called
/// straight after [`write`], once the caller has taken the new mark.
pub(crate) fn sync_root(root: &Path) -> Result<(), CheckpointError> {
    sync_parent_dir(root).map_err(|source| CheckpointError::Io {
        op: "fsync(wal_root after checkpoint)",
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offset() -> WalOffset {
        WalOffset {
            segment: uuid::Uuid::now_v7(),
            byte: 0x0123_4567_89ab_cdef,
        }
    }

    #[test]
    #[ignore = "superseded: RFC 0056 writes version 3, and a version-2 sidecar now decodes as \
                `Reclaim`; a_version_3_sidecar_round_trips_its_held_floors covers the round trip"]
    fn encode_decode_round_trips() {
        let original = offset();
        let bytes = encode(original);
        assert_eq!(bytes.len(), SIDECAR_LEN);
        let decoded = decode(&bytes).expect("decode");
        assert_eq!(decoded.offset, original);
        assert_eq!(decoded.version, SidecarVersion::Current);
    }

    /// A sidecar written before RFC 0052 still decodes, and names the
    /// version that makes it §3.2's migration witness. Refusing it
    /// would refuse to start every pre-RFC root that ever checkpointed.
    #[test]
    fn decode_accepts_the_legacy_version_and_names_it() {
        let original = offset();
        let mut bytes = encode(original);
        bytes[4..6].copy_from_slice(&VERSION_LEGACY.to_le_bytes());
        let decoded = decode(&bytes).expect("legacy sidecar");
        assert_eq!(decoded.offset, original);
        assert_eq!(decoded.version, SidecarVersion::Legacy);
    }

    #[test]
    fn encode_layout_is_the_pinned_32_bytes() {
        let original = offset();
        let bytes = encode(original);
        assert_eq!(&bytes[0..4], b"OWCK");
        // RFC 0052 §3.8's `CHECKPOINT` Invariant row bumps this to 2.
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 2);
        assert_eq!(u16::from_le_bytes([bytes[6], bytes[7]]), 0);
        assert_eq!(&bytes[8..24], original.segment.as_bytes());
        assert_eq!(
            u64::from_le_bytes(bytes[24..32].try_into().expect("8 bytes")),
            original.byte,
        );
    }

    #[test]
    #[ignore = "superseded: RFC 0056 makes version 3 valid, so it is no longer an unknown \
                version; decode_rejects_each_invalid_field_after_version_3 covers this"]
    fn decode_rejects_each_invalid_field() {
        // 3: version 2 is this build's own and version 1 is the
        // accepted legacy, so the arm needs a genuinely unknown one.
        rejects_each_invalid_field(3);
    }

    #[test]
    fn decode_rejects_each_invalid_field_after_version_3() {
        // 4: versions 1 to 3 all read (RFC 0056 §3.4), so the arm
        // needs a genuinely unknown one.
        rejects_each_invalid_field(4);
    }

    fn rejects_each_invalid_field(unknown_version: u8) {
        let valid = encode(offset());
        let mut bad_magic = valid;
        bad_magic[0] = b'X';
        assert!(decode(&bad_magic).expect_err("magic").contains("bad magic"));
        let mut bad_version = valid;
        bad_version[4] = unknown_version;
        assert!(
            decode(&bad_version)
                .expect_err("version")
                .contains("unknown version"),
        );
        let mut bad_flags = valid;
        bad_flags[6] = 1;
        assert!(
            decode(&bad_flags)
                .expect_err("flags")
                .contains("non-zero flags"),
        );
        assert!(decode(&valid[..31]).expect_err("short").contains("size"));
        let mut long = valid.to_vec();
        long.push(0);
        assert!(decode(&long).expect_err("long").contains("size"));
    }

    fn floors() -> HeldFloors {
        use ourios_core::tenant::TenantId;
        HeldFloors::from([
            (TenantId::new("acme"), HeldFloor::At(offset())),
            (TenantId::new("globex"), HeldFloor::StartOfLog),
        ])
    }

    /// RFC0056.8: version 3 carries the held floors, the tenant at the
    /// start of the log among them, and round-trips them.
    #[test]
    fn a_version_3_sidecar_round_trips_its_held_floors() {
        let (original, held) = (offset(), floors());
        let bytes = encode_with(original, &held).expect("encode");
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 3, "written at v3");
        let decoded = decode(&bytes).expect("decode");
        assert_eq!(decoded.offset, original);
        assert_eq!(decoded.floors, held);
        assert_eq!(decoded.version, SidecarVersion::Current);
    }

    /// RFC0056.8: a version-2 sidecar reads as carrying no floors, and
    /// counts as current for RFC 0052 §3.2's witness.
    #[test]
    fn a_version_2_sidecar_reads_as_floorless() {
        let original = offset();
        let mut bytes = encode(original);
        bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
        let decoded = decode(&bytes).expect("v2 sidecar");
        assert_eq!(decoded.offset, original);
        assert!(decoded.floors.is_empty());
        assert_eq!(decoded.version, SidecarVersion::Reclaim);
    }

    /// A version-3 sidecar whose floor table is damaged is corruption,
    /// never a sidecar with fewer floors.
    #[test]
    fn a_damaged_version_3_floor_table_is_rejected() {
        let bytes = encode_with(offset(), &floors()).expect("encode");
        let mut flipped = bytes.clone();
        let last = flipped.len() - 5;
        flipped[last] ^= 0x01;
        assert!(decode(&flipped).is_err(), "a flipped table byte");
        assert!(
            decode(&bytes[..bytes.len() - 1]).is_err(),
            "a truncated table"
        );
    }

    /// An empty floor table is still version 3.
    #[test]
    fn a_version_3_sidecar_without_floors_round_trips() {
        let original = offset();
        let bytes = encode_with(original, &HeldFloors::new()).expect("encode");
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 3, "written at v3");
        let decoded = decode(&bytes).expect("decode");
        assert!(decoded.floors.is_empty());
    }

    /// A tenant id longer than the table's `u16` length is refused, not
    /// truncated into another tenant's id.
    #[test]
    fn a_tenant_id_too_long_for_the_table_is_refused() {
        use ourios_core::tenant::TenantId;
        let long = TenantId::new("a".repeat(usize::from(u16::MAX) + 1));
        let floors = HeldFloors::from([(long, HeldFloor::StartOfLog)]);
        assert!(encode_with(offset(), &floors).is_err());
    }

    /// A table naming one tenant twice is corruption: the second entry
    /// must not silently replace the first.
    #[test]
    fn a_tenant_repeated_in_the_floor_table_is_corrupt() {
        use ourios_core::tenant::TenantId;
        let one = HeldFloors::from([(TenantId::new("acme"), HeldFloor::StartOfLog)]);
        let mut bytes = encode_with(offset(), &one).expect("encode");
        bytes.truncate(bytes.len() - 4);
        // count 1 -> 2, then the same entry again: len 4, "acme", tag 0.
        bytes[SIDECAR_LEN..SIDECAR_LEN + 4].copy_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(b"acme");
        bytes.push(FLOOR_START_OF_LOG);
        let crc = crc32c::crc32c(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        assert!(decode(&bytes).is_err(), "a repeated tenant id");
    }

    /// A version-3 sidecar whose floor table is `table`, with a valid CRC,
    /// so the table parser itself is what rejects it.
    fn sealed_table(table: &[u8]) -> Vec<u8> {
        let mut bytes = header(offset(), VERSION).to_vec();
        bytes.extend_from_slice(table);
        let crc = crc32c::crc32c(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        bytes
    }

    /// One entry for `acme` followed by `tag`.
    fn one_entry_tagged(tag: u8) -> Vec<u8> {
        let mut table = 1u32.to_le_bytes().to_vec();
        table.extend_from_slice(&4u16.to_le_bytes());
        table.extend_from_slice(b"acme");
        table.push(tag);
        table
    }

    #[test]
    fn a_floor_with_an_unknown_tag_is_corrupt() {
        let error = decode(&sealed_table(&one_entry_tagged(2))).expect_err("unknown tag");
        assert!(error.contains("unknown floor tag"), "{error}");
    }

    #[test]
    fn a_floor_table_that_ends_inside_an_entry_is_corrupt() {
        // Tag 1 promises a 24-byte offset that is not there.
        let error = decode(&sealed_table(&one_entry_tagged(FLOOR_AT))).expect_err("cut short");
        assert!(error.contains("ends early"), "{error}");
    }

    #[test]
    fn bytes_after_the_floor_table_are_corrupt() {
        let mut table = one_entry_tagged(FLOOR_START_OF_LOG);
        table.push(0);
        let error = decode(&sealed_table(&table)).expect_err("trailing byte");
        assert!(error.contains("after the floor table"), "{error}");
    }

    /// The writer refuses a sidecar larger than the reader accepts, so a
    /// successful checkpoint never leaves a file its own reader refuses.
    #[test]
    fn a_sidecar_larger_than_the_reader_accepts_is_refused() {
        use ourios_core::tenant::TenantId;
        let floors: HeldFloors = (0..9_000)
            .map(|n| {
                let id = TenantId::new(format!("{n:0>120}"));
                (id, HeldFloor::At(offset()))
            })
            .collect();
        assert!(encode_with(offset(), &floors).is_err());
    }

    #[test]
    fn read_absent_is_none_invalid_is_corrupt() {
        let tmp = tempfile::TempDir::new().expect("temp");
        assert!(read(tmp.path()).expect("absent").is_none());
        let original = offset();
        write(tmp.path(), original, &HeldFloors::new()).expect("write");
        sync_root(tmp.path()).expect("fsync the root");
        assert_eq!(
            read(tmp.path()).expect("present"),
            Some(Sidecar {
                offset: original,
                floors: HeldFloors::new(),
                version: SidecarVersion::Current,
            }),
        );
        std::fs::write(tmp.path().join(SIDECAR_NAME), b"garbage").expect("poison");
        match read(tmp.path()) {
            Err(OpenError::Corrupt { detail }) => {
                assert!(detail.contains("CHECKPOINT sidecar"), "detail: {detail}");
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }
}
