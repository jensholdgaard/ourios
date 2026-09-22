//! Byte-level fixtures for RFC 0052 §3.2's two sidecars.
//!
//! Both codecs are crate-internal, so these roots are built from the
//! §3.2 offset tables directly rather than through the encoder under
//! test. That is the point of a format fixture: the test and the code
//! have to agree on the bytes without sharing an implementation, so a
//! reordered field or a flipped endianness fails here rather than
//! round-tripping quietly.

use std::path::Path;

use ourios_wal::{FrameKind, Wal, WalConfig, WalOffset};

pub const RECLAIM: &str = "RECLAIM";
pub const REBUILD: &str = "RECLAIM.new";
pub const CHECKPOINT: &str = "CHECKPOINT";

pub const FILE_HEADER_LEN: usize = 32;
const SLOT_HEADER_LEN: usize = 24;
const SLOT_TRAILER_LEN: usize = 8;
const DICT_RECORD_LEN: usize = 132;
const ENTRY_LEN: usize = 32;
const PLANNED_HEADER_LEN: usize = 24;
const PAIR_LEN: usize = 32;
const SEGMENT_HEADER_LEN: usize = 24;

pub const CHECKPOINT_ARMED: u16 = 1 << 0;
pub const CHECKPOINT_SEEN: u16 = 1 << 1;
pub const MODE_UNRECORDED: u16 = 0;
pub const MODE_KNOWN: u16 = 1;
pub const MODE_NO_CONSUMER: u16 = 2;

/// The RFC 0052 §3.2 defaults the WAL sizes every record it creates
/// for, until `max_tenants` and `max_unlinks_per_pass` become
/// `WalConfig` knobs.
pub const DEFAULT_MAX_TENANTS: usize = 1_024;
pub const DEFAULT_MAX_UNLINKS_PER_PASS: usize = 128;

pub fn default_config(root: &Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
        macos_full_fsync: false,
    }
}

pub fn open(root: &Path) -> Wal {
    Wal::open(default_config(root)).expect("open")
}

/// `32 + 164 × T + 24 × U + 32 × T × U` — §3.2's one normative
/// definition of `slot_len`.
pub fn slot_len(max_tenants: usize, max_unlinks: usize) -> usize {
    SLOT_HEADER_LEN
        + SLOT_TRAILER_LEN
        + (DICT_RECORD_LEN + ENTRY_LEN) * max_tenants
        + (PLANNED_HEADER_LEN + PAIR_LEN * max_tenants) * max_unlinks
}

/// A whole `RECLAIM` file at the given capacities, holding an empty
/// record with `flags` and `mode` in its first slot at generation 1.
/// The second slot is left zeroed, which reads as generation 0 —
/// "never written" — exactly as a freshly created file does.
pub fn reclaim_file(max_tenants: usize, max_unlinks: usize, flags: u16, mode: u16) -> Vec<u8> {
    let slot = slot_len(max_tenants, max_unlinks);
    let mut out = vec![0u8; FILE_HEADER_LEN + 2 * slot];
    out[0..4].copy_from_slice(b"OWRC");
    out[4..6].copy_from_slice(&1u16.to_le_bytes());
    out[8..16].copy_from_slice(
        &u64::try_from(slot)
            .expect("slot_len fits u64")
            .to_le_bytes(),
    );
    out[16..20].copy_from_slice(
        &u32::try_from(max_tenants)
            .expect("capacity fits u32")
            .to_le_bytes(),
    );
    out[20..24].copy_from_slice(
        &u32::try_from(max_unlinks)
            .expect("capacity fits u32")
            .to_le_bytes(),
    );
    let crc = crc32c::crc32c(&out[0..24]);
    out[24..28].copy_from_slice(&crc.to_le_bytes());
    write_slot_header(
        &mut out[FILE_HEADER_LEN..FILE_HEADER_LEN + slot],
        1,
        flags,
        mode,
    );
    out
}

fn write_slot_header(slot: &mut [u8], generation: u64, flags: u16, mode: u16) {
    slot[0..8].copy_from_slice(&generation.to_le_bytes());
    slot[16..18].copy_from_slice(&flags.to_le_bytes());
    slot[18..20].copy_from_slice(&mode.to_le_bytes());
    seal(slot);
}

