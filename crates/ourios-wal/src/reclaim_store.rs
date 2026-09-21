//! The `RECLAIM` sidecar file (RFC 0052 §3.2) — the lifecycle around
//! [`crate::reclaim`]'s codec.
//!
//! The file is created at its full two-slot size and fsynced once, at
//! `Wal::open`, and every later write rewrites the *inactive* slot in
//! place at a higher generation. That is the whole point of the shape:
//! a full disk is exactly when reclamation has to run, and a
//! temp-write-and-rename commit needs new blocks. A torn in-place write
//! leaves the previous slot live, so no rename, no temp and no
//! allocation are needed on the commit path.
//!
//! `slot_len` is fixed for the life of the file. When the configured
//! capacities outgrow the stored ones the file is rebuilt whole through
//! `RECLAIM.new` — write, fsync, rename, parent fsync — copying each
//! dictionary record to the *same* index in the wider stride, so every
//! slot id survives the rebuild unchanged.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::reclaim::{
    self, FILE_HEADER_LEN, Geometry, REBUILD_NAME, ReclaimRecord, SIDECAR_NAME, SLOT_COUNT,
    SlotIndex,
};
use crate::{OpenError, sync_file_data, sync_parent_dir};

/// The first generation a slot is written at. Zero means "never
/// written", which is how the untouched second slot of a freshly
/// created file reads.
const FIRST_GENERATION: u64 = 1;

/// Why the sidecar cannot be read or written.
#[derive(Debug)]
pub(crate) enum StoreError {
    Io {
        op: &'static str,
        source: std::io::Error,
    },
    /// The bytes are not a record this build can act on. Never read as
    /// "missing": missing means nothing was ever reclaimed, damaged
    /// means something was and the extent is unknown.
    Corrupt { detail: String },
}

impl From<StoreError> for OpenError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Io { op, source } => Self::Io { op, source },
            StoreError::Corrupt { detail } => Self::Corrupt { detail },
        }
    }
}

/// An open `RECLAIM` file: the geometry it was built at, the live
/// slot's record, and a reusable slot-sized buffer so a commit
/// allocates nothing.
#[derive(Debug)]
pub(crate) struct ReclaimStore {
    path: PathBuf,
    root: PathBuf,
    file: File,
    geometry: Geometry,
    live: SlotIndex,
    generation: u64,
    record: ReclaimRecord,
    buffer: Vec<u8>,
    full_fsync: bool,
}

/// Whether `<root>/RECLAIM` is there at all — the input to §3.2's
/// open-time matrix, which reads absence and damage as different
/// things.
pub(crate) fn present(root: &Path) -> Result<bool, StoreError> {
    root.join(SIDECAR_NAME)
        .try_exists()
        .map_err(|source| StoreError::Io {
            op: "stat(RECLAIM)",
            source,
        })
}

impl ReclaimStore {
    /// Create the file at `geometry`, holding `record`, and make it and
    /// its directory entry durable before returning. Every byte is
    /// written rather than the length merely set, so a volume with no
    /// room fails here — at open, where the operator can act — rather
    /// than on the first pass that needs the record.
    pub(crate) fn create(
        root: &Path,
        geometry: Geometry,
        record: &ReclaimRecord,
        full_fsync: bool,
    ) -> Result<Self, StoreError> {
        let path = root.join(SIDECAR_NAME);
        let mut store = Self {
            path: path.clone(),
            root: root.to_path_buf(),
            file: OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .map_err(|source| StoreError::Io {
                    op: "create(RECLAIM)",
                    source,
                })?,
            geometry,
            live: SlotIndex::First,
            generation: FIRST_GENERATION,
            record: record.clone(),
            buffer: vec![0u8; slot_bytes(geometry)],
            full_fsync,
        };
        let bytes = store.whole_file(record, FIRST_GENERATION)?;
        store.write_at(0, &bytes, "write(RECLAIM)")?;
        store.sync("fsync(RECLAIM)")?;
        sync_parent_dir(root).map_err(|source| StoreError::Io {
            op: "fsync(wal_root after create(RECLAIM))",
            source,
        })?;
        Ok(store)
    }

