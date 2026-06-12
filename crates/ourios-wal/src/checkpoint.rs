//! `CHECKPOINT` sidecar codec + atomic persistence (RFC 0008
//! §6.7).
//!
//! The sidecar is a fixed 32 B record: 4 B magic `b"OWCK"`,
//! 2 B version (`= 1`), 2 B flags (reserved, zero), 16 B
//! segment `UUIDv7`, 8 B little-endian byte-in-segment —
//! matching the `(segment, byte)` [`WalOffset`] pair. Storing
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
const VERSION: u16 = 1;

pub(crate) fn encode(offset: WalOffset) -> [u8; SIDECAR_LEN] {
    let mut out = [0u8; SIDECAR_LEN];
    out[0..4].copy_from_slice(&MAGIC);
    out[4..6].copy_from_slice(&VERSION.to_le_bytes());
    // [6..8] flags: reserved, zero.
    out[8..24].copy_from_slice(offset.segment.as_bytes());
    out[24..32].copy_from_slice(&offset.byte.to_le_bytes());
    out
}

fn decode(bytes: &[u8]) -> Result<WalOffset, String> {
    if bytes.len() != SIDECAR_LEN {
        return Err(format!("size {} B, expected {SIDECAR_LEN} B", bytes.len()));
    }
    if bytes[0..4] != MAGIC {
        return Err(format!(
            "bad magic {:02x?}, expected {MAGIC:02x?}",
            &bytes[0..4]
        ));
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != VERSION {
        return Err(format!("unknown version {version}, expected {VERSION}"));
    }
    let flags = u16::from_le_bytes([bytes[6], bytes[7]]);
    if flags != 0 {
        return Err(format!("non-zero flags {flags:#06x}"));
    }
    let mut segment = [0u8; 16];
    segment.copy_from_slice(&bytes[8..24]);
    let mut byte = [0u8; 8];
    byte.copy_from_slice(&bytes[24..32]);
    Ok(WalOffset {
        segment: uuid::Uuid::from_bytes(segment),
        byte: u64::from_le_bytes(byte),
    })
}

/// Read `<root>/CHECKPOINT`. Absent → `Ok(None)` (first-run /
/// pre-checkpoint); present and valid → `Ok(Some(offset))`;
/// present but invalid → [`OpenError::Corrupt`] (§6.6 step 1 —
/// the operator restores or removes the sidecar knowingly;
/// removal is an explicit acceptance of at-least-once
/// re-publish to Parquet).
pub(crate) fn read(root: &Path) -> Result<Option<WalOffset>, OpenError> {
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
    // Read one byte past the fixed size so an oversized sidecar
    // fails the length check instead of silently truncating.
    let mut bytes = Vec::with_capacity(SIDECAR_LEN + 1);
    Read::take(&mut file, SIDECAR_LEN as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| OpenError::Io {
            op: "read(CHECKPOINT)",
            source,
        })?;
    match decode(&bytes) {
        Ok(offset) => Ok(Some(offset)),
        Err(detail) => Err(OpenError::Corrupt {
            detail: format!("CHECKPOINT sidecar at {}: {detail}", path.display()),
        }),
    }
}

/// Atomically persist `offset` to `<root>/CHECKPOINT` per §6.7:
/// write to `CHECKPOINT.tmp`, `fsync`, `rename`, `fsync` the
/// parent directory. Durability is required, not advisory — a
/// crash after `checkpoint(X)` but before housekeeping must
/// still find `X` on restart, or the driver's Parquet-side
/// suppression loses its horizon.
pub(crate) fn write(root: &Path, offset: WalOffset) -> Result<(), CheckpointError> {
    let io = |op: &'static str, source| CheckpointError::Io { op, source };
    let tmp = root.join(TMP_NAME);
    let mut file = File::create(&tmp).map_err(|e| io("create(CHECKPOINT.tmp)", e))?;
    file.write_all(&encode(offset))
        .map_err(|e| io("write(CHECKPOINT.tmp)", e))?;
    file.sync_all()
        .map_err(|e| io("fsync(CHECKPOINT.tmp)", e))?;
    std::fs::rename(&tmp, root.join(SIDECAR_NAME))
        .map_err(|e| io("rename(CHECKPOINT.tmp -> CHECKPOINT)", e))?;
    sync_parent_dir(root).map_err(|e| io("fsync(wal_root after checkpoint)", e))?;
    Ok(())
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
    fn encode_decode_round_trips() {
        let original = offset();
        let bytes = encode(original);
        assert_eq!(bytes.len(), SIDECAR_LEN);
        assert_eq!(decode(&bytes).expect("decode"), original);
    }

    #[test]
    fn encode_layout_is_the_pinned_32_bytes() {
        let original = offset();
        let bytes = encode(original);
        assert_eq!(&bytes[0..4], b"OWCK");
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 1);
        assert_eq!(u16::from_le_bytes([bytes[6], bytes[7]]), 0);
        assert_eq!(&bytes[8..24], original.segment.as_bytes());
        assert_eq!(
            u64::from_le_bytes(bytes[24..32].try_into().expect("8 bytes")),
            original.byte,
        );
    }

    #[test]
    fn decode_rejects_each_invalid_field() {
        let valid = encode(offset());
        let mut bad_magic = valid;
        bad_magic[0] = b'X';
        assert!(decode(&bad_magic).expect_err("magic").contains("bad magic"));
        let mut bad_version = valid;
        bad_version[4] = 2;
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

    #[test]
    fn read_absent_is_none_invalid_is_corrupt() {
        let tmp = tempfile::TempDir::new().expect("temp");
        assert!(read(tmp.path()).expect("absent").is_none());
        let original = offset();
        write(tmp.path(), original).expect("write");
        assert_eq!(read(tmp.path()).expect("present"), Some(original));
        std::fs::write(tmp.path().join(SIDECAR_NAME), b"garbage").expect("poison");
        match read(tmp.path()) {
            Err(OpenError::Corrupt { detail }) => {
                assert!(detail.contains("CHECKPOINT sidecar"), "detail: {detail}");
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }
}