/// Recompute a slot's CRC32-C trailer over `[0 .. slot_len - 8)`.
fn seal(slot: &mut [u8]) {
    let trailer = slot.len() - SLOT_TRAILER_LEN;
    let crc = crc32c::crc32c(&slot[..trailer]);
    slot[trailer..trailer + 4].copy_from_slice(&crc.to_le_bytes());
}

/// The stored `slot_len`, read from the file header the way the WAL
/// does rather than recomputed from the defaults.
pub fn stored_slot_len(bytes: &[u8]) -> usize {
    usize::try_from(u64::from_le_bytes(
        bytes[8..16].try_into().expect("8 bytes"),
    ))
    .expect("slot_len fits usize")
}

/// The live slot's `(generation, header_flags, consumer_mode)`: the
/// valid slot with the greater generation, chosen exactly as a reader
/// chooses it.
pub fn live_slot(bytes: &[u8]) -> (u64, u16, u16) {
    let slot = stored_slot_len(bytes);
    let read = |index: usize| -> Option<(u64, u16, u16)> {
        let at = FILE_HEADER_LEN + index * slot;
        let body = &bytes[at..at + slot];
        let trailer = slot - SLOT_TRAILER_LEN;
        let stored = u32::from_le_bytes(body[trailer..trailer + 4].try_into().expect("4 bytes"));
        if stored != crc32c::crc32c(&body[..trailer]) {
            return None;
        }
        let generation = u64::from_le_bytes(body[0..8].try_into().expect("8 bytes"));
        if generation == 0 {
            return None;
        }
        Some((
            generation,
            u16::from_le_bytes(body[16..18].try_into().expect("2 bytes")),
            u16::from_le_bytes(body[18..20].try_into().expect("2 bytes")),
        ))
    };
    match (read(0), read(1)) {
        (Some(a), Some(b)) if a.0 >= b.0 => a,
        (Some(a), None) => a,
        (Some(_) | None, Some(b)) => b,
        (None, None) => panic!("neither RECLAIM slot is valid"),
    }
}

/// Rewrite the live slot's `header_flags` and `consumer_mode` in
/// place, resealing its CRC — the state a crash in §3.2's migration
/// window leaves behind.
pub fn set_live_witness(root: &Path, flags: u16, mode: u16) {
    let path = root.join(RECLAIM);
    let mut bytes = std::fs::read(&path).expect("read RECLAIM");
    let slot = stored_slot_len(&bytes);
    let (generation, _, _) = live_slot(&bytes);
    let index = usize::from(
        u64::from_le_bytes(
            bytes[FILE_HEADER_LEN..FILE_HEADER_LEN + 8]
                .try_into()
                .expect("8 bytes"),
        ) != generation,
    );
    let at = FILE_HEADER_LEN + index * slot;
    let body = &mut bytes[at..at + slot];
    body[16..18].copy_from_slice(&flags.to_le_bytes());
    body[18..20].copy_from_slice(&mode.to_le_bytes());
    seal(body);
    std::fs::write(&path, &bytes).expect("rewrite RECLAIM");
}

/// Tear the *inactive* slot the way a half-finished in-place write
/// would: its CRC stops matching, so the reader falls back to the slot
/// the last complete commit left.
pub fn tear_inactive_slot(root: &Path) {
    let path = root.join(RECLAIM);
    let mut bytes = std::fs::read(&path).expect("read RECLAIM");
    let slot = stored_slot_len(&bytes);
    let (generation, _, _) = live_slot(&bytes);
    let live_is_first = u64::from_le_bytes(
        bytes[FILE_HEADER_LEN..FILE_HEADER_LEN + 8]
            .try_into()
            .expect("8 bytes"),
    ) == generation;
    let at = FILE_HEADER_LEN + usize::from(live_is_first) * slot;
    bytes[at + SLOT_HEADER_LEN] ^= 0xFF;
    std::fs::write(&path, &bytes).expect("rewrite RECLAIM");
}

/// A version-1 `CHECKPOINT` — the shape RFC0008.7's fixtures wrote and
/// the one §3.2 reads as a legitimate pre-RFC root.
pub fn write_legacy_checkpoint(root: &Path, offset: WalOffset) {
    let mut out = vec![0u8; 32];
    out[0..4].copy_from_slice(b"OWCK");
    out[4..6].copy_from_slice(&1u16.to_le_bytes());
    out[8..24].copy_from_slice(offset.segment.as_bytes());
    out[24..32].copy_from_slice(&offset.byte.to_le_bytes());
    std::fs::write(root.join(CHECKPOINT), &out).expect("write a version-1 CHECKPOINT");
}