    /// Open the existing file, rebuilding it at the larger geometry
    /// when `needed` asks for more than it was built for. A configured
    /// value at or below the stored capacity opens in place and simply
    /// uses less of each slot; the file is never shrunk.
    pub(crate) fn open(
        root: &Path,
        needed: Geometry,
        full_fsync: bool,
    ) -> Result<Self, StoreError> {
        let path = root.join(SIDECAR_NAME);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| StoreError::Io {
                op: "open(RECLAIM)",
                source,
            })?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| StoreError::Io {
                op: "read(RECLAIM)",
                source,
            })?;
        let (geometry, live, generation, record) = decode_file(&path, &bytes)?;
        let mut store = Self {
            path,
            root: root.to_path_buf(),
            file,
            geometry,
            live,
            generation,
            record,
            buffer: vec![0u8; slot_bytes(geometry)],
            full_fsync,
        };
        if !geometry.covers(needed) {
            store.rebuild(needed)?;
        }
        Ok(store)
    }

    pub(crate) fn record(&self) -> &ReclaimRecord {
        &self.record
    }

    /// Make `record` the live one: lay it into the reusable buffer at
    /// the next generation, rewrite the *inactive* slot in place, and
    /// fsync. Nothing is allocated and the file never grows, so a pass
    /// on a full volume still commits.
    pub(crate) fn commit(&mut self, record: &ReclaimRecord) -> Result<(), StoreError> {
        let generation = self.generation.saturating_add(1);
        let target = self.live.other();
        reclaim::encode_slot_into(record, generation, self.geometry, &mut self.buffer)
            .map_err(|e| self.corrupt("encoding a slot", &e))?;
        let at = self.geometry.slot_offset(target);
        // Borrow the buffer out so `write_at` can take `&mut self`; it
        // is swapped straight back, and the length is unchanged, so the
        // next commit still allocates nothing.
        let buffer = std::mem::take(&mut self.buffer);
        let written = self.write_at(at, &buffer, "write(RECLAIM slot)");
        self.buffer = buffer;
        written?;
        self.sync("fsync(RECLAIM slot)")?;
        self.live = target;
        self.generation = generation;
        self.record = record.clone();
        Ok(())
    }

    /// Rewrite the whole file at a geometry covering both the stored
    /// capacities and `needed`, through `RECLAIM.new`. The rename is
    /// atomic and the temp is never read, so a crash at any point
    /// leaves either the old file or the new one, both complete.
    fn rebuild(&mut self, needed: Geometry) -> Result<(), StoreError> {
        let stored = self.geometry;
        let wider = Geometry::new(
            stored.max_tenants().max(needed.max_tenants()),
            stored
                .max_unlinks_per_pass()
                .max(needed.max_unlinks_per_pass()),
        )
        .map_err(|e| self.corrupt("sizing the rebuilt file", &e))?;
        self.geometry = wider;
        let record = self.record.clone();
        let bytes = self.whole_file(&record, self.generation)?;
        let temp = self.root.join(REBUILD_NAME);
        let mut new = File::create(&temp).map_err(|source| StoreError::Io {
            op: "create(RECLAIM.new)",
            source,
        })?;
        new.write_all(&bytes).map_err(|source| StoreError::Io {
            op: "write(RECLAIM.new)",
            source,
        })?;
        sync_file_data(&new, self.full_fsync).map_err(|source| StoreError::Io {
            op: "fsync(RECLAIM.new)",
            source,
        })?;
        std::fs::rename(&temp, &self.path).map_err(|source| StoreError::Io {
            op: "rename(RECLAIM.new -> RECLAIM)",
            source,
        })?;
        sync_parent_dir(&self.root).map_err(|source| StoreError::Io {
            op: "fsync(wal_root after rebuild(RECLAIM))",
            source,
        })?;
        self.file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(|source| StoreError::Io {
                op: "reopen(RECLAIM after rebuild)",
                source,
            })?;
        self.live = SlotIndex::First;
        self.buffer = vec![0u8; slot_bytes(wider)];
        Ok(())
    }

    /// The complete file: header, `record` in the first slot at
    /// `generation`, the second slot zeroed. A zeroed slot carries
    /// generation 0, which no writer produces, so the reader takes the
    /// written one.
    fn whole_file(&self, record: &ReclaimRecord, generation: u64) -> Result<Vec<u8>, StoreError> {
        let geometry = self.geometry;
        let mut bytes = vec![0u8; slot_bytes(geometry) * to_usize(SLOT_COUNT) + header_bytes()];
        bytes[..header_bytes()].copy_from_slice(&reclaim::encode_file_header(geometry));
        let first = to_usize(geometry.slot_offset(SlotIndex::First));
        let end = first + slot_bytes(geometry);
        reclaim::encode_slot_into(record, generation, geometry, &mut bytes[first..end])
            .map_err(|e| self.corrupt("encoding a slot", &e))?;
        Ok(bytes)
    }

    fn write_at(&mut self, at: u64, bytes: &[u8], op: &'static str) -> Result<(), StoreError> {
        self.file
            .seek(SeekFrom::Start(at))
            .map_err(|source| StoreError::Io { op, source })?;
        self.file
            .write_all(bytes)
            .map_err(|source| StoreError::Io { op, source })
    }

    fn sync(&self, op: &'static str) -> Result<(), StoreError> {
        sync_file_data(&self.file, self.full_fsync).map_err(|source| StoreError::Io { op, source })
    }

    fn corrupt(&self, doing: &str, source: &dyn std::fmt::Display) -> StoreError {
        StoreError::Corrupt {
            detail: format!(
                "RECLAIM sidecar at {}: {doing}: {source}",
                self.path.display()
            ),
        }
    }
}

