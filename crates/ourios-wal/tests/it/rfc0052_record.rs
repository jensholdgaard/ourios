//! Reading a `RECLAIM` file back, by RFC 0052 §3.2's offset table.
//!
//! `rfc0052_support` builds these files; this reads them. The split is
//! not tidiness: a fixture that encoded and decoded through the same
//! helper would round-trip a reordered field or a flipped endianness
//! without noticing, and several legs assert on entries a *pass* wrote
//! rather than on bytes the fixture laid down.

use std::collections::BTreeMap;
use std::path::Path;

use ourios_wal::WalOffset;

use crate::rfc0052_support::{FILE_HEADER_LEN, RECLAIM, live_slot, stored_slot_len};

const SLOT_HEADER_LEN: usize = 24;
const DICT_RECORD_LEN: usize = 132;
const ENTRY_LEN: usize = 32;
const PLANNED_HEADER_LEN: usize = 24;
const PAIR_LEN: usize = 32;

const ENTRY_OCCUPIED: u16 = 1 << 0;
const PLANNED_UNCERTAIN: u8 = 1 << 0;
const PLANNED_OCCUPIED: u8 = 1 << 1;

/// One `planned` record as §3.2 stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedRow {
    pub segment: uuid::Uuid,
    /// §3.2's uncertain deletion: the unlink returned `Ok` but the
    /// parent fsync did not.
    pub uncertain: bool,
}

/// Every tenant's `reclaimed_through` in the live slot.
pub fn reclaimed_through(root: &Path) -> BTreeMap<String, WalOffset> {
    Record::read(root).entries()
}

/// The live slot's `planned` records, oldest position first.
pub fn planned_unlinks(root: &Path) -> Vec<PlannedRow> {
    Record::read(root).planned()
}

/// A `RECLAIM` file's live slot with the geometry needed to walk it.
///
/// **Nothing here is walked by a count**: entry `i` belongs to
/// dictionary record `i` and pair `i` to the same, so a position *is*
/// an identity and occupancy is the record's own flag. Reading
/// `entry_count` as a length would read a torn or stale slot's
/// arithmetic.
struct Record {
    slot: Vec<u8>,
    max_tenants: usize,
    max_unlinks: usize,
}

impl Record {
    fn read(root: &Path) -> Self {
        let bytes = std::fs::read(root.join(RECLAIM)).expect("read RECLAIM");
        let len = stored_slot_len(&bytes);
        let (generation, _, _) = live_slot(&bytes);
        let first = u64::from_le_bytes(
            bytes[FILE_HEADER_LEN..FILE_HEADER_LEN + 8]
                .try_into()
                .expect("8 bytes"),
        );
        let at = FILE_HEADER_LEN + usize::from(first != generation) * len;
        Self {
            slot: bytes[at..at + len].to_vec(),
            max_tenants: capacity(&bytes, 16),
            max_unlinks: capacity(&bytes, 20),
        }
    }

    fn entries(&self) -> BTreeMap<String, WalOffset> {
        let entries_at = SLOT_HEADER_LEN + DICT_RECORD_LEN * self.max_tenants;
        let mut out = BTreeMap::new();
        for index in 0..self.max_tenants {
            let dict = &self.slot[SLOT_HEADER_LEN + DICT_RECORD_LEN * index..][..DICT_RECORD_LEN];
            let len = usize::from(u16::from_le_bytes(dict[0..2].try_into().expect("2 bytes")));
            let entry = &self.slot[entries_at + ENTRY_LEN * index..][..ENTRY_LEN];
            let flags = u16::from_le_bytes(entry[0..2].try_into().expect("2 bytes"));
            if len == 0 || flags & ENTRY_OCCUPIED == 0 {
                continue;
            }
            let key = String::from_utf8(dict[2..2 + len].to_vec()).expect("ascii key");
            out.insert(key, offset_at(&entry[8..32]));
        }
        out
    }

    fn planned(&self) -> Vec<PlannedRow> {
        let planned_at = SLOT_HEADER_LEN + (DICT_RECORD_LEN + ENTRY_LEN) * self.max_tenants;
        let stride = PLANNED_HEADER_LEN + PAIR_LEN * self.max_tenants;
        let mut out = Vec::new();
        for index in 0..self.max_unlinks {
            let record = &self.slot[planned_at + stride * index..][..PLANNED_HEADER_LEN];
            // A planned position no segment occupies is a zeroed run.
            if record[22] & PLANNED_OCCUPIED == 0 {
                continue;
            }
            out.push(PlannedRow {
                segment: uuid::Uuid::from_slice(&record[0..16]).expect("16 bytes"),
                uncertain: record[22] & PLANNED_UNCERTAIN != 0,
            });
        }
        out
    }
}

/// A `u32` capacity in the 32 B file header.
fn capacity(bytes: &[u8], at: usize) -> usize {
    usize::try_from(u32::from_le_bytes(
        bytes[at..at + 4].try_into().expect("4 bytes"),
    ))
    .expect("capacity fits usize")
}

/// A `WalOffset` as §3.2 stores it: 16 B uuid in RFC 4122 order then a
/// little-endian `u64` byte.
fn offset_at(bytes: &[u8]) -> WalOffset {
    WalOffset {
        segment: uuid::Uuid::from_slice(&bytes[0..16]).expect("16 bytes"),
        byte: u64::from_le_bytes(bytes[16..24].try_into().expect("8 bytes")),
    }
}