pub fn checkpoint_version(root: &Path) -> u16 {
    let bytes = std::fs::read(root.join(CHECKPOINT)).expect("read CHECKPOINT");
    u16::from_le_bytes(bytes[4..6].try_into().expect("2 bytes"))
}

/// Rewrite every segment header's version field to 1 — a root whose
/// segments all predate RFC 0052. The header carries no checksum, so
/// the two bytes are the whole change.
pub fn downgrade_segments(root: &Path) {
    for path in segment_files(root) {
        let mut bytes = std::fs::read(&path).expect("read segment");
        bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
        std::fs::write(&path, &bytes).expect("rewrite segment header");
    }
}

pub fn segment_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = std::fs::read_dir(root)
        .expect("read_dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "wal"))
        .collect();
    out.sort();
    out
}

/// Mint a closed segment holding one `OtlpBatch` frame per payload in
/// a scratch root, then move it — and, on the first call, the record
/// the producing root created — into `dest_root`. Returns the frames'
/// append offsets.
pub fn build_closed_segment(dest_root: &Path, payloads: &[&[u8]]) -> Vec<WalOffset> {
    let scratch = tempfile::TempDir::new().expect("scratch root");
    let mut wal = open(scratch.path());
    let offsets = payloads
        .iter()
        .map(|p| wal.append(FrameKind::OtlpBatch, p).expect("append"))
        .collect();
    wal.sync().expect("sync");
    drop(wal);
    let seg = segment_files(scratch.path())
        .into_iter()
        .next()
        .expect("scratch holds one segment");
    std::fs::create_dir_all(dest_root).expect("dest root");
    let dest = dest_root.join(seg.file_name().expect("segment file name"));
    std::fs::rename(&seg, &dest).expect("move segment into dest root");
    let record = dest_root.join(RECLAIM);
    if !record.exists() {
        std::fs::copy(scratch.path().join(RECLAIM), &record).expect("bring the record along");
    }
    offsets
}

/// Truncate a segment's 24 B header so it no longer reads — the shape
/// a pre-RFC rotation left when its header fsync failed after the file
/// was already named `.wal`.
pub fn truncate_segment_header(path: &Path) {
    let bytes = std::fs::read(path).expect("read segment");
    std::fs::write(path, &bytes[..SEGMENT_HEADER_LEN / 2]).expect("truncate the header");
}

/// The same as [`build_closed_segment`], with `TenantOtlpBatch`
/// frames: RFC 0052 §3.2's tenant-aware retain rule is driven by the
/// membership only that kind carries. Returns each frame's offset in
/// the order given.
pub fn build_tenant_segment(dest_root: &Path, frames: &[(&str, &[u8])]) -> Vec<WalOffset> {
    let scratch = tempfile::TempDir::new().expect("scratch root");
    let mut wal = open(scratch.path());
    let offsets = frames
        .iter()
        .map(|(tenant, body)| {
            let payload = ourios_wal::TenantBatch::encode(tenant, body).expect("encode");
            wal.append(FrameKind::TenantOtlpBatch, &payload)
                .expect("append")
        })
        .collect();
    wal.sync().expect("sync");
    drop(wal);
    let seg = segment_files(scratch.path())
        .into_iter()
        .next()
        .expect("scratch holds one segment");
    std::fs::create_dir_all(dest_root).expect("dest root");
    let dest = dest_root.join(seg.file_name().expect("segment file name"));
    std::fs::rename(&seg, &dest).expect("move segment into dest root");
    let record = dest_root.join(RECLAIM);
    if !record.exists() {
        std::fs::copy(scratch.path().join(RECLAIM), &record).expect("bring the record along");
    }
    offsets
}

/// The horizons of a caller that holds miner state: every named
/// tenant restorable at its mark, every unnamed one pinned.
pub fn known(marks: &[(&str, WalOffset)]) -> ourios_wal::SnapshotHorizons {
    ourios_wal::SnapshotHorizons::restorable(
        marks
            .iter()
            .map(|(tenant, offset)| (tenant_id(tenant), *offset)),
    )
}

pub fn tenant_id(name: &str) -> ourios_core::tenant::TenantId {
    ourios_core::tenant::TenantId::try_new(name).expect("tenant id")
}