/// The file header and both slots, or the first field that does not
/// check out. The version byte and every checksum are validated before
/// any record is believed.
fn decode_file(
    path: &Path,
    bytes: &[u8],
) -> Result<(Geometry, SlotIndex, u64, ReclaimRecord), StoreError> {
    let corrupt = |source: &dyn std::fmt::Display| StoreError::Corrupt {
        detail: format!("RECLAIM sidecar at {}: {source}", path.display()),
    };
    let header = bytes
        .get(..header_bytes())
        .ok_or_else(|| corrupt(&format!("size {} B, shorter than its header", bytes.len())))?;
    let geometry = reclaim::decode_file_header(header).map_err(|e| corrupt(&e))?;
    let expected = to_usize(geometry.file_len());
    if bytes.len() != expected {
        return Err(corrupt(&format!(
            "size {} B, expected {expected} B at the stored capacities",
            bytes.len()
        )));
    }
    let slot = |index: SlotIndex| {
        let at = to_usize(geometry.slot_offset(index));
        reclaim::decode_slot(&bytes[at..at + slot_bytes(geometry)], geometry)
    };
    let (live, decoded) = reclaim::choose_live(slot(SlotIndex::First), slot(SlotIndex::Second))
        .map_err(|e| corrupt(&e))?;
    Ok((geometry, live, decoded.generation, decoded.record))
}

fn header_bytes() -> usize {
    to_usize(FILE_HEADER_LEN)
}

fn slot_bytes(geometry: Geometry) -> usize {
    to_usize(geometry.slot_len())
}

