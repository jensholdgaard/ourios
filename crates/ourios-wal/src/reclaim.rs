//! `RECLAIM` sidecar codec (RFC 0052 §3.2).
//!
//! One 32 B file header, then two fixed-size slots. A slot is a whole
//! record — slot header, tenant dictionary, per-tenant entries, planned
//! unlinks, CRC32-C trailer — at fixed strides derived from the two
//! capacities the file was built for, so a position *is* an identity
//! and no count is ever walked as a length. A writer rewrites the
//! inactive slot in place; a reader takes the valid slot with the
//! greater generation, so a torn write leaves the previous slot live.
//!
//! Every multi-byte integer is little-endian, every reserved byte is
//! zero on write and refused when non-zero on read, and a UUID is its
//! 16 bytes in RFC 4122 order — the same conventions as the segment
//! header and the `CHECKPOINT` sidecar.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use ourios_core::tenant::{MAX_TENANT_BYTES, TenantId};
use uuid::Uuid;

use crate::WalOffset;

pub(crate) const SIDECAR_NAME: &str = "RECLAIM";
pub(crate) const REBUILD_NAME: &str = "RECLAIM.new";
const MAGIC: [u8; 4] = *b"OWRC";
const VERSION: u16 = 1;

/// The file header's length as an array size; [`FILE_HEADER_LEN`] is
/// the same number where offsets are computed.
const FILE_HEADER_BYTES: usize = 32;
pub(crate) const FILE_HEADER_LEN: u64 = 32;
const FILE_HEADER_CRC_COVERS: usize = 24;
const SLOT_HEADER_LEN: u64 = 24;
const SLOT_TRAILER_LEN: u64 = 8;
const KEY_LEN: usize = MAX_TENANT_BYTES;
const DICT_RECORD_LEN: u64 = 132;
// §3.2 pins the record at `u16 len`, the key padded to `KEY_LEN`, then
// `u16 flags` at offset 130. Widening the tenant bound moves those
// offsets, which is a format change; it fails here rather than letting
// a key run into the flags field.
const _: () = assert!(2 + KEY_LEN + 2 == 132);
const ENTRY_LEN: u64 = 32;
const PLANNED_HEADER_LEN: u64 = 24;
const PAIR_LEN: u64 = 32;
const OFFSET_LEN: usize = 24;
pub(crate) const SLOT_COUNT: u64 = 2;

/// The format ceiling on `max_tenants`: slot ids are `u16` and id 0 is
/// usable, so 65,536 records address the whole space.
pub const MAX_TENANTS_CEILING: u32 = 65_536;
/// The format ceiling on `max_unlinks_per_pass` (RFC 0052 §3.8).
pub const MAX_UNLINKS_PER_PASS_CEILING: u32 = 65_536;

const HEADER_FLAGS_MASK: u16 = 0b1111;
const CHECKPOINT_ARMED: u16 = 1 << 0;
const CHECKPOINT_SEEN: u16 = 1 << 1;
const PUBLISHED_SEEDED_ARMED: u16 = 1 << 2;
const PUBLISHED_SEEDED_CONFIRMED: u16 = 1 << 3;

const DICT_TOMBSTONED: u16 = 1 << 0;
const ENTRY_OCCUPIED: u16 = 1 << 0;
const PLANNED_UNCERTAIN: u8 = 1 << 0;
const PLANNED_OCCUPIED: u8 = 1 << 1;
const PAIR_PRESENT: u16 = 1 << 0;

/// The two capacities a `RECLAIM` file is built for, and every offset
/// that follows from them. Construction validates the format ceilings;
/// the strides are fixed for the life of the file (§3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    max_tenants: u32,
    max_unlinks_per_pass: u32,
}

/// Why two capacities cannot describe a `RECLAIM` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeometryError {
    /// `max_tenants` outside `1..=65_536`.
    MaxTenants { found: u32 },
    /// `max_unlinks_per_pass` outside `1..=65_536`.
    MaxUnlinksPerPass { found: u32 },
    /// The file the two capacities describe does not fit this
    /// platform's address space.
    Unrepresentable { file_len: u64 },
}

impl fmt::Display for GeometryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MaxTenants { found } => write!(
                f,
                "max_tenants {found} outside 1..={MAX_TENANTS_CEILING} (RECLAIM addresses tenants by u16 slot id)"
            ),
            Self::MaxUnlinksPerPass { found } => write!(
                f,
                "max_unlinks_per_pass {found} outside 1..={MAX_UNLINKS_PER_PASS_CEILING}"
            ),
            Self::Unrepresentable { file_len } => {
                write!(f, "a {file_len} B RECLAIM file is not addressable here")
            }
        }
    }
}

impl std::error::Error for GeometryError {}

impl Geometry {
    /// # Errors
    ///
    /// [`GeometryError`] when either capacity is outside its ceiling.
    pub fn new(max_tenants: u32, max_unlinks_per_pass: u32) -> Result<Self, GeometryError> {
        if !(1..=MAX_TENANTS_CEILING).contains(&max_tenants) {
            return Err(GeometryError::MaxTenants { found: max_tenants });
        }
        if !(1..=MAX_UNLINKS_PER_PASS_CEILING).contains(&max_unlinks_per_pass) {
            return Err(GeometryError::MaxUnlinksPerPass {
                found: max_unlinks_per_pass,
            });
        }
        let geometry = Self {
            max_tenants,
            max_unlinks_per_pass,
        };
        let file_len = geometry.file_len();
        if usize::try_from(file_len).is_err() {
            return Err(GeometryError::Unrepresentable { file_len });
        }
        Ok(geometry)
    }

    #[must_use]
    pub fn max_tenants(self) -> u32 {
        self.max_tenants
    }

    #[must_use]
    pub fn max_unlinks_per_pass(self) -> u32 {
        self.max_unlinks_per_pass
    }

    /// Whether a file built at `self` can hold everything a file built
    /// at `needed` can — the open-time "rebuild or open in place" test.
    #[must_use]
    pub fn covers(self, needed: Self) -> bool {
        self.max_tenants >= needed.max_tenants
            && self.max_unlinks_per_pass >= needed.max_unlinks_per_pass
    }

    fn tenants(self) -> u64 {
        u64::from(self.max_tenants)
    }

    fn unlinks(self) -> u64 {
        u64::from(self.max_unlinks_per_pass)
    }

    /// `32 + 164 × T + 24 × U + 32 × T × U` — the one normative
    /// definition of `slot_len` (§3.2). At the ceilings this is below
    /// 2^38, so the arithmetic cannot overflow a `u64`.
    #[must_use]
    pub fn slot_len(self) -> u64 {
        SLOT_HEADER_LEN
            + SLOT_TRAILER_LEN
            + (DICT_RECORD_LEN + ENTRY_LEN) * self.tenants()
            + self.planned_stride() * self.unlinks()
    }

    #[must_use]
    pub fn file_len(self) -> u64 {
        FILE_HEADER_LEN + SLOT_COUNT * self.slot_len()
    }

    pub(crate) fn slot_offset(self, slot: SlotIndex) -> u64 {
        FILE_HEADER_LEN + slot.ordinal() * self.slot_len()
    }

    fn dictionary_at(id: SlotId) -> usize {
        to_usize(SLOT_HEADER_LEN + DICT_RECORD_LEN * u64::from(id.0))
    }

    fn entry_at(self, id: SlotId) -> usize {
        to_usize(SLOT_HEADER_LEN + DICT_RECORD_LEN * self.tenants() + ENTRY_LEN * u64::from(id.0))
    }

    fn planned_at(self, position: usize) -> usize {
        let base = SLOT_HEADER_LEN + (DICT_RECORD_LEN + ENTRY_LEN) * self.tenants();
        to_usize(base) + position * to_usize(self.planned_stride())
    }

    fn pair_at(self, position: usize, id: SlotId) -> usize {
        self.planned_at(position) + to_usize(PLANNED_HEADER_LEN + PAIR_LEN * u64::from(id.0))
    }

    fn planned_stride(self) -> u64 {
        PLANNED_HEADER_LEN + PAIR_LEN * self.tenants()
    }

    fn trailer_offset(self) -> usize {
        to_usize(self.slot_len() - SLOT_TRAILER_LEN)
    }

    /// Every slot id the geometry addresses, in order.
    fn ids(self) -> impl Iterator<Item = SlotId> {
        (0..to_usize(self.tenants())).filter_map(|index| u16::try_from(index).ok().map(SlotId))
    }
}

/// Which of the two slots a record lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotIndex {
    First,
    Second,
}

impl SlotIndex {
    #[must_use]
    pub fn other(self) -> Self {
        match self {
            Self::First => Self::Second,
            Self::Second => Self::First,
        }
    }

    fn ordinal(self) -> u64 {
        match self {
            Self::First => 0,
            Self::Second => 1,
        }
    }
}

/// A tenant's position in the dictionary — its identity in both
/// sidecars for the life of the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotId(u16);

impl SlotId {
    #[must_use]
    pub fn get(self) -> u16 {
        self.0
    }

    fn index(self) -> usize {
        usize::from(self.0)
    }
}

impl fmt::Display for SlotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A two-state witness (§3.2): `Armed` is written durably before the
/// first attempt at the thing witnessed, the terminal state at the
/// next record write after that attempt succeeded, and the terminal
/// state never clears. On disk the terminal state keeps the armed bit
/// set, so "seen without armed" is not a value this type can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Witness {
    #[default]
    Unarmed,
    Armed,
    /// `checkpoint_seen` / `published_seeded_confirmed`.
    Terminal,
}

impl Witness {
    fn bits(self, armed: u16, terminal: u16) -> u16 {
        match self {
            Self::Unarmed => 0,
            Self::Armed => armed,
            Self::Terminal => armed | terminal,
        }
    }

    fn from_bits(bits: u16, armed: u16, terminal: u16) -> Result<Self, FormatError> {
        match (bits & armed != 0, bits & terminal != 0) {
            (false, false) => Ok(Self::Unarmed),
            (true, false) => Ok(Self::Armed),
            (true, true) => Ok(Self::Terminal),
            (false, true) => Err(FormatError::WitnessOrder { found: bits }),
        }
    }
}

/// The witness bits of a slot's `header_flags`: the checkpoint pair
/// this RFC defines and the `PUBLISHED` seeding pair RFC 0053 adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WitnessFlags {
    pub checkpoint: Witness,
    pub published_seeding: Witness,
}

impl WitnessFlags {
    fn bits(self) -> u16 {
        self.checkpoint.bits(CHECKPOINT_ARMED, CHECKPOINT_SEEN)
            | self
                .published_seeding
                .bits(PUBLISHED_SEEDED_ARMED, PUBLISHED_SEEDED_CONFIRMED)
    }

