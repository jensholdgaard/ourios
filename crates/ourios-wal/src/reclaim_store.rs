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
    self, FILE_HEADER_BYTES, FILE_HEADER_LEN, Geometry, REBUILD_NAME, ReclaimRecord, SIDECAR_NAME,
    SlotIndex,
};
use crate::{OpenError, sync_file_data, sync_parent_dir};

/// RFC 0052 §6's fault-injection point inside a pass's `RECLAIM` write:
/// it runs immediately before the slot is written, so a test can hold
/// the write in flight and observe what the rest of the WAL does
/// meanwhile. Inert unless armed, and armed only through the
/// `fault-injection` feature.
#[derive(Clone, Default)]
pub(crate) struct WriteHook(Option<std::sync::Arc<dyn Fn() + Send + Sync>>);

impl WriteHook {
    #[cfg(feature = "fault-injection")]
    pub(crate) fn armed(hook: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Some(std::sync::Arc::new(hook)))
    }

    pub(crate) fn run(&self) {
        if let Some(hook) = &self.0 {
            hook();
        }
    }
}

impl std::fmt::Debug for WriteHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("WriteHook").field(&self.0.is_some()).finish()
    }
}

/// The `RECLAIM` sidecar as the [`crate::Wal`] and the file half of its
/// passes share it (RFC 0052 §3.7).
///
/// A pass writes its record with the journal guard released, while a
/// checkpoint still writes the same file under it, so the store has an
/// owner of its own: one mutex over the store and its fd. Every slot
/// write — a pass's record, a checkpoint's arming and witness, the
/// record a rotation creates — and every read-modify-write of the live
/// record goes through it, so no two writers ever interleave on the
/// two-slot alternation.
///
/// The cell a pass's [`crate::UnlinkPermit`] reads lives here too, and
/// it only moves with this lock held. That is what makes a pass's
/// "is this plan still the live one?" and its record write one step: a
/// later `housekeeping_prepare` cannot supersede a plan in the middle
/// of writing it.
///
/// Whether a record exists and whether its checkpoint witness is
/// terminal are also published lock-free, refreshed whenever a lock is
/// released, because rotation and the §3.5 export read them under the
/// journal guard and must not wait behind a pass's slot write.
#[derive(Clone)]
pub(crate) struct ReclaimSlot(std::sync::Arc<Shared>);

struct Shared {
    instance: u64,
    live: std::sync::Arc<std::sync::atomic::AtomicU64>,
    max_unlinks_per_pass: u32,
    state: std::sync::Mutex<SlotState>,
    recorded: std::sync::atomic::AtomicBool,
    witnessed: std::sync::atomic::AtomicBool,
}

struct SlotState {
    store: Option<ReclaimStore>,
    hook: WriteHook,
    /// The owning `Wal` was dropped: a plan that outlives it writes
    /// nothing, since a reopen of the root holds its own handle on the
    /// same file.
    closed: bool,
}

/// The slot, locked. Dereferences to the store, and publishes the
/// lock-free summary when it is released.
pub(crate) struct Held<'a> {
    shared: &'a Shared,
    state: std::sync::MutexGuard<'a, SlotState>,
}

impl ReclaimSlot {
    pub(crate) fn new(
        store: Option<ReclaimStore>,
        instance: u64,
        live: std::sync::Arc<std::sync::atomic::AtomicU64>,
        max_unlinks_per_pass: u32,
    ) -> Self {
        let slot = Self(std::sync::Arc::new(Shared {
            instance,
            live,
            max_unlinks_per_pass,
            state: std::sync::Mutex::new(SlotState {
                store,
                hook: WriteHook::default(),
                closed: false,
            }),
            recorded: std::sync::atomic::AtomicBool::new(false),
            witnessed: std::sync::atomic::AtomicBool::new(false),
        }));
        drop(slot.lock());
        slot
    }