/// `Geometry::new` proved the whole file fits a `usize`, so every
/// length derived from it does too.
fn to_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reclaim::{RecordedMode, Witness, WitnessFlags};

    fn geometry(tenants: u32, unlinks: u32) -> Geometry {
        Geometry::new(tenants, unlinks).expect("geometry")
    }

    fn armed() -> ReclaimRecord {
        ReclaimRecord {
            witness: WitnessFlags {
                checkpoint: Witness::Armed,
                ..WitnessFlags::default()
            },
            ..ReclaimRecord::default()
        }
    }

    /// The created file is exactly the two-slot size the geometry
    /// gives, and reopening it yields the record it was created with.
    #[test]
    fn create_preallocates_the_whole_file_and_reopens_to_the_same_record() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let g = geometry(4, 2);
        let record = armed();
        ReclaimStore::create(tmp.path(), g, &record, false).expect("create");
        let len = std::fs::metadata(tmp.path().join(SIDECAR_NAME))
            .expect("stat")
            .len();
        assert_eq!(len, g.file_len(), "the file is preallocated whole");
        let store = ReclaimStore::open(tmp.path(), g, false).expect("reopen");
        assert_eq!(store.record(), &record);
    }

    /// A commit alternates slots, never extends the file, and is what a
    /// later reader sees — the property that makes a pass on a full
    /// volume safe.
    #[test]
    fn a_commit_alternates_slots_without_growing_the_file() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let g = geometry(4, 2);
        let mut store =
            ReclaimStore::create(tmp.path(), g, &ReclaimRecord::default(), false).expect("create");
        assert_eq!(store.live, SlotIndex::First);
        let mut record = armed();
        store.commit(&record).expect("commit");
        assert_eq!(store.live, SlotIndex::Second);
        record.consumer_mode = RecordedMode::NoConsumer;
        store.commit(&record).expect("second commit");
        assert_eq!(store.live, SlotIndex::First);
        assert_eq!(
            std::fs::metadata(tmp.path().join(SIDECAR_NAME))
                .expect("stat")
                .len(),
            g.file_len(),
            "no commit ever extends the file",
        );
        let reopened = ReclaimStore::open(tmp.path(), g, false).expect("reopen");
        assert_eq!(reopened.record(), &record);
    }

    /// A configured capacity above the stored one rebuilds the file
    /// wider and keeps the record; one at or below opens in place.
    #[test]
    fn a_wider_geometry_rebuilds_and_a_narrower_one_opens_in_place() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let small = geometry(2, 1);
        let record = armed();
        ReclaimStore::create(tmp.path(), small, &record, false).expect("create");
        let wide = geometry(8, 4);
        let grown = ReclaimStore::open(tmp.path(), wide, false).expect("rebuild");
        assert_eq!(grown.geometry, wide);
        assert_eq!(grown.record(), &record);
        assert!(
            !tmp.path().join(REBUILD_NAME).exists(),
            "the rebuild temp is renamed away, never left behind",
        );
        let unchanged = ReclaimStore::open(tmp.path(), small, false).expect("open in place");
        assert_eq!(
            unchanged.geometry, wide,
            "a smaller configured value never shrinks the file",
        );
    }

    /// Damage anywhere in the header or in both slots is `Corrupt`
    /// naming the file — never silently read as a missing record.
    #[test]
    fn a_damaged_file_is_corrupt_naming_the_file() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let g = geometry(2, 1);
        ReclaimStore::create(tmp.path(), g, &armed(), false).expect("create");
        let path = tmp.path().join(SIDECAR_NAME);
        let mut bytes = std::fs::read(&path).expect("read");
        bytes[4] = 0xFF;
        std::fs::write(&path, &bytes).expect("poison the version byte");
        match ReclaimStore::open(tmp.path(), g, false) {
            Err(StoreError::Corrupt { detail }) => {
                assert!(detail.contains("RECLAIM sidecar at"), "detail: {detail}");
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    /// A torn write into the inactive slot leaves the previous slot
    /// live: the reader takes the valid slot with the greater
    /// generation, so the record the last complete commit left survives.
    #[test]
    fn a_torn_inactive_slot_leaves_the_previous_slot_live() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let g = geometry(2, 1);
        let mut store =
            ReclaimStore::create(tmp.path(), g, &ReclaimRecord::default(), false).expect("create");
        let committed = armed();
        store.commit(&committed).expect("commit");
        drop(store);
        let path = tmp.path().join(SIDECAR_NAME);
        let mut bytes = std::fs::read(&path).expect("read");
        // The first slot is the inactive one after that commit; tear
        // it the way a half-finished in-place write would.
        let at = to_usize(g.slot_offset(SlotIndex::First));
        bytes[at + 32] ^= 0xFF;
        std::fs::write(&path, &bytes).expect("tear the inactive slot");
        let reopened = ReclaimStore::open(tmp.path(), g, false).expect("reopen");
        assert_eq!(reopened.record(), &committed);
    }
}