    fn from_bits(bits: u16) -> Result<Self, FormatError> {
        if bits & !HEADER_FLAGS_MASK != 0 {
            return Err(FormatError::ReservedHeaderFlags { found: bits });
        }
        Ok(Self {
            checkpoint: Witness::from_bits(bits, CHECKPOINT_ARMED, CHECKPOINT_SEEN)?,
            published_seeding: Witness::from_bits(
                bits,
                PUBLISHED_SEEDED_ARMED,
                PUBLISHED_SEEDED_CONFIRMED,
            )?,
        })
    }
}

/// The consumer mode every pass on this root ran under (§3.2).
/// "Unrecorded" is a value, not an absence: a record is created with
/// it and the first pass adopts its own mode durably.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecordedMode {
    #[default]
    Unrecorded,
    Known,
    NoConsumer,
}

impl RecordedMode {
    /// Whether an entry written under `entry` can sit in a record whose
    /// root recorded `self`: every pass on a root runs under the
    /// recorded mode, and no pass runs under an unrecorded one.
    #[must_use]
    pub fn admits(self, entry: EntryMode) -> bool {
        matches!(
            (self, entry),
            (Self::Known, EntryMode::Known) | (Self::NoConsumer, EntryMode::NoConsumer)
        )
    }

    fn code(self) -> u16 {
        match self {
            Self::Unrecorded => 0,
            Self::Known => 1,
            Self::NoConsumer => 2,
        }
    }

    fn from_code(code: u16) -> Result<Self, FormatError> {
        match code {
            0 => Ok(Self::Unrecorded),
            1 => Ok(Self::Known),
            2 => Ok(Self::NoConsumer),
            found => Err(FormatError::UnknownMode {
                field: "consumer_mode",
                found,
            }),
        }
    }
}

/// The mode an entry was written under. Unlike the root's recorded
/// mode there is no unrecorded value: an entry exists only because a
/// pass with a mode wrote it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryMode {
    Known,
    NoConsumer,
}

impl EntryMode {
    fn code(self) -> u16 {
        match self {
            Self::Known => 1,
            Self::NoConsumer => 2,
        }
    }

    fn from_code(code: u16) -> Result<Self, FormatError> {
        match code {
            1 => Ok(Self::Known),
            2 => Ok(Self::NoConsumer),
            found => Err(FormatError::UnknownMode {
                field: "entry mode",
                found,
            }),
        }
    }
}

/// A tenant's `reclaimed_through`: the proof that frames at or below
/// `offset` are gone, and the mode that made reclaiming them sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub mode: EntryMode,
    pub offset: WalOffset,
}

/// What a dictionary position says about its tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotState {
    /// A recorded tenant; `None` means nothing of its has been
    /// reclaimed.
    Live { reclaimed_through: Option<Entry> },
    /// A removed tenant. Its key stays so the id stays retired; it can
    /// carry no entry.
    Tombstoned,
}

/// One dictionary record: the tenant a slot id names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictRecord {
    pub key: TenantId,
    pub state: SlotState,
}

impl DictRecord {
    fn is_live(&self) -> bool {
        matches!(self.state, SlotState::Live { .. })
    }
}

/// A tenant's key and whether it is tombstoned — the part of a
/// dictionary record both sidecars carry, used to seed the shared id
/// space from the union of the two (§3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedKey {
    pub key: TenantId,
    pub tombstoned: bool,
}

/// The per-slot tenant dictionary: a dense prefix of records whose
/// position is the slot id, so `next_slot_id` is its length. Ids are
/// assigned upward, never renumbered and never reused.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Dictionary {
    records: Vec<DictRecord>,
}

/// The two dictionaries name one id with different keys — a state no
/// correct writer produces, refused rather than read past (§3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictionaryConflict {
    pub id: SlotId,
    pub left: TenantId,
    pub right: TenantId,
}

impl fmt::Display for DictionaryConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "slot id {} names {:?} in one dictionary and {:?} in the other",
            self.id, self.left, self.right
        )
    }
}

impl std::error::Error for DictionaryConflict {}

/// Every id the geometry allows is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DictionaryFull {
    pub max_tenants: u32,
}

impl fmt::Display for DictionaryFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RECLAIM dictionary holds max_tenants = {} records; a tombstoned id is never reused",
            self.max_tenants
        )
    }
}

impl std::error::Error for DictionaryFull {}

impl Dictionary {
    #[must_use]
    pub fn next_slot_id(&self) -> u32 {
        // The dictionary never exceeds `MAX_TENANTS_CEILING` records, so
        // its length fits.
        u32::try_from(self.records.len()).unwrap_or(u32::MAX)
    }

    #[must_use]
    pub fn get(&self, id: SlotId) -> Option<&DictRecord> {
        self.records.get(id.index())
    }

    pub fn get_mut(&mut self, id: SlotId) -> Option<&mut DictRecord> {
        self.records.get_mut(id.index())
    }

    /// Whether `id` names a recorded, non-tombstoned tenant.
    #[must_use]
    pub fn is_live(&self, id: SlotId) -> bool {
        self.get(id).is_some_and(DictRecord::is_live)
    }

    /// The live id of `key`, if it holds one. A tombstoned record never
    /// answers: its tenant is gone and its id retired.
    #[must_use]
    pub fn id_of(&self, key: &TenantId) -> Option<SlotId> {
        self.records
            .iter()
            .position(|record| record.key == *key && record.is_live())
            .and_then(|index| u16::try_from(index).ok())
            .map(SlotId)
    }

    /// Every live record with its id, in id order.
    pub fn live(&self) -> impl Iterator<Item = (SlotId, &DictRecord)> {
        self.iter().filter(|(_, record)| record.is_live())
    }

    /// Every record with its id, in id order.
    pub fn iter(&self) -> impl Iterator<Item = (SlotId, &DictRecord)> {
        self.records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| u16::try_from(index).ok().map(|id| (SlotId(id), record)))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The id `key` holds, assigning the next one when it holds none.
    ///
    /// # Errors
    ///
    /// [`DictionaryFull`] when every id the geometry allows is taken.
    pub fn assign(&mut self, key: &TenantId, geometry: Geometry) -> Result<SlotId, DictionaryFull> {
        if let Some(id) = self.id_of(key) {
            return Ok(id);
        }
        let next = self.records.len();
        let full = DictionaryFull {
            max_tenants: geometry.max_tenants,
        };
        if next >= to_usize(geometry.tenants()) {
            return Err(full);
        }
        let id = u16::try_from(next).map(SlotId).map_err(|_| full)?;
        self.records.push(DictRecord {
            key: key.clone(),
            state: SlotState::Live {
                reclaimed_through: None,
            },
        });
        Ok(id)
    }

    /// Retire `id`: the key stays, the entry is dropped, no later tenant
    /// takes the id. A tombstoned id is left as it is.
    pub fn tombstone(&mut self, id: SlotId) -> Result<(), UnknownSlotId> {
        match self.records.get_mut(id.index()) {
            Some(record) => {
                record.state = SlotState::Tombstoned;
                Ok(())
            }
            None => Err(UnknownSlotId { id }),
        }
    }

    /// The `(key, tombstoned)` view of every record, for seeding the
    /// other sidecar's table.
    #[must_use]
    pub fn seed_keys(&self) -> Vec<SeedKey> {
        self.records
            .iter()
            .map(|record| SeedKey {
                key: record.key.clone(),
                tombstoned: !record.is_live(),
            })
            .collect()
    }

    /// Seed the shared id space from this dictionary and the other
    /// sidecar's keys (§3.2): an id named by either is reserved, an id
    /// named by both must agree on its key, a tombstone in either
    /// retires the id, and this dictionary's entries are kept.
    ///
    /// # Errors
    ///
    /// [`DictionaryConflict`] when both name one id with different keys.
    pub fn union(&self, other: &[SeedKey]) -> Result<Self, DictionaryConflict> {
        let mut records = self.records.clone();
        for (index, seed) in other.iter().enumerate() {
            match records.get_mut(index) {
                Some(record) if record.key != seed.key => {
                    let id = u16::try_from(index).map_or(SlotId(u16::MAX), SlotId);
                    return Err(DictionaryConflict {
                        id,
                        left: record.key.clone(),
                        right: seed.key.clone(),
                    });
                }
                Some(record) => {
                    if seed.tombstoned {
                        record.state = SlotState::Tombstoned;
                    }
                }
                None => records.push(DictRecord {
                    key: seed.key.clone(),
                    state: if seed.tombstoned {
                        SlotState::Tombstoned
                    } else {
                        SlotState::Live {
                            reclaimed_through: None,
                        }
                    },
                }),
            }
        }
        Ok(Self { records })
    }
}

/// An id no dictionary record holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownSlotId {
    pub id: SlotId,
}

impl fmt::Display for UnknownSlotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "slot id {} is not in the dictionary", self.id)
    }
}

impl std::error::Error for UnknownSlotId {}

/// One popped segment the record promises to unlink: its uuid and each
/// tenant's last offset in it, written before the unlinks so open can
/// reconcile a crash between the two (§3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedUnlink {
    pub segment: Uuid,
    /// The parent fsync after this segment's unlink failed, so whether
    /// the entry survives a restart is unknown until re-verified.
    pub uncertain: bool,
    pub last_offsets: BTreeMap<SlotId, WalOffset>,
}

/// One slot's content: what a pass writes and open reads back.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReclaimRecord {
    pub witness: WitnessFlags,
    pub consumer_mode: RecordedMode,
    pub dictionary: Dictionary,
    pub planned: Vec<PlannedUnlink>,
}

/// A slot that decoded: its generation and the record it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedSlot {
    pub generation: u64,
    pub record: ReclaimRecord,
}