    /// Recovering a poisoned lock is sound here: a commit updates its
    /// in-memory generation and live slot only after the write and the
    /// fsync returned, so a panic part-way leaves the previous record
    /// live on disk and in memory alike, and the next commit rewrites
    /// the same inactive slot.
    pub(crate) fn lock(&self) -> Held<'_> {
        Held {
            shared: &self.0,
            state: self
                .0
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        }
    }

    /// The `Wal` this slot belongs to, as its [`crate::PassId`]s name it.
    pub(crate) fn instance(&self) -> u64 {
        self.0.instance
    }

    /// The live-pass cell; see the type's documentation for when it
    /// may move.
    pub(crate) fn live(&self) -> &std::sync::Arc<std::sync::atomic::AtomicU64> {
        &self.0.live
    }

    pub(crate) fn max_unlinks_per_pass(&self) -> u32 {
        self.0.max_unlinks_per_pass
    }

    /// Whether the root has a record at all. Once it has one it never
    /// loses it, so a `true` here needs no lock to act on.
    pub(crate) fn has_record(&self) -> bool {
        self.0.recorded.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Whether the record's checkpoint witness is terminal.
    pub(crate) fn checkpoint_witnessed(&self) -> bool {
        self.0.witnessed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Revoke every outstanding permit and refuse every later record
    /// write through this slot. Taken under the lock, so a write already
    /// in flight finishes first rather than racing the reopen that may
    /// follow.
    ///
    /// The store is dropped here rather than with the last handle: a
    /// plan keeps the slot alive for as long as the caller holds it, and
    /// its descriptor on `RECLAIM` must not outlive the `Wal`, or a
    /// caller that keeps one plan per reopen leaks one fd per reopen.
    pub(crate) fn close(&self) {
        let mut held = self.lock();
        held.state.closed = true;
        held.state.store = None;
        self.0.live.store(0, std::sync::atomic::Ordering::Release);
    }

    #[cfg(feature = "fault-injection")]
    pub(crate) fn arm_hook(&self, hook: WriteHook) {
        self.lock().state.hook = hook;
    }
}

impl std::fmt::Debug for ReclaimSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReclaimSlot")
            .field("instance", &self.0.instance)
            .field("recorded", &self.has_record())
            .finish_non_exhaustive()
    }
}

impl Held<'_> {
    pub(crate) fn closed(&self) -> bool {
        self.state.closed
    }

    pub(crate) fn hook(&self) -> WriteHook {
        self.state.hook.clone()
    }
}

impl std::ops::Deref for Held<'_> {
    type Target = Option<ReclaimStore>;

    fn deref(&self) -> &Self::Target {
        &self.state.store
    }
}

impl std::ops::DerefMut for Held<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state.store
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let store = self.state.store.as_ref();
        let witnessed = store
            .is_some_and(|store| store.record().witness.checkpoint == reclaim::Witness::Terminal);
        self.shared
            .recorded
            .store(store.is_some(), std::sync::atomic::Ordering::Release);
        self.shared
            .witnessed
            .store(witnessed, std::sync::atomic::Ordering::Release);
    }
}

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