/// Rotation debris a previous process left: `<uuid>.wal.partial` is
/// the reserved shape §3.3's sweep pops.
pub fn write_partial(root: &Path) -> std::path::PathBuf {
    let path = root.join(format!("{}.wal.partial", uuid::Uuid::now_v7()));
    std::fs::write(&path, b"rotation debris").expect("write a partial");
    path
}

/// The live slot's `reclaimed_through` entries, keyed by tenant, read
/// straight out of §3.2's offset table: entry `i` belongs to
/// dictionary record `i`, so the position **is** the identity and
/// nothing is walked by a count.
pub fn reclaimed_through(root: &Path) -> std::collections::BTreeMap<String, WalOffset> {
    let bytes = std::fs::read(root.join(RECLAIM)).expect("read RECLAIM");
    let slot = live_slot_bytes(&bytes);
    let max_tenants = usize::try_from(u32::from_le_bytes(
        bytes[16..20].try_into().expect("4 bytes"),
    ))
    .expect("capacity fits usize");
    let entries_at = SLOT_HEADER_LEN + DICT_RECORD_LEN * max_tenants;
    let mut out = std::collections::BTreeMap::new();
    for index in 0..max_tenants {
        let dict = &slot[SLOT_HEADER_LEN + DICT_RECORD_LEN * index..][..DICT_RECORD_LEN];
        let len = usize::from(u16::from_le_bytes(dict[0..2].try_into().expect("2 bytes")));
        if len == 0 {
            continue;
        }
        let entry = &slot[entries_at + ENTRY_LEN * index..][..ENTRY_LEN];
        if u16::from_le_bytes(entry[0..2].try_into().expect("2 bytes")) & 1 == 0 {
            continue;
        }
        let key = String::from_utf8(dict[2..2 + len].to_vec()).expect("ascii key");
        out.insert(key, read_offset(&entry[8..32]));
    }
    out
}

/// One `planned` record as §3.2 stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedRow {
    pub segment: uuid::Uuid,
    /// §3.2's uncertain deletion: the unlink returned `Ok` but the
    /// parent fsync did not.
    pub uncertain: bool,
}

/// The live slot's `planned` records, oldest position first.
pub fn planned_unlinks(root: &Path) -> Vec<PlannedRow> {
    let bytes = std::fs::read(root.join(RECLAIM)).expect("read RECLAIM");
    let slot = live_slot_bytes(&bytes);
    let max_tenants = usize::try_from(u32::from_le_bytes(
        bytes[16..20].try_into().expect("4 bytes"),
    ))
    .expect("capacity fits usize");
    let max_unlinks = usize::try_from(u32::from_le_bytes(
        bytes[20..24].try_into().expect("4 bytes"),
    ))
    .expect("capacity fits usize");
    let planned_at = SLOT_HEADER_LEN + (DICT_RECORD_LEN + ENTRY_LEN) * max_tenants;
    let stride = PLANNED_HEADER_LEN + PAIR_LEN * max_tenants;
    let mut out = Vec::new();
    for index in 0..max_unlinks {
        let record = &slot[planned_at + stride * index..][..PLANNED_HEADER_LEN];
        // Bit 1 of the flags byte at offset 22 is `occupied`; a
        // position no segment occupies is a zeroed run.
        if record[22] & (1 << 1) == 0 {
            continue;
        }
        out.push(PlannedRow {
            segment: uuid::Uuid::from_slice(&record[0..16]).expect("16 bytes"),
            uncertain: record[22] & 1 != 0,
        });
    }
    out
}

/// The valid slot with the greater generation, as a reader picks it.
fn live_slot_bytes(bytes: &[u8]) -> &[u8] {
    let slot = stored_slot_len(bytes);
    let (generation, _, _) = live_slot(bytes);
    let first = u64::from_le_bytes(
        bytes[FILE_HEADER_LEN..FILE_HEADER_LEN + 8]
            .try_into()
            .expect("8 bytes"),
    );
    let index = usize::from(first != generation);
    &bytes[FILE_HEADER_LEN + index * slot..][..slot]
}

/// A `WalOffset` as §3.2 stores it: 16 B uuid in RFC 4122 order then
/// a little-endian `u64` byte.
fn read_offset(bytes: &[u8]) -> WalOffset {
    WalOffset {
        segment: uuid::Uuid::from_slice(&bytes[0..16]).expect("16 bytes"),
        byte: u64::from_le_bytes(bytes[16..24].try_into().expect("8 bytes")),
    }
}