/// Why a record cannot be laid out at a geometry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// The caller's buffer is not exactly one slot.
    BufferLength { found: usize, slot_len: u64 },
    /// More dictionary records than `max_tenants`.
    TooManyTenants { found: usize, max_tenants: u32 },
    /// More planned records than `max_unlinks_per_pass`.
    TooManyPlanned {
        found: usize,
        max_unlinks_per_pass: u32,
    },
    /// A planned record's present pairs exceed what its `u16`
    /// `tenant_count` can report.
    TooManyPairs { segment: Uuid, found: usize },
    /// A planned pair names an id that is not live in the dictionary.
    PairWithoutTenant { segment: Uuid, id: SlotId },
    /// A planned pair's offset lies in a segment other than the one
    /// the record names.
    PairSegment {
        segment: Uuid,
        id: SlotId,
        found: Uuid,
    },
    /// An entry's mode is not the mode the root recorded.
    EntryModeDisagrees {
        id: SlotId,
        entry: EntryMode,
        recorded: RecordedMode,
    },
    /// Planned records beside an unrecorded mode: no pass can have
    /// written them.
    PlannedWithoutMode,
    /// A dictionary key is empty or longer than `KEY_LEN`.
    KeyLength { id: SlotId, found: usize },
    /// A generation of zero marks a slot never written.
    ZeroGeneration,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BufferLength { found, slot_len } => {
                write!(f, "buffer is {found} B, a slot is {slot_len} B")
            }
            Self::TooManyTenants { found, max_tenants } => write!(
                f,
                "{found} dictionary records exceed max_tenants {max_tenants}"
            ),
            Self::TooManyPlanned {
                found,
                max_unlinks_per_pass,
            } => write!(
                f,
                "{found} planned records exceed max_unlinks_per_pass {max_unlinks_per_pass}"
            ),
            Self::TooManyPairs { segment, found } => write!(
                f,
                "planned segment {segment} carries {found} pairs; tenant_count reports at most {}",
                u16::MAX
            ),
            Self::PairWithoutTenant { segment, id } => write!(
                f,
                "planned segment {segment} carries a pair for slot id {id} which is not live"
            ),
            Self::PairSegment { segment, id, found } => write!(
                f,
                "planned segment {segment} pair {id} names offset segment {found}"
            ),
            Self::EntryModeDisagrees {
                id,
                entry,
                recorded,
            } => write!(
                f,
                "entry {id} was written under {entry:?} but the root recorded {recorded:?}"
            ),
            Self::PlannedWithoutMode => {
                f.write_str("planned records beside an unrecorded consumer mode")
            }
            Self::KeyLength { id, found } => {
                write!(f, "slot id {id} key is {found} B; a key is 1..={KEY_LEN} B")
            }
            Self::ZeroGeneration => f.write_str("generation 0 marks a slot never written"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// Why bytes are not a `RECLAIM` file header or slot. Every arm names
/// where, so the `OpenError::Corrupt` detail an operator reads says
/// what was found rather than only that something was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    Length {
        found: usize,
        expected: u64,
    },
    BadMagic {
        found: [u8; 4],
    },
    UnknownVersion {
        found: u16,
    },
    Reserved {
        field: &'static str,
        at: usize,
    },
    CrcMismatch {
        found: u32,
        computed: u32,
    },
    SlotLen {
        stored: u64,
        computed: u64,
    },
    Capacity(GeometryError),
    ReservedHeaderFlags {
        found: u16,
    },
    /// A terminal witness bit without its armed bit.
    WitnessOrder {
        found: u16,
    },
    UnknownMode {
        field: &'static str,
        found: u16,
    },
    NextSlotId {
        found: u32,
        max_tenants: u32,
    },
    DictionaryHole {
        id: SlotId,
    },
    DictionaryPastNext {
        id: SlotId,
        next_slot_id: u32,
    },
    KeyLength {
        id: SlotId,
        found: usize,
    },
    KeyNotUtf8 {
        id: SlotId,
    },
    DictionaryFlags {
        id: SlotId,
        found: u16,
    },
    EntryFlags {
        id: SlotId,
        found: u16,
    },
    EntryWithoutTenant {
        id: SlotId,
    },
    /// An entry's mode is not the mode the slot header recorded.
    EntryModeDisagrees {
        id: SlotId,
        entry: EntryMode,
        recorded: RecordedMode,
    },
    EntryCount {
        stored: u32,
        found: u32,
    },
    /// Two live records name one tenant, so its id is ambiguous.
    DuplicateKey {
        first: SlotId,
        id: SlotId,
        key: TenantId,
    },
    /// Planned records beside an unrecorded mode.
    PlannedWithoutMode {
        found: u32,
    },
    PlannedFlags {
        position: usize,
        found: u8,
    },
    PlannedCount {
        stored: u32,
        found: u32,
    },
    PairFlags {
        position: usize,
        id: SlotId,
        found: u16,
    },
    PairWithoutTenant {
        position: usize,
        id: SlotId,
    },
    /// A pair's offset lies in a segment other than its record's.
    PairSegment {
        position: usize,
        id: SlotId,
        found: Uuid,
    },
    TenantCount {
        position: usize,
        stored: u16,
        found: usize,
    },
    ZeroGeneration,
    /// Neither slot decoded.
    NoValidSlot {
        first: Box<FormatError>,
        second: Box<FormatError>,
    },
    /// Both slots decoded at the same generation, which one writer
    /// alternating slots cannot produce.
    EqualGenerations {
        generation: u64,
    },
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DictionaryHole { .. }
            | Self::DictionaryPastNext { .. }
            | Self::KeyLength { .. }
            | Self::KeyNotUtf8 { .. }
            | Self::DictionaryFlags { .. }
            | Self::EntryFlags { .. }
            | Self::EntryWithoutTenant { .. }
            | Self::EntryModeDisagrees { .. }
            | Self::EntryCount { .. }
            | Self::DuplicateKey { .. } => self.fmt_dictionary(f),
            Self::PlannedWithoutMode { .. }
            | Self::PlannedFlags { .. }
            | Self::PlannedCount { .. }
            | Self::PairFlags { .. }
            | Self::PairWithoutTenant { .. }
            | Self::PairSegment { .. }
            | Self::TenantCount { .. } => self.fmt_planned(f),
            _ => self.fmt_header(f),
        }
    }
}

impl FormatError {
    /// The file-header, slot-header and slot-selection arms. A variant
    /// routed here by mistake renders as its `Debug` form rather than
    /// silently as nothing.
    fn fmt_header(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length { found, expected } => write!(f, "size {found} B, expected {expected} B"),
            Self::BadMagic { found } => write!(f, "bad magic {found:02x?}, expected {MAGIC:02x?}"),
            Self::UnknownVersion { found } => {
                write!(f, "unknown version {found}, expected {VERSION}")
            }
            Self::Reserved { field, at } => {
                write!(f, "non-zero reserved bytes in {field} at offset {at}")
            }
            Self::CrcMismatch { found, computed } => {
                write!(f, "CRC32-C {found:#010x} stored, {computed:#010x} computed")
            }
            Self::SlotLen { stored, computed } => write!(
                f,
                "slot_len {stored} disagrees with the {computed} the stored capacities give"
            ),
            Self::Capacity(e) => write!(f, "stored capacities: {e}"),
            Self::ReservedHeaderFlags { found } => {
                write!(f, "header_flags {found:#06x} sets a reserved bit")
            }
            Self::WitnessOrder { found } => write!(
                f,
                "header_flags {found:#06x} carries a terminal witness bit without its armed bit"
            ),
            Self::UnknownMode { field, found } => write!(f, "{field} {found} is not a mode"),
            Self::NextSlotId { found, max_tenants } => {
                write!(f, "next_slot_id {found} exceeds max_tenants {max_tenants}")
            }
            Self::ZeroGeneration => f.write_str("generation 0: the slot was never written"),
            Self::NoValidSlot { first, second } => {
                write!(
                    f,
                    "neither slot is valid (first: {first}; second: {second})"
                )
            }
            Self::EqualGenerations { generation } => {
                write!(f, "both slots carry generation {generation}")
            }
            other => write!(f, "{other:?}"),
        }
    }

    /// The dictionary and entry arms.
    fn fmt_dictionary(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DictionaryHole { id } => {
                write!(f, "dictionary record {id} is unused below next_slot_id")
            }
            Self::DictionaryPastNext { id, next_slot_id } => write!(
                f,
                "dictionary record {id} is used at or above next_slot_id {next_slot_id}"
            ),
            Self::KeyLength { id, found } => write!(
                f,
                "dictionary record {id} key length {found} outside 1..={KEY_LEN}"
            ),
            Self::KeyNotUtf8 { id } => write!(f, "dictionary record {id} key is not UTF-8"),
            Self::DictionaryFlags { id, found } => write!(
                f,
                "dictionary record {id} flags {found:#06x} sets a reserved bit"
            ),
            Self::EntryFlags { id, found } => {
                write!(f, "entry {id} flags {found:#06x} sets a reserved bit")
            }
            Self::EntryWithoutTenant { id } => write!(
                f,
                "entry {id} is occupied but its dictionary record is not live"
            ),
            Self::EntryModeDisagrees {
                id,
                entry,
                recorded,
            } => write!(
                f,
                "entry {id} was written under {entry:?} but the slot recorded {recorded:?}"
            ),
            Self::EntryCount { stored, found } => {
                write!(
                    f,
                    "entry_count {stored} stored, {found} occupied entries found"
                )
            }
            Self::DuplicateKey { first, id, key } => write!(
                f,
                "dictionary records {first} and {id} both name live tenant {key:?}"
            ),
            other => write!(f, "{other:?}"),
        }
    }

    /// The planned-record and pair arms.
    fn fmt_planned(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlannedWithoutMode { found } => {
                write!(
                    f,
                    "{found} planned records beside an unrecorded consumer mode"
                )
            }
            Self::PlannedFlags { position, found } => write!(
                f,
                "planned record {position} flags {found:#04x} sets a reserved bit"
            ),
            Self::PlannedCount { stored, found } => write!(
                f,
                "planned_count {stored} stored, {found} occupied planned records found"
            ),
            Self::PairFlags {
                position,
                id,
                found,
            } => write!(
                f,
                "planned record {position} pair {id} flags {found:#06x} sets a reserved bit"
            ),
            Self::PairWithoutTenant { position, id } => write!(
                f,
                "planned record {position} pair {id} is present but its dictionary record is not live"
            ),
            Self::PairSegment {
                position,
                id,
                found,
            } => write!(
                f,
                "planned record {position} pair {id} names offset segment {found}"
            ),
            Self::TenantCount {
                position,
                stored,
                found,
            } => write!(
                f,
                "planned record {position} tenant_count {stored} stored, {found} present pairs found"
            ),
            other => write!(f, "{other:?}"),
        }
    }
}

impl std::error::Error for FormatError {}

// ---------------------------------------------------------------
// File header
// ---------------------------------------------------------------