/// The parts a store is assembled from once its file exists — the
/// same set whether the file was just created or reopened. The
/// reusable slot buffer is one of them so it is reserved fallibly, by
/// whichever path already knows the geometry, and then moved into the
/// store rather than allocated a second time.
struct Opened {
    file: File,
    geometry: Geometry,
    live: SlotIndex,
    generation: u64,
    record: ReclaimRecord,
    buffer: Vec<u8>,
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
    ///
    /// **Creation goes through `RECLAIM.new`, like the rebuild.**
    /// Writing the final name first would leave a half-written sidecar
    /// behind when the allocation fails part-way, and the next open
    /// reads *present* bytes as corruption rather than as absence — so
    /// a transient ENOSPC would brick a root on which nothing had been
    /// reclaimed. The rename is atomic and the temp is never read, so
    /// every surviving state is either "no record" or "a complete
    /// record". This is not the §3.2 commit path, which must never
    /// allocate; a creation allocates by definition.
    pub(crate) fn create(
        root: &Path,
        geometry: Geometry,
        record: &ReclaimRecord,
        full_fsync: bool,
    ) -> Result<Self, StoreError> {
        let path = root.join(SIDECAR_NAME);
        let buffer = zeroed(slot_bytes(geometry), &path, "a slot")?;
        let bytes = whole_file(geometry, record, FIRST_GENERATION, &path)?;
        let opened = Opened {
            file: install(root, &path, &bytes, full_fsync)?,
            geometry,
            live: SlotIndex::First,
            generation: FIRST_GENERATION,
            record: record.clone(),
            buffer,
        };
        Ok(Self::assemble(root, opened, full_fsync))
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
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| StoreError::Io {
                op: "open(RECLAIM)",
                source,
            })?;
        let opened = decode_file(&path, file)?;
        let geometry = opened.geometry;
        let mut store = Self::assemble(root, opened, full_fsync);
        if !geometry.covers(needed) {
            store.rebuild(needed)?;
        }
        Ok(store)
    }

    /// Build the store around a file that already exists, wherever it
    /// came from.
    fn assemble(root: &Path, opened: Opened, full_fsync: bool) -> Self {
        Self {
            path: root.join(SIDECAR_NAME),
            root: root.to_path_buf(),
            buffer: opened.buffer,
            file: opened.file,
            geometry: opened.geometry,
            live: opened.live,
            generation: opened.generation,
            record: opened.record,
            full_fsync,
        }
    }

    pub(crate) fn record(&self) -> &ReclaimRecord {
        &self.record
    }

    /// Take `record` as the in-memory state **without** writing it.
    ///
    /// RFC 0052 §3.2 advances `reclaimed_through` in
    /// `housekeeping_commit`, which runs under the writer position —
    /// exactly where a slot write and its fsync must not — and makes
    /// it durable at the next record write. The gap is safe because
    /// the `planned` list was durable before the unlinks: a crash in
    /// it reconciles at open, where an absent planned segment is
    /// treated as a completed reclamation and raises the same entry.
    pub(crate) fn adopt(&mut self, record: ReclaimRecord) {
        self.record = record;
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
        let buffer = zeroed(slot_bytes(wider), &self.path, "a slot")?;
        let bytes = whole_file(wider, &self.record.clone(), self.generation, &self.path)?;
        self.file = install(&self.root, &self.path, &bytes, self.full_fsync)?;
        self.geometry = wider;
        self.live = SlotIndex::First;
        self.buffer = buffer;
        Ok(())
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

/// The complete file: header, `record` in the first slot at
/// `generation`, the second slot zeroed. A zeroed slot carries
/// generation 0, which no writer produces, so the reader takes the
/// written one.
fn whole_file(
    geometry: Geometry,
    record: &ReclaimRecord,
    generation: u64,
    path: &Path,
) -> Result<Vec<u8>, StoreError> {
    let mut bytes = zeroed(to_usize(geometry.file_len()), path, "a whole file")?;
    bytes[..header_bytes()].copy_from_slice(&reclaim::encode_file_header(geometry));
    let first = to_usize(geometry.slot_offset(SlotIndex::First));
    let end = first + slot_bytes(geometry);
    reclaim::encode_slot_into(record, generation, geometry, &mut bytes[first..end]).map_err(
        |e| StoreError::Corrupt {
            detail: format!(
                "RECLAIM sidecar at {}: encoding a slot: {e}",
                path.display()
            ),
        },
    )?;
    Ok(bytes)
}

/// Put `bytes` at `path` atomically: write them whole to
/// `RECLAIM.new`, fsync it, rename it over `path`, fsync the parent,
/// and hand back a handle on the result. The temp is never read, so a
/// crash or a failed allocation at any point leaves either the old
/// file or the new one and never a half-written sidecar — which the
/// next open would have to read as corruption rather than absence.
fn install(root: &Path, path: &Path, bytes: &[u8], full_fsync: bool) -> Result<File, StoreError> {
    let io = |op: &'static str| move |source| StoreError::Io { op, source };
    let temp = root.join(REBUILD_NAME);
    let mut new = File::create(&temp).map_err(io("create(RECLAIM.new)"))?;
    new.write_all(bytes).map_err(io("write(RECLAIM.new)"))?;
    sync_file_data(&new, full_fsync).map_err(io("fsync(RECLAIM.new)"))?;
    std::fs::rename(&temp, path).map_err(io("rename(RECLAIM.new -> RECLAIM)"))?;
    sync_parent_dir(root).map_err(io("fsync(wal_root after installing RECLAIM)"))?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(io("reopen(RECLAIM after install)"))
}

/// The file header and both slots, or the first field that does not
/// check out. The version byte and every checksum are validated before
/// any record is believed. The slot buffer the decode reserves is
/// handed back in the [`Opened`] so the store reuses it.
///
/// The fixed header is read and validated **before** anything sized by
/// it is allocated, and the geometry it declares is checked against the
/// file's real length: a header claiming the format ceiling describes a
/// file of hundreds of GiB, and trusting it enough to read the file
/// whole would let a malformed sidecar stall or exhaust startup.
fn decode_file(path: &Path, mut file: File) -> Result<Opened, StoreError> {
    let corrupt = |source: &dyn std::fmt::Display| StoreError::Corrupt {
        detail: format!("RECLAIM sidecar at {}: {source}", path.display()),
    };
    let io = |op: &'static str| move |source| StoreError::Io { op, source };
    let mut header = [0u8; FILE_HEADER_BYTES];
    file.read_exact(&mut header)
        .map_err(io("read(RECLAIM file header)"))?;
    let geometry = reclaim::decode_file_header(&header).map_err(|e| corrupt(&e))?;
    let expected = geometry.file_len();
    let found = file.metadata().map_err(io("stat(RECLAIM)"))?.len();
    if found != expected {
        return Err(corrupt(&format!(
            "size {found} B, expected {expected} B at the stored capacities"
        )));
    }
    let mut slot = zeroed(slot_bytes(geometry), path, "a slot")?;
    let mut read = |index: SlotIndex| -> Result<_, StoreError> {
        file.seek(SeekFrom::Start(geometry.slot_offset(index)))
            .map_err(io("seek(RECLAIM slot)"))?;
        file.read_exact(&mut slot)
            .map_err(io("read(RECLAIM slot)"))?;
        Ok(reclaim::decode_slot(&slot, geometry))
    };
    let first = read(SlotIndex::First)?;
    let second = read(SlotIndex::Second)?;
    let (live, decoded) = reclaim::choose_live(first, second).map_err(|e| corrupt(&e))?;
    Ok(Opened {
        file,
        geometry,
        live,
        generation: decoded.generation,
        record: decoded.record,
        buffer: slot,
    })
}

/// `len` zeroed bytes, or `Corrupt` naming the file.
///
/// Every buffer on the open path is sized from what the file declares,
/// and §3.2's 65,536 × 65,536 format ceilings are a legal shape
/// describing a ~128 GiB slot, so each one is reserved fallibly: a
/// stored geometry this machine cannot hold is a refusal the operator
/// can act on, never an allocation abort during startup.
fn zeroed(len: usize, path: &Path, what: &str) -> Result<Vec<u8>, StoreError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| StoreError::Corrupt {
            detail: format!(
                "RECLAIM sidecar at {}: cannot hold {what} of {len} B",
                path.display()
            ),
        })?;
    bytes.resize(len, 0);
    Ok(bytes)
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

    /// A creation that cannot complete leaves **no** `RECLAIM` behind.
    /// Writing the final name first would leave a half-written sidecar
    /// that the next open reads as corruption rather than absence, so a
    /// transient allocation failure would brick a root on which nothing
    /// had been reclaimed — exactly the case §3.2 says must start
    /// normally once space is freed.
    #[test]
    fn a_failed_create_leaves_no_partial_sidecar_behind() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let g = geometry(4, 2);
        // A directory in the temp's place: `File::create` cannot open
        // it, which stands in for the allocation failing part-way.
        std::fs::create_dir_all(tmp.path().join(REBUILD_NAME)).expect("block the temp");
        ReclaimStore::create(tmp.path(), g, &armed(), false).expect_err("create must fail");
        assert!(
            !tmp.path().join(SIDECAR_NAME).exists(),
            "the final name is never touched until the temp is complete",
        );

        // And once the obstruction is gone the root starts normally.
        std::fs::remove_dir(tmp.path().join(REBUILD_NAME)).expect("free the temp");
        let store = ReclaimStore::create(tmp.path(), g, &armed(), false).expect("create");
        assert_eq!(store.record(), &armed());
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

    /// Damage is `Corrupt` naming the file, never silently read as a
    /// missing record — and a header declaring a geometry the file
    /// does not have is refused *before* anything sized by it is
    /// allocated, since the stored capacities can describe hundreds of
    /// GiB.
    #[test]
    fn a_damaged_file_is_corrupt_naming_the_file() {
        /// How one case damages a valid file.
        type Damage = fn(Vec<u8>) -> Vec<u8>;

        let cases: [(&str, Damage); 2] = [
            ("the version byte", |mut bytes| {
                bytes[4] = 0xFF;
                bytes
            }),
            // The header stays intact; the file is not as long as it
            // claims to be.
            ("the file length", |bytes| bytes[..bytes.len() - 1].to_vec()),
        ];
        for (what, damage) in cases {
            let tmp = tempfile::TempDir::new().expect("temp");
            let g = geometry(2, 1);
            ReclaimStore::create(tmp.path(), g, &armed(), false).expect("create");
            let path = tmp.path().join(SIDECAR_NAME);
            let bytes = std::fs::read(&path).expect("read");
            std::fs::write(&path, damage(bytes)).expect("damage the file");
            match ReclaimStore::open(tmp.path(), g, false) {
                Err(StoreError::Corrupt { detail }) => assert!(
                    detail.contains("RECLAIM sidecar at"),
                    "{what}: detail names the file, got {detail}",
                ),
                other => panic!("{what}: expected Corrupt, got {other:?}"),
            }
        }
    }

    /// A stored geometry this machine cannot hold is `Corrupt` naming
    /// the file, not an allocation abort — and the buffer the decode
    /// reserved is the one the store keeps, so opening never allocates
    /// a second slot past the guard.
    ///
    /// The refusal is asserted at the guard rather than through
    /// `open`, because driving the real path to it means a sidecar
    /// declaring §3.2's ceiling geometry, and a host that lets a
    /// ~128 GiB reservation through would then zero it for real.
    /// `usize::MAX` overflows the reserve without touching the
    /// allocator, which is the same branch every call site takes.
    #[test]
    fn a_geometry_too_large_to_hold_is_corrupt_rather_than_an_abort() {
        match zeroed(usize::MAX, Path::new("/wal/RECLAIM"), "a slot") {
            Err(StoreError::Corrupt { detail }) => {
                assert!(detail.contains("/wal/RECLAIM"), "names the file: {detail}");
                assert!(
                    detail.contains("cannot hold a slot"),
                    "names what did not fit: {detail}",
                );
            }
            Err(other) => panic!("expected Corrupt, got {other:?}"),
            Ok(bytes) => panic!("expected a refusal, got {} B", bytes.len()),
        }

        let tmp = tempfile::TempDir::new().expect("temp");
        let g = geometry(4, 2);
        ReclaimStore::create(tmp.path(), g, &armed(), false).expect("create");
        let store = ReclaimStore::open(tmp.path(), g, false).expect("reopen");
        assert_eq!(
            store.buffer.len(),
            slot_bytes(g),
            "the decode's checked buffer is the store's commit buffer",
        );
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