/// The 32 B file header for a file built at `geometry`.
#[must_use]
pub fn encode_file_header(geometry: Geometry) -> [u8; FILE_HEADER_BYTES] {
    let mut out = [0u8; FILE_HEADER_BYTES];
    out[0..4].copy_from_slice(&MAGIC);
    out[4..6].copy_from_slice(&VERSION.to_le_bytes());
    // [6..8] reserved.
    out[8..16].copy_from_slice(&geometry.slot_len().to_le_bytes());
    out[16..20].copy_from_slice(&geometry.max_tenants.to_le_bytes());
    out[20..24].copy_from_slice(&geometry.max_unlinks_per_pass.to_le_bytes());
    let crc = crc32c::crc32c(&out[..FILE_HEADER_CRC_COVERS]);
    out[24..28].copy_from_slice(&crc.to_le_bytes());
    // [28..32] reserved.
    out
}

/// The geometry a file header was written at, once every field checks.
///
/// # Errors
///
/// [`FormatError`] naming the first field that does not.
pub fn decode_file_header(bytes: &[u8]) -> Result<Geometry, FormatError> {
    if bytes.len() != FILE_HEADER_BYTES {
        return Err(FormatError::Length {
            found: bytes.len(),
            expected: FILE_HEADER_LEN,
        });
    }
    let mut magic = [0u8; 4];
    magic.copy_from_slice(&bytes[0..4]);
    if magic != MAGIC {
        return Err(FormatError::BadMagic { found: magic });
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != VERSION {
        return Err(FormatError::UnknownVersion { found: version });
    }
    reserved_zero(bytes, 6..8, "file header")?;
    let computed = crc32c::crc32c(&bytes[..FILE_HEADER_CRC_COVERS]);
    let found = read_u32(bytes, 24);
    if found != computed {
        return Err(FormatError::CrcMismatch { found, computed });
    }
    reserved_zero(bytes, 28..32, "file header")?;
    let stored = read_u64(bytes, 8);
    let geometry =
        Geometry::new(read_u32(bytes, 16), read_u32(bytes, 20)).map_err(FormatError::Capacity)?;
    let computed = geometry.slot_len();
    if stored != computed {
        return Err(FormatError::SlotLen { stored, computed });
    }
    Ok(geometry)
}

// ---------------------------------------------------------------
// Slot: encode
// ---------------------------------------------------------------

/// Lay `record` out as one `slot_len`-byte slot at `generation`.
///
/// # Errors
///
/// [`EncodeError`] when the record does not fit the geometry or names
/// an id the dictionary does not hold live.
pub fn encode_slot(
    record: &ReclaimRecord,
    generation: u64,
    geometry: Geometry,
) -> Result<Vec<u8>, EncodeError> {
    let mut out = vec![0u8; to_usize(geometry.slot_len())];
    encode_slot_into(record, generation, geometry, &mut out)?;
    Ok(out)
}

/// Lay `record` out into `out`, a caller-owned buffer of exactly
/// `slot_len` bytes that is zeroed first. The steady-state writer keeps
/// one such buffer for the life of the file, so a pass allocates
/// nothing (§3.2); [`encode_slot`] is the allocating convenience.
///
/// # Errors
///
/// [`EncodeError`] when the record does not fit the geometry, names an
/// id the dictionary does not hold live, or `out` is the wrong length.
pub fn encode_slot_into(
    record: &ReclaimRecord,
    generation: u64,
    geometry: Geometry,
    out: &mut [u8],
) -> Result<(), EncodeError> {
    if out.len() != to_usize(geometry.slot_len()) {
        return Err(EncodeError::BufferLength {
            found: out.len(),
            slot_len: geometry.slot_len(),
        });
    }
    if generation == 0 {
        return Err(EncodeError::ZeroGeneration);
    }
    let tenants = record.dictionary.len();
    if tenants > to_usize(geometry.tenants()) {
        return Err(EncodeError::TooManyTenants {
            found: tenants,
            max_tenants: geometry.max_tenants,
        });
    }
    if record.planned.len() > to_usize(geometry.unlinks()) {
        return Err(EncodeError::TooManyPlanned {
            found: record.planned.len(),
            max_unlinks_per_pass: geometry.max_unlinks_per_pass,
        });
    }
    if record.consumer_mode == RecordedMode::Unrecorded && !record.planned.is_empty() {
        return Err(EncodeError::PlannedWithoutMode);
    }
    out.fill(0);
    let mut writer = SlotWriter {
        out: &mut *out,
        geometry,
    };
    let entry_count = writer.dictionary(record)?;
    for (position, planned) in record.planned.iter().enumerate() {
        writer.planned(position, planned, &record.dictionary)?;
    }
    out[0..8].copy_from_slice(&generation.to_le_bytes());
    out[8..12].copy_from_slice(&entry_count.to_le_bytes());
    // `planned.len() <= max_unlinks_per_pass <= 65_536` fits a u32.
    let planned_count = u32::try_from(record.planned.len()).unwrap_or(u32::MAX);
    out[12..16].copy_from_slice(&planned_count.to_le_bytes());
    out[16..18].copy_from_slice(&record.witness.bits().to_le_bytes());
    out[18..20].copy_from_slice(&record.consumer_mode.code().to_le_bytes());
    out[20..24].copy_from_slice(&record.dictionary.next_slot_id().to_le_bytes());
    let trailer = geometry.trailer_offset();
    let crc = crc32c::crc32c(&out[..trailer]);
    out[trailer..trailer + 4].copy_from_slice(&crc.to_le_bytes());
    Ok(())
}

/// One slot being laid out: the buffer and the geometry that says
/// where each structure lives.
struct SlotWriter<'a> {
    out: &'a mut [u8],
    geometry: Geometry,
}

impl SlotWriter<'_> {
    /// The dictionary and entry arrays; returns the occupied-entry
    /// count. An entry is written only under the mode the root
    /// recorded, since recovery reads it by that mode (§3.2).
    fn dictionary(&mut self, record: &ReclaimRecord) -> Result<u32, EncodeError> {
        let mut entry_count = 0u32;
        for (id, dict) in record.dictionary.iter() {
            encode_dict_record(&mut self.out[Geometry::dictionary_at(id)..], id, dict)?;
            entry_count += self.entry(id, dict, record.consumer_mode)?;
        }
        Ok(entry_count)
    }

    /// One position's entry, if its record carries one; returns how
    /// many entries were written (0 or 1).
    fn entry(
        &mut self,
        id: SlotId,
        dict: &DictRecord,
        recorded: RecordedMode,
    ) -> Result<u32, EncodeError> {
        let SlotState::Live {
            reclaimed_through: Some(entry),
        } = dict.state
        else {
            return Ok(0);
        };
        if !recorded.admits(entry.mode) {
            return Err(EncodeError::EntryModeDisagrees {
                id,
                entry: entry.mode,
                recorded,
            });
        }
        let at = self.geometry.entry_at(id);
        self.out[at..at + 2].copy_from_slice(&ENTRY_OCCUPIED.to_le_bytes());
        self.out[at + 2..at + 4].copy_from_slice(&entry.mode.code().to_le_bytes());
        write_offset(&mut self.out[at + 8..at + 8 + OFFSET_LEN], entry.offset);
        Ok(1)
    }

    /// One planned record and its pairs.
    fn planned(
        &mut self,
        position: usize,
        planned: &PlannedUnlink,
        dictionary: &Dictionary,
    ) -> Result<(), EncodeError> {
        check_pairs(planned, dictionary)?;
        let at = self.geometry.planned_at(position);
        self.out[at..at + 16].copy_from_slice(planned.segment.as_bytes());
        let tenant_count =
            u16::try_from(planned.last_offsets.len()).map_err(|_| EncodeError::TooManyPairs {
                segment: planned.segment,
                found: planned.last_offsets.len(),
            })?;
        self.out[at + 16..at + 18].copy_from_slice(&tenant_count.to_le_bytes());
        let uncertain = if planned.uncertain {
            PLANNED_UNCERTAIN
        } else {
            0
        };
        self.out[at + 22] = PLANNED_OCCUPIED | uncertain;
        for (&id, &offset) in &planned.last_offsets {
            let at = self.geometry.pair_at(position, id);
            self.out[at..at + 2].copy_from_slice(&PAIR_PRESENT.to_le_bytes());
            write_offset(&mut self.out[at + 8..at + 8 + OFFSET_LEN], offset);
        }
        Ok(())
    }
}

fn encode_dict_record(out: &mut [u8], id: SlotId, dict: &DictRecord) -> Result<(), EncodeError> {
    let key = dict.key.as_str().as_bytes();
    if key.is_empty() || key.len() > KEY_LEN {
        return Err(EncodeError::KeyLength {
            id,
            found: key.len(),
        });
    }
    // `key.len() <= KEY_LEN == 128` fits a u16.
    let len = u16::try_from(key.len()).unwrap_or(u16::MAX);
    out[0..2].copy_from_slice(&len.to_le_bytes());
    out[2..2 + key.len()].copy_from_slice(key);
    let flags = if dict.is_live() { 0 } else { DICT_TOMBSTONED };
    out[130..132].copy_from_slice(&flags.to_le_bytes());
    Ok(())
}

/// Every pair names a live tenant and lies in its record's segment —
/// a pair is that tenant's last offset *in* that segment (§3.2).
fn check_pairs(planned: &PlannedUnlink, dictionary: &Dictionary) -> Result<(), EncodeError> {
    for (&id, &offset) in &planned.last_offsets {
        if !dictionary.is_live(id) {
            return Err(EncodeError::PairWithoutTenant {
                segment: planned.segment,
                id,
            });
        }
        if offset.segment != planned.segment {
            return Err(EncodeError::PairSegment {
                segment: planned.segment,
                id,
                found: offset.segment,
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------
// Slot: decode
// ---------------------------------------------------------------

/// Read one slot back. The CRC is checked before any field is read, so
/// a torn slot is refused whole rather than partly believed.
///
/// # Errors
///
/// [`FormatError`] naming the first check that fails.
pub fn decode_slot(bytes: &[u8], geometry: Geometry) -> Result<DecodedSlot, FormatError> {
    let slot_len = geometry.slot_len();
    if bytes.len() != to_usize(slot_len) {
        return Err(FormatError::Length {
            found: bytes.len(),
            expected: slot_len,
        });
    }
    let trailer = geometry.trailer_offset();
    let computed = crc32c::crc32c(&bytes[..trailer]);
    let found = read_u32(bytes, trailer);
    if found != computed {
        return Err(FormatError::CrcMismatch { found, computed });
    }
    reserved_zero(bytes, trailer + 4..trailer + 8, "slot trailer")?;
    let generation = read_u64(bytes, 0);
    if generation == 0 {
        return Err(FormatError::ZeroGeneration);
    }
    let witness = WitnessFlags::from_bits(read_u16(bytes, 16))?;
    let consumer_mode = RecordedMode::from_code(read_u16(bytes, 18))?;
    let next_slot_id = read_u32(bytes, 20);
    if next_slot_id > geometry.max_tenants {
        return Err(FormatError::NextSlotId {
            found: next_slot_id,
            max_tenants: geometry.max_tenants,
        });
    }
    let dictionary = decode_dictionary(bytes, geometry, next_slot_id, read_u32(bytes, 8))?;
    entries_agree_with(&dictionary, consumer_mode)?;
    let planned_count = read_u32(bytes, 12);
    if consumer_mode == RecordedMode::Unrecorded && planned_count != 0 {
        return Err(FormatError::PlannedWithoutMode {
            found: planned_count,
        });
    }
    let planned = decode_planned(bytes, geometry, &dictionary, planned_count)?;
    Ok(DecodedSlot {
        generation,
        record: ReclaimRecord {
            witness,
            consumer_mode,
            dictionary,
            planned,
        },
    })
}

/// The dictionary and entry arrays, read in lockstep since entry `i`
/// belongs to record `i`.
fn decode_dictionary(
    bytes: &[u8],
    geometry: Geometry,
    next_slot_id: u32,
    entry_count: u32,
) -> Result<Dictionary, FormatError> {
    let mut records = Vec::with_capacity(to_usize(u64::from(next_slot_id)));
    for id in geometry.ids() {
        let key = decode_dict_record(&bytes[Geometry::dictionary_at(id)..], id, next_slot_id)?;
        let entry = decode_entry(&bytes[geometry.entry_at(id)..], id)?;
        records.extend(slot_record(id, key, entry)?);
    }
    let dictionary = Dictionary { records };
    let found = occupied_entries(&dictionary);
    if found != entry_count {
        return Err(FormatError::EntryCount {
            stored: entry_count,
            found,
        });
    }
    live_keys_are_distinct(&dictionary)?;
    Ok(dictionary)
}

/// How many positions carry a `reclaimed_through`, which is what the
/// slot header's `entry_count` states.
fn occupied_entries(dictionary: &Dictionary) -> u32 {
    let found = dictionary
        .iter()
        .filter(|(_, record)| {
            matches!(
                record.state,
                SlotState::Live {
                    reclaimed_through: Some(_)
                }
            )
        })
        .count();
    // One entry per position, and positions are capped at `u16::MAX + 1`.
    u32::try_from(found).unwrap_or(u32::MAX)
}

/// One key holds at most one live id (§3.2): ids are assigned through
/// [`Dictionary::id_of`], which answers with the first live match, so
/// a second live record for the same tenant makes its
/// `reclaimed_through` unreachable and reconciliation reads the wrong
/// proof. A tombstoned record beside a live one is the ordinary case —
/// a retired id keeps its key — and is left alone.
fn live_keys_are_distinct(dictionary: &Dictionary) -> Result<(), FormatError> {
    let mut seen: HashMap<&TenantId, SlotId> = HashMap::new();
    for (id, record) in dictionary.live() {
        if let Some(first) = seen.insert(&record.key, id) {
            return Err(FormatError::DuplicateKey {
                first,
                id,
                key: record.key.clone(),
            });
        }
    }
    Ok(())
}

/// Every entry was written under the mode the slot recorded (§3.2):
/// recovery reads an entry by its mode, and a mode the header does not
/// vouch for would let a `NoConsumer` entry be read as checkpoint-
/// covered on a root that had a miner.
fn entries_agree_with(dictionary: &Dictionary, recorded: RecordedMode) -> Result<(), FormatError> {
    for (id, record) in dictionary.live() {
        if let SlotState::Live {
            reclaimed_through: Some(entry),
        } = record.state
            && !recorded.admits(entry.mode)
        {
            return Err(FormatError::EntryModeDisagrees {
                id,
                entry: entry.mode,
                recorded,
            });
        }
    }
    Ok(())
}

/// A dictionary record's key and tombstone bit; `None` for an unused
/// position, which must lie at or above `next_slot_id`.
fn decode_dict_record(
    bytes: &[u8],
    id: SlotId,
    next_slot_id: u32,
) -> Result<Option<(TenantId, bool)>, FormatError> {
    let len = usize::from(read_u16(bytes, 0));
    let below_next = u32::from(id.0) < next_slot_id;
    if len == 0 {
        if below_next {
            return Err(FormatError::DictionaryHole { id });
        }
        reserved_zero(bytes, 2..to_usize(DICT_RECORD_LEN), "dictionary record")?;
        return Ok(None);
    }
    if !below_next {
        return Err(FormatError::DictionaryPastNext { id, next_slot_id });
    }
    if len > KEY_LEN {
        return Err(FormatError::KeyLength { id, found: len });
    }
    let key =
        std::str::from_utf8(&bytes[2..2 + len]).map_err(|_| FormatError::KeyNotUtf8 { id })?;
    reserved_zero(bytes, 2 + len..130, "dictionary key padding")?;
    let flags = read_u16(bytes, 130);
    if flags & !DICT_TOMBSTONED != 0 {
        return Err(FormatError::DictionaryFlags { id, found: flags });
    }
    Ok(Some((TenantId::new(key), flags & DICT_TOMBSTONED != 0)))
}

/// An entry's content; `None` for an unoccupied one, which must be
/// all zero past its flags.
fn decode_entry(bytes: &[u8], id: SlotId) -> Result<Option<Entry>, FormatError> {
    let flags = read_u16(bytes, 0);
    if flags & !ENTRY_OCCUPIED != 0 {
        return Err(FormatError::EntryFlags { id, found: flags });
    }
    if flags & ENTRY_OCCUPIED == 0 {
        reserved_zero(bytes, 2..to_usize(ENTRY_LEN), "entry")?;
        return Ok(None);
    }
    let mode = EntryMode::from_code(read_u16(bytes, 2))?;
    reserved_zero(bytes, 4..8, "entry")?;
    Ok(Some(Entry {
        mode,
        offset: read_offset(bytes, 8),
    }))
}

/// Combine a position's dictionary record and entry: an entry may sit
/// only beside a live record.
fn slot_record(
    id: SlotId,
    key: Option<(TenantId, bool)>,
    entry: Option<Entry>,
) -> Result<Option<DictRecord>, FormatError> {
    match (key, entry) {
        (None, None) => Ok(None),
        (Some((key, false)), reclaimed_through) => Ok(Some(DictRecord {
            key,
            state: SlotState::Live { reclaimed_through },
        })),
        (Some((key, true)), None) => Ok(Some(DictRecord {
            key,
            state: SlotState::Tombstoned,
        })),
        (None | Some((_, true)), Some(_)) => Err(FormatError::EntryWithoutTenant { id }),
    }
}

/// The planned array; a present pair must name a live dictionary id.
fn decode_planned(
    bytes: &[u8],
    geometry: Geometry,
    dictionary: &Dictionary,
    planned_count: u32,
) -> Result<Vec<PlannedUnlink>, FormatError> {
    let mut planned = Vec::new();
    for position in 0..to_usize(geometry.unlinks()) {
        if let Some(record) = decode_planned_record(bytes, geometry, dictionary, position)? {
            planned.push(record);
        }
    }
    let planned_found = u32::try_from(planned.len()).unwrap_or(u32::MAX);
    if planned_found != planned_count {
        return Err(FormatError::PlannedCount {
            stored: planned_count,
            found: planned_found,
        });
    }
    Ok(planned)
}

/// One planned position; `None` when no segment occupies it, in which
/// case the whole stride must be zero.
fn decode_planned_record(
    bytes: &[u8],
    geometry: Geometry,
    dictionary: &Dictionary,
    position: usize,
) -> Result<Option<PlannedUnlink>, FormatError> {
    let at = geometry.planned_at(position);
    let flags = bytes[at + 22];
    if flags & !(PLANNED_OCCUPIED | PLANNED_UNCERTAIN) != 0 {
        return Err(FormatError::PlannedFlags {
            position,
            found: flags,
        });
    }
    reserved_zero(bytes, at + 18..at + 22, "planned record")?;
    reserved_zero(bytes, at + 23..at + 24, "planned record")?;
    if flags & PLANNED_OCCUPIED == 0 {
        if flags != 0 {
            return Err(FormatError::PlannedFlags {
                position,
                found: flags,
            });
        }
        reserved_zero(bytes, at..at + 18, "planned record")?;
        let end = at + to_usize(geometry.planned_stride());
        reserved_zero(bytes, at + 24..end, "planned pairs")?;
        return Ok(None);
    }
    let mut segment = [0u8; 16];
    segment.copy_from_slice(&bytes[at..at + 16]);
    let segment = Uuid::from_bytes(segment);
    let mut last_offsets = BTreeMap::new();
    for id in geometry.ids() {
        let pair_at = geometry.pair_at(position, id);
        let Some(offset) = decode_pair(&bytes[pair_at..], dictionary, position, id)? else {
            continue;
        };
        if offset.segment != segment {
            return Err(FormatError::PairSegment {
                position,
                id,
                found: offset.segment,
            });
        }
        last_offsets.insert(id, offset);
    }
    let tenant_count = read_u16(bytes, at + 16);
    if usize::from(tenant_count) != last_offsets.len() {
        return Err(FormatError::TenantCount {
            position,
            stored: tenant_count,
            found: last_offsets.len(),
        });
    }
    Ok(Some(PlannedUnlink {
        segment,
        uncertain: flags & PLANNED_UNCERTAIN != 0,
        last_offsets,
    }))
}

/// One planned pair; `None` when absent, which must be all zero.
fn decode_pair(
    bytes: &[u8],
    dictionary: &Dictionary,
    position: usize,
    id: SlotId,
) -> Result<Option<WalOffset>, FormatError> {
    let flags = read_u16(bytes, 0);
    if flags & !PAIR_PRESENT != 0 {
        return Err(FormatError::PairFlags {
            position,
            id,
            found: flags,
        });
    }
    reserved_zero(bytes, 2..8, "planned pair")?;
    if flags & PAIR_PRESENT == 0 {
        reserved_zero(bytes, 8..to_usize(PAIR_LEN), "planned pair")?;
        return Ok(None);
    }
    if !dictionary.is_live(id) {
        return Err(FormatError::PairWithoutTenant { position, id });
    }
    Ok(Some(read_offset(bytes, 8)))
}

/// The slot a reader believes: the valid one with the greater
/// generation. One valid slot is the ordinary post-write state (the
/// other torn, or never written); none is corruption.
///
/// # Errors
///
/// [`FormatError::NoValidSlot`] when neither decodes,
/// [`FormatError::EqualGenerations`] when both do at one generation.
pub fn choose_live(
    first: Result<DecodedSlot, FormatError>,
    second: Result<DecodedSlot, FormatError>,
) -> Result<(SlotIndex, DecodedSlot), FormatError> {
    match (first, second) {
        (Ok(a), Ok(b)) => match a.generation.cmp(&b.generation) {
            std::cmp::Ordering::Greater => Ok((SlotIndex::First, a)),
            std::cmp::Ordering::Less => Ok((SlotIndex::Second, b)),
            std::cmp::Ordering::Equal => Err(FormatError::EqualGenerations {
                generation: a.generation,
            }),
        },
        (Ok(a), Err(_)) => Ok((SlotIndex::First, a)),
        (Err(_), Ok(b)) => Ok((SlotIndex::Second, b)),
        (Err(first), Err(second)) => Err(FormatError::NoValidSlot {
            first: Box::new(first),
            second: Box::new(second),
        }),
    }
}

// ---------------------------------------------------------------
// Byte helpers
// ---------------------------------------------------------------

/// `Geometry::new` proved `file_len` fits a `usize`, and every offset
/// is below it, so the conversion cannot fail for a constructed
/// geometry; saturating keeps the helper total without a panic path.
fn to_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn read_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(buf)
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(buf)
}

fn read_offset(bytes: &[u8], at: usize) -> WalOffset {
    let mut segment = [0u8; 16];
    segment.copy_from_slice(&bytes[at..at + 16]);
    WalOffset {
        segment: Uuid::from_bytes(segment),
        byte: read_u64(bytes, at + 16),
    }
}

fn write_offset(out: &mut [u8], offset: WalOffset) {
    out[0..16].copy_from_slice(offset.segment.as_bytes());
    out[16..24].copy_from_slice(&offset.byte.to_le_bytes());
}

fn reserved_zero(
    bytes: &[u8],
    range: std::ops::Range<usize>,
    field: &'static str,
) -> Result<(), FormatError> {
    match bytes[range.clone()].iter().position(|&b| b != 0) {
        Some(offset) => Err(FormatError::Reserved {
            field,
            at: range.start + offset,
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use proptest::prelude::*;

    fn geometry(t: u32, u: u32) -> Geometry {
        Geometry::new(t, u).expect("geometry")
    }

    fn tenant(s: &str) -> TenantId {
        TenantId::new(s)
    }

    fn offset(byte: u64) -> WalOffset {
        WalOffset {
            segment: Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0001),
            byte,
        }
    }

    /// The fixture every slot test reads: three tenants (one tombstoned,
    /// one with an entry) and two planned records at geometry (8, 4).
    fn populated() -> ReclaimRecord {
        let g = geometry(8, 4);
        let mut dictionary = Dictionary::default();
        let a = dictionary.assign(&tenant("acme"), g).expect("assign a");
        let b = dictionary.assign(&tenant("beta-eu"), g).expect("assign b");
        let c = dictionary.assign(&tenant("gone"), g).expect("assign c");
        dictionary.tombstone(c).expect("tombstone");
        dictionary.get_mut(a).expect("a").state = SlotState::Live {
            reclaimed_through: Some(Entry {
                mode: EntryMode::Known,
                offset: offset(4096),
            }),
        };
        let mut planned = BTreeMap::new();
        planned.insert(a, offset(8192));
        planned.insert(b, offset(100));
        ReclaimRecord {
            witness: WitnessFlags {
                checkpoint: Witness::Terminal,
                published_seeding: Witness::Unarmed,
            },
            consumer_mode: RecordedMode::Known,
            dictionary,
            planned: vec![
                PlannedUnlink {
                    segment: offset(0).segment,
                    uncertain: false,
                    last_offsets: planned,
                },
                PlannedUnlink {
                    segment: Uuid::from_u128(9),
                    uncertain: true,
                    last_offsets: BTreeMap::new(),
                },
            ],
        }
    }

    /// Offsets inside a (8, 4) slot, written out so a test reads like
    /// the §3.2 table.
    const DICT: usize = 24;
    const ENTRIES: usize = DICT + 8 * 132;
    const PLANNED: usize = ENTRIES + 8 * 32;
    const STRIDE: usize = 24 + 32 * 8;

    /// The §3.2 figures at the defaults: 1024 tenants and a cap of 128.
    #[test]
    fn default_geometry_matches_the_rfc_figures() {
        let g = geometry(1024, 128);
        let slots = (
            g.slot_offset(SlotIndex::First),
            g.slot_offset(SlotIndex::Second),
        );
        assert_eq!((g.slot_len(), g.file_len()), (4_365_344, 8_730_720));
        assert_eq!(slots, (32, 32 + 4_365_344));
    }

    #[test]
    fn geometry_refuses_the_format_ceilings() {
        let refused = [
            (Geometry::new(0, 1), GeometryError::MaxTenants { found: 0 }),
            (
                Geometry::new(65_537, 1),
                GeometryError::MaxTenants { found: 65_537 },
            ),
            (
                Geometry::new(1, 0),
                GeometryError::MaxUnlinksPerPass { found: 0 },
            ),
        ];
        for (found, expected) in refused {
            assert_eq!(found, Err(expected));
        }
        assert!(Geometry::new(65_536, 65_536).is_ok());
    }

    #[test]
    fn a_geometry_covers_one_no_wider_on_either_axis() {
        assert!(geometry(2, 2).covers(geometry(1, 2)));
        assert!(!geometry(2, 2).covers(geometry(3, 1)));
        assert!(!geometry(2, 2).covers(geometry(1, 3)));
    }

    #[test]
    fn file_header_layout_is_the_pinned_32_bytes() {
        let g = geometry(1024, 128);
        let bytes = encode_file_header(g);
        let fixed = (&bytes[0..4], &bytes[4..8], &bytes[28..32]);
        assert_eq!(fixed, (&b"OWRC"[..], &[1, 0, 0, 0][..], &[0, 0, 0, 0][..]));
        let fields = (
            read_u64(&bytes, 8),
            read_u32(&bytes, 16),
            read_u32(&bytes, 20),
        );
        assert_eq!(fields, (4_365_344, 1024, 128));
        assert_eq!(read_u32(&bytes, 24), crc32c::crc32c(&bytes[..24]));
        assert_eq!(decode_file_header(&bytes), Ok(g));
    }

    #[test]
    fn file_header_rejects_each_invalid_field() {
        let valid = encode_file_header(geometry(4, 2));
        let corrupt = |at: usize, value: u8| {
            let mut bad = valid;
            bad[at] = value;
            decode_file_header(&bad)
        };
        assert!(matches!(
            corrupt(0, b'X'),
            Err(FormatError::BadMagic { .. })
        ));
        assert!(matches!(
            corrupt(4, 2),
            Err(FormatError::UnknownVersion { found: 2 })
        ));
        assert!(matches!(corrupt(7, 1), Err(FormatError::Reserved { .. })));
        let after_crc = corrupt(30, 1);
        assert!(matches!(after_crc, Err(FormatError::Reserved { .. })));
        assert!(matches!(
            corrupt(9, valid[9] ^ 1),
            Err(FormatError::CrcMismatch { .. })
        ));
        assert!(matches!(
            decode_file_header(&valid[..31]),
            Err(FormatError::Length { found: 31, .. })
        ));
    }

    #[test]
    fn file_header_slot_len_must_agree_with_the_capacities() {
        let mut bad = encode_file_header(geometry(4, 2));
        bad[8..16].copy_from_slice(&1u64.to_le_bytes());
        let crc = crc32c::crc32c(&bad[..24]);
        bad[24..28].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            decode_file_header(&bad),
            Err(FormatError::SlotLen { stored: 1, .. })
        ));
    }

    #[test]
    fn slot_header_fields_sit_at_their_pinned_offsets() {
        let g = geometry(8, 4);
        let bytes = encode_slot(&populated(), 3, g).expect("encode");
        assert_eq!(bytes.len() as u64, g.slot_len());
        let header = (
            read_u64(&bytes, 0),
            read_u32(&bytes, 8),
            read_u32(&bytes, 12),
            read_u16(&bytes, 16),
            read_u16(&bytes, 18),
            read_u32(&bytes, 20),
        );
        // generation, live-entry count, planned count, armed | seen,
        // Known, next_slot_id.
        assert_eq!(header, (3, 1, 2, 0b11, 1, 3));
    }

    #[test]
    fn dictionary_entries_and_planned_records_sit_at_their_pinned_offsets() {
        let g = geometry(8, 4);
        let bytes = encode_slot(&populated(), 3, g).expect("encode");
        // Record 2 is the tombstone: len 4, key "gone", flags bit 0.
        let at = DICT + 2 * 132;
        let tombstone = (
            read_u16(&bytes, at),
            &bytes[at + 2..at + 6],
            read_u16(&bytes, at + 130),
        );
        assert_eq!(tombstone, (4, &b"gone"[..], 1));
        // Entry 0 is occupied under Known at byte 4096; entry 1 is zero.
        let entry = (
            read_u16(&bytes, ENTRIES),
            read_u16(&bytes, ENTRIES + 2),
            read_u64(&bytes, ENTRIES + 8 + 16),
        );
        assert_eq!(entry, (1, 1, 4096));
        assert!(bytes[ENTRIES + 32..ENTRIES + 64].iter().all(|&b| b == 0));
        // Planned record 1 is occupied and uncertain with no pairs.
        let second = PLANNED + STRIDE;
        assert_eq!(
            (bytes[second + 22], read_u16(&bytes, second + 16)),
            (0b11, 0)
        );
    }

    #[test]
    fn populated_and_empty_records_round_trip() {
        let g = geometry(8, 4);
        let record = populated();
        let bytes = encode_slot(&record, 3, g).expect("encode");
        let decoded = decode_slot(&bytes, g).expect("decode");
        assert_eq!((decoded.generation, decoded.record), (3, record));
        let small = geometry(2, 1);
        let bytes = encode_slot(&ReclaimRecord::default(), 1, small).expect("encode empty");
        let decoded = decode_slot(&bytes, small).expect("decode empty");
        assert_eq!(
            (decoded.generation, decoded.record),
            (1, ReclaimRecord::default())
        );
    }

    #[test]
    fn encode_refuses_what_the_geometry_cannot_hold() {
        let g = geometry(2, 1);
        assert_eq!(
            encode_slot(&ReclaimRecord::default(), 0, g),
            Err(EncodeError::ZeroGeneration)
        );
        let mut record = ReclaimRecord::default();
        for key in ["a", "b", "c"] {
            record
                .dictionary
                .assign(&tenant(key), geometry(8, 8))
                .expect("assign");
        }
        assert!(matches!(
            encode_slot(&record, 1, g),
            Err(EncodeError::TooManyTenants { found: 3, .. })
        ));
        let planned = PlannedUnlink {
            segment: Uuid::from_u128(1),
            uncertain: false,
            last_offsets: BTreeMap::new(),
        };
        let record = ReclaimRecord {
            consumer_mode: RecordedMode::Known,
            planned: vec![planned; 2],
            ..ReclaimRecord::default()
        };
        assert!(matches!(
            encode_slot(&record, 1, g),
            Err(EncodeError::TooManyPlanned { found: 2, .. })
        ));
    }

    #[test]
    fn a_record_without_a_mode_carries_no_entry_and_no_plan() {
        let g = geometry(2, 1);
        let mut record = ReclaimRecord::default();
        let id = record.dictionary.assign(&tenant("a"), g).expect("assign");
        record.dictionary.get_mut(id).expect("a").state = SlotState::Live {
            reclaimed_through: Some(Entry {
                mode: EntryMode::Known,
                offset: offset(1),
            }),
        };
        assert!(matches!(
            encode_slot(&record, 1, g),
            Err(EncodeError::EntryModeDisagrees {
                recorded: RecordedMode::Unrecorded,
                ..
            })
        ));
        record.consumer_mode = RecordedMode::NoConsumer;
        assert!(matches!(
            encode_slot(&record, 1, g),
            Err(EncodeError::EntryModeDisagrees {
                entry: EntryMode::Known,
                recorded: RecordedMode::NoConsumer,
                ..
            })
        ));
        let planned_only = ReclaimRecord {
            planned: vec![PlannedUnlink {
                segment: offset(0).segment,
                uncertain: false,
                last_offsets: BTreeMap::new(),
            }],
            ..ReclaimRecord::default()
        };
        assert_eq!(
            encode_slot(&planned_only, 1, g),
            Err(EncodeError::PlannedWithoutMode)
        );
        // The same shape on disk: a Known slot whose header mode is zeroed.
        let unrecorded = resealed(geometry(8, 4), |b| b[18] = 0);
        assert!(matches!(
            unrecorded,
            Err(FormatError::EntryModeDisagrees {
                recorded: RecordedMode::Unrecorded,
                ..
            })
        ));
    }

    #[test]
    fn encode_slot_into_needs_exactly_one_slot_and_zeroes_it_first() {
        let g = geometry(2, 1);
        let mut short = vec![0u8; to_usize(g.slot_len()) - 1];
        assert!(matches!(
            encode_slot_into(&ReclaimRecord::default(), 1, g, &mut short),
            Err(EncodeError::BufferLength { .. })
        ));
        let mut dirty = vec![0xffu8; to_usize(g.slot_len())];
        encode_slot_into(&ReclaimRecord::default(), 1, g, &mut dirty).expect("encode");
        assert_eq!(
            dirty,
            encode_slot(&ReclaimRecord::default(), 1, g).expect("encode")
        );
    }

    #[test]
    fn encode_refuses_a_pair_in_another_segment() {
        let g = geometry(2, 1);
        let mut record = ReclaimRecord {
            consumer_mode: RecordedMode::Known,
            ..ReclaimRecord::default()
        };
        let id = record.dictionary.assign(&tenant("a"), g).expect("assign");
        let mut last_offsets = BTreeMap::new();
        last_offsets.insert(id, offset(1));
        record.planned = vec![PlannedUnlink {
            segment: Uuid::from_u128(7),
            uncertain: false,
            last_offsets,
        }];
        assert!(matches!(
            encode_slot(&record, 1, g),
            Err(EncodeError::PairSegment { id: SlotId(0), .. })
        ));
    }

    #[test]
    fn encode_refuses_a_pair_on_a_tombstoned_id() {
        let g = geometry(2, 1);
        let mut record = ReclaimRecord {
            consumer_mode: RecordedMode::Known,
            ..ReclaimRecord::default()
        };
        let id = record.dictionary.assign(&tenant("a"), g).expect("assign");
        record.dictionary.tombstone(id).expect("tombstone");
        let mut last_offsets = BTreeMap::new();
        last_offsets.insert(id, offset(1));
        record.planned = vec![PlannedUnlink {
            segment: offset(0).segment,
            uncertain: false,
            last_offsets,
        }];
        assert!(matches!(
            encode_slot(&record, 1, g),
            Err(EncodeError::PairWithoutTenant { .. })
        ));
    }

    /// Re-seal a deliberately inconsistent slot so the CRC passes and
    /// the field check under test is the one that fires.
    fn resealed(g: Geometry, edit: impl FnOnce(&mut [u8])) -> Result<DecodedSlot, FormatError> {
        let mut bytes = encode_slot(&populated(), 5, g).expect("encode");
        edit(&mut bytes);
        let trailer = g.trailer_offset();
        let crc = crc32c::crc32c(&bytes[..trailer]);
        bytes[trailer..trailer + 4].copy_from_slice(&crc.to_le_bytes());
        decode_slot(&bytes, g)
    }

    #[test]
    fn a_torn_byte_fails_the_crc_before_any_field_is_read() {
        let g = geometry(8, 4);
        let mut torn = encode_slot(&populated(), 5, g).expect("encode");
        torn[24] ^= 0xff;
        assert!(matches!(
            decode_slot(&torn, g),
            Err(FormatError::CrcMismatch { .. })
        ));
        let zeroed = vec![0u8; to_usize(g.slot_len())];
        assert!(matches!(
            decode_slot(&zeroed, g),
            Err(FormatError::CrcMismatch { .. })
        ));
    }

    #[test]
    fn decode_refuses_each_slot_header_inconsistency() {
        let g = geometry(8, 4);
        let reserved_flag = resealed(g, |b| b[17] = 0x80);
        assert!(matches!(
            reserved_flag,
            Err(FormatError::ReservedHeaderFlags { .. })
        ));
        let seen_unarmed = resealed(g, |b| b[16] = 0b10);
        assert!(matches!(
            seen_unarmed,
            Err(FormatError::WitnessOrder { found: 0b10 })
        ));
        let mode = resealed(g, |b| b[18] = 7);
        assert!(matches!(
            mode,
            Err(FormatError::UnknownMode {
                field: "consumer_mode",
                found: 7
            })
        ));
        let generation = resealed(g, |b| b[0..8].fill(0));
        assert!(matches!(generation, Err(FormatError::ZeroGeneration)));
        let mut bad = encode_slot(&populated(), 5, g).expect("encode");
        bad[g.trailer_offset() + 5] = 1;
        assert!(matches!(
            decode_slot(&bad, g),
            Err(FormatError::Reserved {
                field: "slot trailer",
                ..
            })
        ));
    }

    #[test]
    fn decode_refuses_each_dictionary_inconsistency() {
        let g = geometry(8, 4);
        // A hole below next_slot_id: blank record 1 out.
        let hole = resealed(g, |b| b[DICT + 132..DICT + 2 * 132].fill(0));
        assert!(matches!(
            hole,
            Err(FormatError::DictionaryHole { id: SlotId(1) })
        ));
        // A used record at or above next_slot_id.
        let past = resealed(g, |b| b[20..24].copy_from_slice(&2u32.to_le_bytes()));
        assert!(matches!(
            past,
            Err(FormatError::DictionaryPastNext { id: SlotId(2), .. })
        ));
        let padding = resealed(g, |b| b[DICT + 2 + 4] = b'!');
        assert!(matches!(
            padding,
            Err(FormatError::Reserved {
                field: "dictionary key padding",
                ..
            })
        ));
        let flags = resealed(g, |b| b[DICT + 131] = 0x80);
        assert!(matches!(
            flags,
            Err(FormatError::DictionaryFlags { id: SlotId(0), .. })
        ));
    }

    /// Rewrite dictionary record `id` as a live record for `key`.
    fn write_live_key(bytes: &mut [u8], id: usize, key: &str) {
        let at = DICT + id * 132;
        bytes[at..at + 132].fill(0);
        let len = u16::try_from(key.len()).expect("key length");
        bytes[at..at + 2].copy_from_slice(&len.to_le_bytes());
        bytes[at + 2..at + 2 + key.len()].copy_from_slice(key.as_bytes());
    }

    #[test]
    fn decode_refuses_a_second_live_record_for_one_tenant() {
        let g = geometry(8, 4);
        // Record 0 is a live "acme"; make record 1 a second one.
        let duplicate = resealed(g, |b| write_live_key(b, 1, "acme"));
        assert!(matches!(
            duplicate,
            Err(FormatError::DuplicateKey {
                first: SlotId(0),
                id: SlotId(1),
                ..
            })
        ));
        // Record 2 is the tombstoned "gone": a live record for the same
        // tenant at a lower id is not a duplicate, and holds the id.
        let revived = resealed(g, |b| write_live_key(b, 1, "gone")).expect("decode");
        assert_eq!(
            revived.record.dictionary.id_of(&tenant("gone")),
            Some(SlotId(1))
        );
    }

    #[test]
    fn decode_refuses_each_entry_inconsistency() {
        let g = geometry(8, 4);
        // An occupied entry beside the tombstone at id 2.
        let beside_tombstone = resealed(g, |b| {
            b[ENTRIES + 2 * 32] = 1;
            b[ENTRIES + 2 * 32 + 2] = 1;
        });
        assert!(matches!(
            beside_tombstone,
            Err(FormatError::EntryWithoutTenant { id: SlotId(2) })
        ));
        let mode_zero = resealed(g, |b| b[ENTRIES + 2] = 0);
        assert!(matches!(
            mode_zero,
            Err(FormatError::UnknownMode {
                field: "entry mode",
                found: 0
            })
        ));
        // A NoConsumer entry under a Known header.
        let disagrees = resealed(g, |b| b[ENTRIES + 2] = 2);
        assert!(matches!(
            disagrees,
            Err(FormatError::EntryModeDisagrees {
                id: SlotId(0),
                entry: EntryMode::NoConsumer,
                recorded: RecordedMode::Known
            })
        ));
        let count = resealed(g, |b| b[8..12].copy_from_slice(&9u32.to_le_bytes()));
        assert!(matches!(
            count,
            Err(FormatError::EntryCount {
                stored: 9,
                found: 1
            })
        ));
    }

    #[test]
    fn decode_refuses_each_planned_inconsistency() {
        let g = geometry(8, 4);
        // A present pair for the tombstoned id 2.
        let pair = resealed(g, |b| b[PLANNED + 24 + 2 * 32] = 1);
        assert!(matches!(
            pair,
            Err(FormatError::PairWithoutTenant {
                position: 0,
                id: SlotId(2)
            })
        ));
        let tenant_count = resealed(g, |b| b[PLANNED + 16] = 5);
        assert!(matches!(
            tenant_count,
            Err(FormatError::TenantCount {
                position: 0,
                stored: 5,
                found: 2
            })
        ));
        let flags = resealed(g, |b| b[PLANNED + 22] = 0b110);
        assert!(matches!(
            flags,
            Err(FormatError::PlannedFlags { position: 0, .. })
        ));
        // A pair whose offset names another segment than its record.
        let elsewhere = resealed(g, |b| b[PLANNED + 24 + 8] ^= 1);
        assert!(matches!(
            elsewhere,
            Err(FormatError::PairSegment {
                position: 0,
                id: SlotId(0),
                ..
            })
        ));
        // Bytes in an unoccupied planned position.
        let unoccupied = resealed(g, |b| b[PLANNED + 2 * STRIDE + 5] = 1);
        assert!(matches!(
            unoccupied,
            Err(FormatError::Reserved {
                field: "planned record",
                ..
            })
        ));
    }

    #[test]
    fn the_greater_generation_wins() {
        let g = geometry(4, 2);
        let older = decode_slot(&encode_slot(&ReclaimRecord::default(), 1, g).expect("e"), g);
        let mut record = ReclaimRecord::default();
        record.dictionary.assign(&tenant("x"), g).expect("assign");
        let newer = decode_slot(&encode_slot(&record, 2, g).expect("e"), g);
        let (index, live) = choose_live(older.clone(), newer.clone()).expect("both valid");
        assert_eq!((index, live.generation), (SlotIndex::Second, 2));
        let (index, live) = choose_live(newer, older.clone()).expect("both valid");
        assert_eq!((index, live.generation), (SlotIndex::First, 2));
        assert!(matches!(
            choose_live(older.clone(), older),
            Err(FormatError::EqualGenerations { generation: 1 })
        ));
    }

    #[test]
    fn a_lone_valid_slot_is_taken_and_none_is_corruption() {
        let g = geometry(4, 2);
        let valid = decode_slot(&encode_slot(&ReclaimRecord::default(), 1, g).expect("e"), g);
        let torn = Err(FormatError::CrcMismatch {
            found: 0,
            computed: 1,
        });
        let (index, live) = choose_live(torn.clone(), valid.clone()).expect("second valid");
        assert_eq!((index, live.generation), (SlotIndex::Second, 1));
        let (index, _) = choose_live(valid, torn.clone()).expect("first valid");
        assert_eq!(index, SlotIndex::First);
        assert!(matches!(
            choose_live(torn.clone(), torn),
            Err(FormatError::NoValidSlot { .. })
        ));
    }

    #[test]
    fn dictionary_assigns_upward_and_never_reuses_a_retired_id() {
        let g = geometry(4, 1);
        let mut dictionary = Dictionary::default();
        let a = dictionary.assign(&tenant("a"), g).expect("a");
        let b = dictionary.assign(&tenant("b"), g).expect("b");
        assert_eq!((a, b), (SlotId(0), SlotId(1)));
        assert_eq!(dictionary.assign(&tenant("a"), g), Ok(a), "idempotent");
        dictionary.tombstone(a).expect("tombstone");
        let retired = dictionary.id_of(&tenant("a"));
        let a2 = dictionary.assign(&tenant("a"), g).expect("a again");
        assert_eq!(
            (retired, a2),
            (None, SlotId(2)),
            "a returning tenant takes a fresh id"
        );
        dictionary.assign(&tenant("d"), g).expect("d");
        let full = dictionary.assign(&tenant("e"), g);
        assert_eq!(full, Err(DictionaryFull { max_tenants: 4 }));
        let unknown = dictionary.tombstone(SlotId(9));
        assert_eq!(unknown, Err(UnknownSlotId { id: SlotId(9) }));
    }

    #[test]
    fn dictionaries_union_by_position_and_refuse_a_conflict() {
        let g = geometry(4, 1);
        let mut dictionary = Dictionary::default();
        for key in ["a", "b", "c"] {
            dictionary.assign(&tenant(key), g).expect("assign");
        }
        // The other sidecar knows two more tenants and has tombstoned "b".
        let mut other = Dictionary::default();
        for key in ["a", "b", "c", "d", "e"] {
            let id = other.assign(&tenant(key), geometry(8, 1)).expect("other");
            if key == "b" {
                other.tombstone(id).expect("tombstone b");
            }
        }
        let seeded = dictionary.union(&other.seed_keys()).expect("union");
        let ids = (
            seeded.next_slot_id(),
            seeded.id_of(&tenant("e")),
            seeded.id_of(&tenant("b")),
        );
        assert_eq!(
            ids,
            (5, Some(SlotId(4)), None),
            "max of the two; a tombstone in either retires"
        );
        let mut conflicting = Dictionary::default();
        conflicting.assign(&tenant("zzz"), g).expect("zzz");
        assert_eq!(
            dictionary.union(&conflicting.seed_keys()),
            Err(DictionaryConflict {
                id: SlotId(0),
                left: tenant("a"),
                right: tenant("zzz"),
            })
        );
    }

    fn witness_of(two_bits: u8) -> Witness {
        match two_bits {
            0 => Witness::Unarmed,
            1 => Witness::Armed,
            _ => Witness::Terminal,
        }
    }

    fn arb_offset() -> impl Strategy<Value = WalOffset> {
        (any::<u128>(), any::<u64>()).prop_map(|(segment, byte)| WalOffset {
            segment: Uuid::from_u128(segment),
            byte,
        })
    }

    /// A record's state under `mode`: entries only exist under a
    /// recorded mode and carry that mode.
    fn arb_state(mode: RecordedMode) -> BoxedStrategy<SlotState> {
        let bare = prop_oneof![
            Just(SlotState::Tombstoned),
            Just(SlotState::Live {
                reclaimed_through: None
            }),
        ];
        let entry_mode = match mode {
            RecordedMode::Unrecorded => return bare.boxed(),
            RecordedMode::Known => EntryMode::Known,
            RecordedMode::NoConsumer => EntryMode::NoConsumer,
        };
        let entry = arb_offset().prop_map(move |offset| SlotState::Live {
            reclaimed_through: Some(Entry {
                mode: entry_mode,
                offset,
            }),
        });
        prop_oneof![bare, entry].boxed()
    }

    fn arb_dictionary(max_tenants: usize, mode: RecordedMode) -> impl Strategy<Value = Dictionary> {
        let key = prop_oneof![
            "[A-Za-z0-9._-]{1,16}",
            Just("k".repeat(KEY_LEN)),
            "[\\x21-\\x7e]{1,128}",
        ];
        prop::collection::vec((key, arb_state(mode)), 0..=max_tenants).prop_map(|records| {
            // A key holds at most one live id, so a repeat becomes the
            // retired record that case allows (§3.2).
            let mut claimed = HashSet::new();
            let records = records
                .into_iter()
                .map(|(key, state)| {
                    let state = match state {
                        SlotState::Live { .. } if !claimed.insert(key.clone()) => {
                            SlotState::Tombstoned
                        }
                        state => state,
                    };
                    DictRecord {
                        key: TenantId::new(key),
                        state,
                    }
                })
                .collect();
            Dictionary { records }
        })
    }

    fn arb_planned(
        live: Vec<SlotId>,
        max_unlinks: usize,
    ) -> impl Strategy<Value = Vec<PlannedUnlink>> {
        let count = live.len();
        let pairs = prop::collection::btree_map(
            prop::sample::select(if live.is_empty() {
                vec![SlotId(0)]
            } else {
                live
            }),
            any::<u64>(),
            0..=count,
        );
        let planned =
            (any::<u128>(), any::<bool>(), pairs).prop_map(|(segment, uncertain, bytes)| {
                let segment = Uuid::from_u128(segment);
                PlannedUnlink {
                    segment,
                    uncertain,
                    last_offsets: bytes
                        .into_iter()
                        .map(|(id, byte)| (id, WalOffset { segment, byte }))
                        .collect(),
                }
            });
        prop::collection::vec(planned, 0..=max_unlinks)
    }

    fn arb_record(g: Geometry) -> impl Strategy<Value = ReclaimRecord> {
        let mode = prop_oneof![
            Just(RecordedMode::Unrecorded),
            Just(RecordedMode::Known),
            Just(RecordedMode::NoConsumer)
        ];
        let tenants = to_usize(g.tenants());
        let unlinks = to_usize(g.unlinks());
        (any::<u8>(), mode)
            .prop_flat_map(move |(flag_bits, consumer_mode)| {
                (
                    Just(flag_bits),
                    Just(consumer_mode),
                    arb_dictionary(tenants, consumer_mode),
                )
            })
            .prop_flat_map(move |(flag_bits, consumer_mode, dictionary)| {
                let live: Vec<SlotId> = dictionary.live().map(|(id, _)| id).collect();
                let unlinks = if consumer_mode == RecordedMode::Unrecorded {
                    0
                } else {
                    unlinks
                };
                arb_planned(live, unlinks).prop_map(move |planned| ReclaimRecord {
                    witness: WitnessFlags {
                        checkpoint: witness_of(flag_bits & 0b11),
                        published_seeding: witness_of((flag_bits >> 2) & 0b11),
                    },
                    consumer_mode,
                    dictionary: dictionary.clone(),
                    planned,
                })
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(96))]

        /// Any record the writer lays out is read back identical, and
        /// any single flipped byte is refused: every byte before the
        /// trailer is under the CRC, the CRC itself must match, and the
        /// trailer's reserved bytes are checked.
        #[test]
        fn slot_round_trips_and_any_flipped_byte_is_refused(
            record in arb_record(geometry(6, 3)),
            generation in 1u64..,
            flip in any::<prop::sample::Index>(),
            mask in 1u8..,
        ) {
            let g = geometry(6, 3);
            let bytes = encode_slot(&record, generation, g).expect("encode");
            let decoded = decode_slot(&bytes, g).expect("decode");
            prop_assert_eq!((decoded.generation, &decoded.record), (generation, &record));
            let mut flipped = bytes.clone();
            let at = flip.index(flipped.len());
            flipped[at] ^= mask;
            prop_assert!(decode_slot(&flipped, g).is_err(), "byte {at} flipped by {mask:#04x}");
        }

        /// A record laid out at a wider geometry — the rebuild's copy at
        /// the same index — reads back identical.
        #[test]
        fn a_record_survives_a_wider_geometry(record in arb_record(geometry(3, 2))) {
            let wide = geometry(9, 5);
            let bytes = encode_slot(&record, 1, wide).expect("encode");
            prop_assert_eq!(decode_slot(&bytes, wide).expect("decode").record, record);
        }
    }
}
