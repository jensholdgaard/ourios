//! The `ReplicatedEntry` (`0x04`) payload codec (RFC 0065 §4.3, §4.4).
//!
//! A replicated group's WAL holds its Raft log. Every record is one
//! `0x04` frame whose payload starts with a tag byte; integers are
//! little-endian, as everywhere in RFC 0008.
//!
//! ```text
//! | tag: u8 | body                                                          |
//! | 0x01 Blank          | term: u64_le | index: u64_le                      |
//! | 0x03 Membership     | term | index | membership bytes                   |
//! | 0x04 Control        | term | index | kind: u8 | kind's body             |
//! | 0x05 Batch          | term | index | time: u64_le |                     |
//! |                     | TenantOtlpBatch payload (RFC 0046)                |
//! | 0x10 TruncateAfter  | term | index   (replay drops entries above index) |
//! | 0x11 TruncateAll    | (empty)        (replay drops every entry)         |
//! | 0x12 Purge          | term | index   (replay drops entries at or below) |
//! | 0x13 Committed      | term | index                                      |
//! ```
//!
//! Tags `0x01` and `0x03..=0x05` are log entries; `0x10..=0x13` are the
//! markers that replay honours in WAL order, so conflict truncation and
//! purge need no tail-truncate primitive. A `Batch` entry's `time` is the
//! leader's wall clock in Unix nanoseconds, at most `2^63 - 1`, and its
//! payload carries the tenant exactly as a `TenantOtlpBatch` frame does,
//! so RFC 0052's ledger keys on it. Tag `0x02`, the untimed batch, is no
//! longer read (RFC 0065 §6.5). The membership bytes are the consensus layer's own encoding;
//! this codec only bounds them. A `Control` entry is one the leader
//! appends beside the batches (RFC 0065 §6.5), a kind byte and its body:
//!
//! ```text
//! | 0x01 FlusherAssignment | replica: uuid                                   |
//! | 0x02 Checkpoint        | proposer: uuid | cut term: u64_le | cut index: u64_le |
//! |                        | highest kind: u8                                |
//! | 0x03 IdBlock           | after: u64_le | through: u64_le                   |
//! | 0x04 MinerConfig       | similarity_threshold: f32 | similarity_floor: f32 |
//! |                        | param_byte_limit: u32 | prefix_depth: u8        |
//! |                        | max_node_children: u16 | max_templates: u32     |
//! |                        | max_line_tokens: u16 | upstream_templates: u8   |
//! |                        | upstream_template_byte_limit: u32               |
//! |                        | upstream_association_limit: u16                 |
//! ```
//!
//! A UUID is 16 bytes big-endian and an `f32` its IEEE 754 bits; every
//! other kind is reserved. An id block is RFC 0059 §3.3's `(after,
//! through]`, with `after < through <= 2^63 - 1`, and `upstream_templates`
//! is `0` ignore, `1` observe or `2` adopt. Any other tag, an unknown
//! control kind, a fixed-size body of the wrong length, an id block
//! outside that range, another `upstream_templates` value, a `Batch` whose
//! time is above `2^63 - 1` or whose tenant prefix is malformed is a [`ReplicatedRecordError`], so
//! a replay refuses the log rather than skip an entry it cannot apply.
//! The miner configuration's other bounds are `MinerConfig`'s, checked by
//! the reader that applies it.

use std::time::{Duration, SystemTime};

use crate::{FrameKind, TenantBatch, TenantBatchError};

const TAG_BLANK: u8 = 0x01;
const TAG_MEMBERSHIP: u8 = 0x03;
const TAG_CONTROL: u8 = 0x04;
const TAG_BATCH: u8 = 0x05;
const TIME_LEN: usize = 8;
const KIND_FLUSHER_ASSIGNMENT: u8 = 0x01;
const KIND_CHECKPOINT: u8 = 0x02;
const KIND_ID_BLOCK: u8 = 0x03;
const KIND_MINER_CONFIG: u8 = 0x04;
const REPLICA_LEN: usize = 16;
const CHECKPOINT_LEN: usize = REPLICA_LEN + LOG_ID_LEN + 1;
const ID_BLOCK_LEN: usize = 16;
const MINER_CONFIG_LEN: usize = MinerConfigV1::LEN;
/// The highest template id (RFC 0059 §3.3).
const MAX_TEMPLATE_ID: u64 = i64::MAX.unsigned_abs();
const TAG_TRUNCATE_AFTER: u8 = 0x10;
const TAG_TRUNCATE_ALL: u8 = 0x11;
const TAG_PURGE: u8 = 0x12;
const TAG_COMMITTED: u8 = 0x13;

const LOG_ID_LEN: usize = 16;
/// Bytes before an entry's own payload: the tag and the log id.
const ENTRY_PREFIX_LEN: usize = 1 + LOG_ID_LEN;
/// Bytes before a batch entry's `TenantOtlpBatch` payload.
const BATCH_PREFIX_LEN: usize = ENTRY_PREFIX_LEN + TIME_LEN;

/// A Raft log id under a single-leader-per-term election: `(term, index)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RaftLogId {
    pub term: u64,
    pub index: u64,
}

impl RaftLogId {
    fn write(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.term.to_le_bytes());
        out.extend_from_slice(&self.index.to_le_bytes());
    }

    fn read(bytes: [u8; LOG_ID_LEN]) -> Self {
        let (term, index) = u64_pair(bytes);
        Self { term, index }
    }
}

fn u64_pair(bytes: [u8; 16]) -> (u64, u64) {
    let mut first = [0u8; 8];
    let mut second = [0u8; 8];
    first.copy_from_slice(&bytes[..8]);
    second.copy_from_slice(&bytes[8..]);
    (u64::from_le_bytes(first), u64::from_le_bytes(second))
}

/// A batch's time: Unix nanoseconds, at most `2^63 - 1`, the largest the
/// audit stream's `TIMESTAMP(NANOS)` column holds (RFC 0065 §6.5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BatchTime(u64);

impl BatchTime {
    /// The latest time a batch carries.
    pub const MAX: Self = Self(i64::MAX.unsigned_abs());

    /// `nanos` after the epoch, unless above [`Self::MAX`].
    #[must_use]
    pub const fn from_nanos(nanos: u64) -> Option<Self> {
        match nanos {
            n if n <= Self::MAX.0 => Some(Self(n)),
            _ => None,
        }
    }

    /// `time`, `0` before the epoch and [`Self::MAX`] past it.
    #[must_use]
    pub fn from_system_time(time: SystemTime) -> Self {
        match time.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(since) => u64::try_from(since.as_nanos())
                .ok()
                .and_then(Self::from_nanos)
                .unwrap_or(Self::MAX),
            Err(_) => Self(0),
        }
    }

    #[must_use]
    pub const fn nanos(self) -> u64 {
        self.0
    }

    #[must_use]
    pub fn system_time(self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_nanos(self.0)
    }
}

/// What a log entry carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryBody<'a> {
    /// The empty entry a new leader commits.
    Blank,
    /// A tenant's batch: the leader's time and the `TenantOtlpBatch`
    /// payload, tenant prefix included.
    Batch { time: BatchTime, batch: &'a [u8] },
    /// A membership change, in the consensus layer's encoding.
    Membership(&'a [u8]),
    /// An entry the leader appends beside the batches (RFC 0065 §6.5).
    Control(Control),
}

/// A control entry's kind and body (RFC 0065 §6.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// The group's flusher: its replica UUID, 16 bytes big-endian.
    FlusherAssignment([u8; REPLICA_LEN]),
    /// The group's checkpoint: the replica that proposed it, the log id of
    /// the last entry its bundle mined, and the log's highest kind there.
    Checkpoint {
        proposer: [u8; REPLICA_LEN],
        cut: RaftLogId,
        highest_kind: u8,
    },
    /// A block `(after, through]` of template ids reserved in the store
    /// (RFC 0059 §3.3); only `after < through <= 2^63 - 1` is encoded or
    /// decoded.
    IdBlock { after: u64, through: u64 },
    /// The group's miner configuration, algorithm 1.
    MinerConfig(MinerConfigV1),
}

/// How the miner treats a record's `log.record.template` (RFC 0050 §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamTemplates {
    Ignore,
    Observe,
    Adopt,
}

impl UpstreamTemplates {
    const fn byte(self) -> u8 {
        match self {
            Self::Ignore => 0,
            Self::Observe => 1,
            Self::Adopt => 2,
        }
    }

    const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Ignore),
            1 => Some(Self::Observe),
            2 => Some(Self::Adopt),
            _ => None,
        }
    }
}

/// A miner configuration entry of algorithm 1: the `MinerConfig` fields
/// (RFC 0004 §3.2, RFC 0023 §3.1, RFC 0050 §3.2) in declaration order.
///
/// Equality is bitwise, as the entry is.
#[derive(Debug, Clone, Copy)]
pub struct MinerConfigV1 {
    pub similarity_threshold: f32,
    pub similarity_floor: f32,
    pub param_byte_limit: u32,
    pub prefix_depth: u8,
    pub max_node_children: u16,
    pub max_templates: u32,
    pub max_line_tokens: u16,
    pub upstream_templates: UpstreamTemplates,
    pub upstream_template_byte_limit: u32,
    pub upstream_association_limit: u16,
}

impl MinerConfigV1 {
    /// The body's length in bytes.
    pub const LEN: usize = 28;

    /// The body's bytes, little-endian.
    #[must_use]
    pub fn to_bytes(self) -> [u8; Self::LEN] {
        let mut out = [0; Self::LEN];
        let fields: [&[u8]; 10] = [
            &self.similarity_threshold.to_bits().to_le_bytes(),
            &self.similarity_floor.to_bits().to_le_bytes(),
            &self.param_byte_limit.to_le_bytes(),
            &[self.prefix_depth],
            &self.max_node_children.to_le_bytes(),
            &self.max_templates.to_le_bytes(),
            &self.max_line_tokens.to_le_bytes(),
            &[self.upstream_templates.byte()],
            &self.upstream_template_byte_limit.to_le_bytes(),
            &self.upstream_association_limit.to_le_bytes(),
        ];
        let mut at = 0;
        for field in fields {
            out[at..at + field.len()].copy_from_slice(field);
            at += field.len();
        }
        out
    }

    /// The body `bytes` hold.
    ///
    /// # Errors
    ///
    /// [`ReplicatedRecordError::InvalidUpstreamTemplates`] when
    /// `upstream_templates` is not `0`, `1` or `2`.
    pub fn from_bytes(bytes: [u8; Self::LEN]) -> Result<Self, ReplicatedRecordError> {
        let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let u32_at = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let upstream = bytes[21];
        let Some(upstream_templates) = UpstreamTemplates::from_byte(upstream) else {
            return Err(ReplicatedRecordError::InvalidUpstreamTemplates { found: upstream });
        };
        Ok(Self {
            similarity_threshold: f32::from_bits(u32_at(0)),
            similarity_floor: f32::from_bits(u32_at(4)),
            param_byte_limit: u32_at(8),
            prefix_depth: bytes[12],
            max_node_children: u16_at(13),
            max_templates: u32_at(15),
            max_line_tokens: u16_at(19),
            upstream_templates,
            upstream_template_byte_limit: u32_at(22),
            upstream_association_limit: u16_at(26),
        })
    }
}

impl PartialEq for MinerConfigV1 {
    fn eq(&self, other: &Self) -> bool {
        self.to_bytes() == other.to_bytes()
    }
}

impl Eq for MinerConfigV1 {}

impl Control {
    /// The highest control kind this codec reads; every lower one is read
    /// too (RFC 0065 §6.8).
    pub const HIGHEST_KIND: u8 = KIND_MINER_CONFIG;

    /// The entry's kind byte.
    #[must_use]
    pub const fn kind(self) -> u8 {
        match self {
            Self::FlusherAssignment(_) => KIND_FLUSHER_ASSIGNMENT,
            Self::Checkpoint { .. } => KIND_CHECKPOINT,
            Self::IdBlock { .. } => KIND_ID_BLOCK,
            Self::MinerConfig(_) => KIND_MINER_CONFIG,
        }
    }

    const fn body_len(self) -> usize {
        match self {
            Self::FlusherAssignment(_) => REPLICA_LEN,
            Self::Checkpoint { .. } => CHECKPOINT_LEN,
            Self::IdBlock { .. } => ID_BLOCK_LEN,
            Self::MinerConfig(_) => MINER_CONFIG_LEN,
        }
    }

    /// The entry, unless it is an id block outside the id domain.
    fn checked(self) -> Result<Self, ReplicatedRecordError> {
        match self {
            Self::IdBlock { after, through } if after >= through || through > MAX_TEMPLATE_ID => {
                Err(ReplicatedRecordError::InvalidIdBlock { after, through })
            }
            _ => Ok(self),
        }
    }

    fn write(self, out: &mut Vec<u8>) {
        out.push(self.kind());
        match self {
            Self::FlusherAssignment(replica) => out.extend_from_slice(&replica),
            Self::Checkpoint {
                proposer,
                cut,
                highest_kind,
            } => {
                out.extend_from_slice(&proposer);
                cut.write(out);
                out.push(highest_kind);
            }
            Self::IdBlock { after, through } => {
                out.extend_from_slice(&after.to_le_bytes());
                out.extend_from_slice(&through.to_le_bytes());
            }
            Self::MinerConfig(config) => out.extend_from_slice(&config.to_bytes()),
        }
    }

    fn checkpoint(body: [u8; CHECKPOINT_LEN]) -> Self {
        let mut proposer = [0; REPLICA_LEN];
        proposer.copy_from_slice(&body[..REPLICA_LEN]);
        let mut cut = [0; LOG_ID_LEN];
        cut.copy_from_slice(&body[REPLICA_LEN..REPLICA_LEN + LOG_ID_LEN]);
        Self::Checkpoint {
            proposer,
            cut: RaftLogId::read(cut),
            highest_kind: body[CHECKPOINT_LEN - 1],
        }
    }

    fn id_block(body: [u8; ID_BLOCK_LEN]) -> Self {
        let (after, through) = u64_pair(body);
        Self::IdBlock { after, through }
    }

    fn read(body: &[u8]) -> Result<Self, ReplicatedRecordError> {
        // `WrongLength` counts every byte after the tag, the log id too.
        let Some((&kind, rest)) = body.split_first() else {
            return Err(wrong_length(TAG_CONTROL, LOG_ID_LEN + 1, LOG_ID_LEN));
        };
        let wrong = |len| wrong_length(TAG_CONTROL, LOG_ID_LEN + 1 + len, LOG_ID_LEN + body.len());
        match kind {
            KIND_FLUSHER_ASSIGNMENT => <[u8; REPLICA_LEN]>::try_from(rest)
                .map(Self::FlusherAssignment)
                .map_err(|_| wrong(REPLICA_LEN)),
            KIND_CHECKPOINT => <[u8; CHECKPOINT_LEN]>::try_from(rest)
                .map(Self::checkpoint)
                .map_err(|_| wrong(CHECKPOINT_LEN)),
            KIND_ID_BLOCK => <[u8; ID_BLOCK_LEN]>::try_from(rest)
                .map_err(|_| wrong(ID_BLOCK_LEN))
                .and_then(|body| Self::id_block(body).checked()),
            KIND_MINER_CONFIG => <[u8; MINER_CONFIG_LEN]>::try_from(rest)
                .map_err(|_| wrong(MINER_CONFIG_LEN))
                .and_then(MinerConfigV1::from_bytes)
                .map(Self::MinerConfig),
            found => Err(ReplicatedRecordError::UnknownControlKind { found }),
        }
    }
}

/// One record of a replicated group's WAL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicatedRecord<'a> {
    Entry {
        log_id: RaftLogId,
        body: EntryBody<'a>,
    },
    /// Keep the log up to and including the id; `None` keeps nothing.
    TruncateAfter(Option<RaftLogId>),
    /// Drop the log up to and including the id.
    Purge(RaftLogId),
    /// The highest log id known committed.
    Committed(RaftLogId),
}

impl<'a> ReplicatedRecord<'a> {
    /// The largest `TenantOtlpBatch` payload a batch entry can carry: its
    /// frame, prefix and time included, must fit
    /// [`MAX_FRAME_BYTES`](crate::MAX_FRAME_BYTES).
    pub const MAX_ENTRY_PAYLOAD_BYTES: usize = crate::MAX_FRAME_BYTES - BATCH_PREFIX_LEN;

    /// The frame payload for this record.
    ///
    /// # Errors
    ///
    /// [`ReplicatedRecordError::TenantBatch`] when a `Batch` payload is not
    /// a well-formed `TenantOtlpBatch` payload, and
    /// [`ReplicatedRecordError::InvalidIdBlock`] for an id block outside
    /// the id domain, so no record is written that [`Self::decode`] would
    /// refuse.
    pub fn encode(&self) -> Result<Vec<u8>, ReplicatedRecordError> {
        let mut out = Vec::with_capacity(ENTRY_PREFIX_LEN + self.body_len());
        match *self {
            Self::Entry { log_id, body } => {
                let (tag, bytes) = match body {
                    EntryBody::Blank => (TAG_BLANK, &[][..]),
                    EntryBody::Batch { time, batch } => {
                        TenantBatch::decode(batch).map_err(ReplicatedRecordError::TenantBatch)?;
                        out.push(TAG_BATCH);
                        log_id.write(&mut out);
                        out.extend_from_slice(&time.nanos().to_le_bytes());
                        out.extend_from_slice(batch);
                        return Ok(out);
                    }
                    EntryBody::Membership(bytes) => (TAG_MEMBERSHIP, bytes),
                    EntryBody::Control(control) => {
                        let control = control.checked()?;
                        out.push(TAG_CONTROL);
                        log_id.write(&mut out);
                        control.write(&mut out);
                        return Ok(out);
                    }
                };
                out.push(tag);
                log_id.write(&mut out);
                out.extend_from_slice(bytes);
            }
            Self::TruncateAfter(None) => out.push(TAG_TRUNCATE_ALL),
            Self::TruncateAfter(Some(log_id)) => push_marker(&mut out, TAG_TRUNCATE_AFTER, log_id),
            Self::Purge(log_id) => push_marker(&mut out, TAG_PURGE, log_id),
            Self::Committed(log_id) => push_marker(&mut out, TAG_COMMITTED, log_id),
        }
        Ok(out)
    }

    fn body_len(&self) -> usize {
        match self {
            Self::Entry {
                body: EntryBody::Batch { batch, .. },
                ..
            } => TIME_LEN + batch.len(),
            Self::Entry {
                body: EntryBody::Membership(bytes),
                ..
            } => bytes.len(),
            Self::Entry {
                body: EntryBody::Control(control),
                ..
            } => 1 + control.body_len(),
            _ => 0,
        }
    }

    /// Decode a `0x04` frame payload.
    ///
    /// # Errors
    ///
    /// See [`ReplicatedRecordError`].
    pub fn decode(payload: &'a [u8]) -> Result<Self, ReplicatedRecordError> {
        let Some((&tag, body)) = payload.split_first() else {
            return Err(ReplicatedRecordError::Empty);
        };
        match tag {
            TAG_BLANK | TAG_MEMBERSHIP | TAG_CONTROL | TAG_BATCH => decode_entry(tag, body),
            TAG_TRUNCATE_ALL if body.is_empty() => Ok(Self::TruncateAfter(None)),
            TAG_TRUNCATE_ALL => Err(wrong_length(tag, 0, body.len())),
            TAG_TRUNCATE_AFTER => exact_log_id(tag, body).map(|id| Self::TruncateAfter(Some(id))),
            TAG_PURGE => exact_log_id(tag, body).map(Self::Purge),
            TAG_COMMITTED => exact_log_id(tag, body).map(Self::Committed),
            found => Err(ReplicatedRecordError::UnknownTag { found }),
        }
    }
}

fn push_marker(out: &mut Vec<u8>, tag: u8, log_id: RaftLogId) {
    out.push(tag);
    log_id.write(out);
}

fn wrong_length(tag: u8, expected: usize, found: usize) -> ReplicatedRecordError {
    ReplicatedRecordError::WrongLength {
        tag,
        expected,
        found,
    }
}

fn exact_log_id(tag: u8, body: &[u8]) -> Result<RaftLogId, ReplicatedRecordError> {
    <[u8; LOG_ID_LEN]>::try_from(body)
        .map(RaftLogId::read)
        .map_err(|_| wrong_length(tag, LOG_ID_LEN, body.len()))
}

fn decode_entry(tag: u8, body: &[u8]) -> Result<ReplicatedRecord<'_>, ReplicatedRecordError> {
    let Some((id, rest)) = body.split_first_chunk::<LOG_ID_LEN>() else {
        return Err(wrong_length(tag, LOG_ID_LEN, body.len()));
    };
    let log_id = RaftLogId::read(*id);
    let body = match tag {
        TAG_BLANK if rest.is_empty() => EntryBody::Blank,
        TAG_BLANK => return Err(wrong_length(tag, LOG_ID_LEN, body.len())),
        TAG_BATCH => {
            let Some((time, batch)) = rest.split_first_chunk::<TIME_LEN>() else {
                return Err(wrong_length(tag, LOG_ID_LEN + TIME_LEN, body.len()));
            };
            let nanos = u64::from_le_bytes(*time);
            let time = BatchTime::from_nanos(nanos)
                .ok_or(ReplicatedRecordError::InvalidBatchTime { found: nanos })?;
            TenantBatch::decode(batch).map_err(ReplicatedRecordError::TenantBatch)?;
            EntryBody::Batch { time, batch }
        }
        TAG_CONTROL => EntryBody::Control(Control::read(rest)?),
        _ => EntryBody::Membership(rest),
    };
    Ok(ReplicatedRecord::Entry { log_id, body })
}

/// The `TenantOtlpBatch` payload a frame carries, if any: the whole
/// payload of a `0x03` frame, or the batch of a `0x04` `Batch` entry.
/// The ledger reads tenant membership through this, on append and on
/// rebuild alike, so the two cannot disagree.
pub(crate) fn tenant_batch_payload(kind: FrameKind, payload: &[u8]) -> Option<&[u8]> {
    match kind {
        FrameKind::TenantOtlpBatch => Some(payload),
        FrameKind::ReplicatedEntry if payload.first() == Some(&TAG_BATCH) => {
            payload.get(BATCH_PREFIX_LEN..)
        }
        FrameKind::ReplicatedEntry | FrameKind::OtlpBatch | FrameKind::AuditEvent => None,
    }
}

/// A `0x04` payload that is not one of the documented records.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReplicatedRecordError {
    /// The payload has no tag byte.
    Empty,
    /// The tag is not one this build defines.
    UnknownTag { found: u8 },
    /// A body shorter than its log id, or a fixed-size body of the wrong
    /// length; `expected` counts the bytes after the tag.
    WrongLength {
        tag: u8,
        expected: usize,
        found: usize,
    },
    /// A `Batch` payload whose tenant prefix is malformed.
    TenantBatch(TenantBatchError),
    /// A `Batch` whose time is above `2^63 - 1`.
    InvalidBatchTime { found: u64 },
    /// A control entry of a kind this build does not read.
    UnknownControlKind { found: u8 },
    /// An id block that is not `after < through <= 2^63 - 1`.
    InvalidIdBlock { after: u64, through: u64 },
    /// A miner configuration whose `upstream_templates` is not `0`, `1` or
    /// `2`.
    InvalidUpstreamTemplates { found: u8 },
}

impl std::fmt::Display for ReplicatedRecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "ReplicatedEntry payload is empty; it needs a tag byte"),
            Self::UnknownTag { found } => {
                write!(f, "unknown ReplicatedEntry tag 0x{found:02x}")
            }
            Self::WrongLength {
                tag,
                expected,
                found,
            } => write!(
                f,
                "ReplicatedEntry tag 0x{tag:02x} body is {found} byte(s); expected {expected}"
            ),
            Self::TenantBatch(e) => write!(f, "ReplicatedEntry Batch payload: {e}"),
            Self::InvalidBatchTime { found } => {
                write!(f, "ReplicatedEntry Batch time {found} is above 2^63 - 1")
            }
            Self::UnknownControlKind { found } => {
                write!(f, "unknown ReplicatedEntry control kind 0x{found:02x}")
            }
            Self::InvalidIdBlock { after, through } => write!(
                f,
                "ReplicatedEntry id block ({after}, {through}] is not after < through <= 2^63 - 1"
            ),
            Self::InvalidUpstreamTemplates { found } => write!(
                f,
                "ReplicatedEntry miner configuration upstream_templates {found} is not 0, 1 or 2"
            ),
        }
    }
}

impl std::error::Error for ReplicatedRecordError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TenantBatch(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const ID: RaftLogId = RaftLogId {
        term: 0x0102_0304_0506_0708,
        index: 0x1112_1314_1516_1718,
    };
    const ID_BYTES: [u8; 16] = [
        0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, 0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12,
        0x11,
    ];

    fn golden(tag: u8, tail: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend_from_slice(&ID_BYTES);
        out.extend_from_slice(tail);
        out
    }

    fn round_trip(record: ReplicatedRecord<'_>, expected: &[u8]) {
        let encoded = record.encode().expect("encode");
        assert_eq!(encoded, expected, "golden bytes for {record:?}");
        assert_eq!(ReplicatedRecord::decode(&encoded), Ok(record));
    }

    #[test]
    #[ignore = "superseded: a batch is tag 0x05 with the leader's time and 0x02 is refused \
                (RFC 0065 §6.5, #1244); see entries_encode_to_the_documented_bytes_with_a_timed_batch"]
    fn entries_encode_to_the_documented_bytes() {
        let batch = TenantBatch::encode("acme", b"proto").expect("batch");
        let blank = ReplicatedRecord::Entry {
            log_id: ID,
            body: EntryBody::Blank,
        };
        round_trip(blank, &golden(0x01, &[]));
        let normal = ReplicatedRecord::Entry {
            log_id: ID,
            body: EntryBody::Batch {
                time: BatchTime::default(),
                batch: &batch,
            },
        };
        round_trip(normal, &golden(0x02, b"\x04\x00acmeproto"));
        let membership = ReplicatedRecord::Entry {
            log_id: ID,
            body: EntryBody::Membership(b"m"),
        };
        round_trip(membership, &golden(0x03, b"m"));
    }

    /// A batch is tag `0x05`, the log id, the time and the
    /// `TenantOtlpBatch` payload (RFC 0065 §6.5).
    #[test]
    fn entries_encode_to_the_documented_bytes_with_a_timed_batch() {
        let batch = TenantBatch::encode("acme", b"proto").expect("batch");
        let blank = ReplicatedRecord::Entry {
            log_id: ID,
            body: EntryBody::Blank,
        };
        round_trip(blank, &golden(0x01, &[]));
        let time = BatchTime::from_nanos(0x2122_2324_2526_2728).expect("in range");
        let timed = ReplicatedRecord::Entry {
            log_id: ID,
            body: EntryBody::Batch {
                time,
                batch: &batch,
            },
        };
        let mut tail = vec![0x28, 0x27, 0x26, 0x25, 0x24, 0x23, 0x22, 0x21];
        tail.extend_from_slice(b"\x04\x00acmeproto");
        round_trip(timed, &golden(0x05, &tail));
        let membership = ReplicatedRecord::Entry {
            log_id: ID,
            body: EntryBody::Membership(b"m"),
        };
        round_trip(membership, &golden(0x03, b"m"));
    }

    fn batch_payload(time: u64, batch: &[u8]) -> Vec<u8> {
        let mut tail = time.to_le_bytes().to_vec();
        tail.extend_from_slice(batch);
        golden(0x05, &tail)
    }

    /// A batch's time is at most `2^63 - 1`: the largest round-trips, one
    /// above it is refused, and so is a batch too short to hold a time.
    #[test]
    fn a_batch_time_above_the_largest_is_refused() {
        let batch = TenantBatch::encode("acme", b"p").expect("batch");
        let max = i64::MAX.unsigned_abs();
        assert_eq!(BatchTime::MAX.nanos(), max);
        for nanos in [0, 1, max] {
            let time = BatchTime::from_nanos(nanos).expect("in range");
            let record = ReplicatedRecord::Entry {
                log_id: ID,
                body: EntryBody::Batch {
                    time,
                    batch: &batch,
                },
            };
            round_trip(record, &batch_payload(nanos, &batch));
        }
        for nanos in [max + 1, u64::MAX] {
            assert_eq!(BatchTime::from_nanos(nanos), None);
            let decoded = ReplicatedRecord::decode(&batch_payload(nanos, &batch)).map(|_| ());
            let invalid = ReplicatedRecordError::InvalidBatchTime { found: nanos };
            assert_eq!(decoded, Err(invalid), "{nanos}");
        }
        let short = golden(0x05, &[0; 7]);
        let decoded = ReplicatedRecord::decode(&short).map(|_| ());
        assert_eq!(decoded, Err(wrong_length(0x05, 24, 23)));
    }

    /// The untimed batch, tag `0x02`, is no longer read: it is refused as
    /// an unknown tag is, whatever it holds (RFC 0065 §6.5).
    #[test]
    fn an_untimed_batch_is_refused() {
        let batch = TenantBatch::encode("acme", b"proto").expect("batch");
        for payload in [golden(0x02, &batch), golden(0x02, &[0, 0]), vec![0x02]] {
            let decoded = ReplicatedRecord::decode(&payload).map(|_| ());
            let unknown = ReplicatedRecordError::UnknownTag { found: 0x02 };
            assert_eq!(decoded, Err(unknown), "{payload:02x?}");
        }
        let kind = FrameKind::ReplicatedEntry;
        assert_eq!(tenant_batch_payload(kind, &golden(0x02, &batch)), None);
    }

    /// A time before the epoch is `0`, one past the largest is the
    /// largest, and any other is its nanoseconds.
    #[test]
    fn a_batch_time_from_the_wall_clock_is_clamped() {
        let epoch = SystemTime::UNIX_EPOCH;
        let before = epoch.checked_sub(Duration::from_secs(1)).expect("before");
        assert_eq!(BatchTime::from_system_time(before).nanos(), 0);
        let at = epoch + Duration::from_nanos(1_700_000_000_123_456_789);
        let time = BatchTime::from_system_time(at);
        assert_eq!(time.nanos(), 1_700_000_000_123_456_789);
        assert_eq!(time.system_time(), at);
        let past = epoch + Duration::from_nanos(u64::MAX);
        assert_eq!(BatchTime::from_system_time(past), BatchTime::MAX);
    }

    /// The largest batch payload makes a frame of exactly the frame cap.
    #[test]
    fn the_largest_batch_fills_one_frame() {
        let protobuf = vec![0; ReplicatedRecord::MAX_ENTRY_PAYLOAD_BYTES - 6];
        let batch = TenantBatch::encode("acme", &protobuf).expect("batch");
        assert_eq!(batch.len(), ReplicatedRecord::MAX_ENTRY_PAYLOAD_BYTES);
        let record = ReplicatedRecord::Entry {
            log_id: ID,
            body: EntryBody::Batch {
                time: BatchTime::MAX,
                batch: &batch,
            },
        };
        let encoded = record.encode().expect("encode");
        assert_eq!(encoded.len(), crate::MAX_FRAME_BYTES);
    }

    #[test]
    fn markers_encode_to_the_documented_bytes() {
        round_trip(
            ReplicatedRecord::TruncateAfter(Some(ID)),
            &golden(0x10, &[]),
        );
        round_trip(ReplicatedRecord::TruncateAfter(None), &[0x11]);
        round_trip(ReplicatedRecord::Purge(ID), &golden(0x12, &[]));
        round_trip(ReplicatedRecord::Committed(ID), &golden(0x13, &[]));
    }

    #[test]
    #[ignore = "superseded: tag 0x04 is the control entry (RFC 0065 §6.5); see \
                malformed_payloads_and_unknown_control_kinds_are_refused"]
    fn malformed_payloads_are_refused() {
        let decode = |bytes: &[u8]| ReplicatedRecord::decode(bytes).map(|_| ());
        assert_eq!(decode(&[]), Err(ReplicatedRecordError::Empty));
        assert_eq!(
            decode(&[0x04]),
            Err(ReplicatedRecordError::UnknownTag { found: 0x04 })
        );
        assert_eq!(decode(&[0x11, 0]), Err(wrong_length(0x11, 0, 1)));
        assert_eq!(decode(&[0x12, 0, 0]), Err(wrong_length(0x12, 16, 2)));
        assert_eq!(decode(&golden(0x13, &[0])), Err(wrong_length(0x13, 16, 17)));
        assert_eq!(decode(&[0x01; 9]), Err(wrong_length(0x01, 16, 8)));
        assert_eq!(decode(&golden(0x01, &[0])), Err(wrong_length(0x01, 16, 17)));
        assert_eq!(
            decode(&golden(0x02, &[0, 0])),
            Err(ReplicatedRecordError::TenantBatch(
                TenantBatchError::EmptyTenant
            ))
        );
    }

    #[test]
    #[ignore = "superseded: kind 0x02 is the checkpoint (RFC 0065 §6.5); see \
                malformed_payloads_and_control_kinds_above_the_highest_are_refused"]
    fn malformed_payloads_and_unknown_control_kinds_are_refused() {
        let decode = |bytes: &[u8]| ReplicatedRecord::decode(bytes).map(|_| ());
        assert_eq!(decode(&[]), Err(ReplicatedRecordError::Empty));
        assert_eq!(
            decode(&[0x05]),
            Err(ReplicatedRecordError::UnknownTag { found: 0x05 })
        );
        assert_eq!(decode(&[0x11, 0]), Err(wrong_length(0x11, 0, 1)));
        assert_eq!(decode(&[0x12, 0, 0]), Err(wrong_length(0x12, 16, 2)));
        assert_eq!(decode(&golden(0x13, &[0])), Err(wrong_length(0x13, 16, 17)));
        assert_eq!(decode(&[0x01; 9]), Err(wrong_length(0x01, 16, 8)));
        assert_eq!(decode(&golden(0x01, &[0])), Err(wrong_length(0x01, 16, 17)));
        assert_eq!(
            decode(&golden(0x02, &[0, 0])),
            Err(ReplicatedRecordError::TenantBatch(
                TenantBatchError::EmptyTenant
            ))
        );
        assert_eq!(decode(&[0x04]), Err(wrong_length(0x04, 16, 0)));
        assert_eq!(decode(&golden(0x04, &[])), Err(wrong_length(0x04, 17, 16)));
        assert_eq!(
            decode(&golden(0x04, &[0x02])),
            Err(ReplicatedRecordError::UnknownControlKind { found: 0x02 })
        );
        assert_eq!(
            decode(&golden(0x04, &[0x01; 16])),
            Err(wrong_length(0x04, 33, 32))
        );
        assert_eq!(
            decode(&golden(0x04, &[0x01; 18])),
            Err(wrong_length(0x04, 33, 34))
        );
    }

    #[test]
    #[ignore = "superseded: tag 0x05 is the timed batch and 0x02 is refused (RFC 0065 §6.5, \
                #1244); see malformed_payloads_and_untimed_batches_are_refused"]
    fn malformed_payloads_and_control_kinds_above_the_highest_are_refused() {
        let unknown = Control::HIGHEST_KIND + 1;
        let mut short_checkpoint = vec![0x02];
        short_checkpoint.extend_from_slice(&[0; 32]);
        let mut long_checkpoint = short_checkpoint.clone();
        long_checkpoint.extend_from_slice(&[0; 2]);
        let refused = [
            (vec![], ReplicatedRecordError::Empty),
            (
                vec![0x05],
                ReplicatedRecordError::UnknownTag { found: 0x05 },
            ),
            (vec![0x11, 0], wrong_length(0x11, 0, 1)),
            (vec![0x12, 0, 0], wrong_length(0x12, 16, 2)),
            (golden(0x13, &[0]), wrong_length(0x13, 16, 17)),
            (vec![0x01; 9], wrong_length(0x01, 16, 8)),
            (golden(0x01, &[0]), wrong_length(0x01, 16, 17)),
            (
                golden(0x02, &[0, 0]),
                ReplicatedRecordError::TenantBatch(TenantBatchError::EmptyTenant),
            ),
            (vec![0x04], wrong_length(0x04, 16, 0)),
            (golden(0x04, &[]), wrong_length(0x04, 17, 16)),
            (
                golden(0x04, &[unknown]),
                ReplicatedRecordError::UnknownControlKind { found: unknown },
            ),
            (golden(0x04, &[0x01; 16]), wrong_length(0x04, 33, 32)),
            (golden(0x04, &[0x01; 18]), wrong_length(0x04, 33, 34)),
            (golden(0x04, &short_checkpoint), wrong_length(0x04, 50, 49)),
            (golden(0x04, &long_checkpoint), wrong_length(0x04, 50, 51)),
        ];
        for (payload, error) in refused {
            let decoded = ReplicatedRecord::decode(&payload).map(|_| ());
            assert_eq!(decoded, Err(error), "{payload:02x?}");
        }
    }

    #[test]
    fn malformed_payloads_and_untimed_batches_are_refused() {
        let unknown = Control::HIGHEST_KIND + 1;
        let mut short_checkpoint = vec![0x02];
        short_checkpoint.extend_from_slice(&[0; 32]);
        let mut long_checkpoint = short_checkpoint.clone();
        long_checkpoint.extend_from_slice(&[0; 2]);
        let refused = [
            (vec![], ReplicatedRecordError::Empty),
            (
                vec![0x06],
                ReplicatedRecordError::UnknownTag { found: 0x06 },
            ),
            (
                golden(0x02, &[0, 0]),
                ReplicatedRecordError::UnknownTag { found: 0x02 },
            ),
            (vec![0x11, 0], wrong_length(0x11, 0, 1)),
            (vec![0x12, 0, 0], wrong_length(0x12, 16, 2)),
            (golden(0x13, &[0]), wrong_length(0x13, 16, 17)),
            (vec![0x01; 9], wrong_length(0x01, 16, 8)),
            (golden(0x01, &[0]), wrong_length(0x01, 16, 17)),
            (vec![0x05], wrong_length(0x05, 16, 0)),
            (
                golden(0x05, &[0; 10]),
                ReplicatedRecordError::TenantBatch(TenantBatchError::EmptyTenant),
            ),
            (vec![0x04], wrong_length(0x04, 16, 0)),
            (golden(0x04, &[]), wrong_length(0x04, 17, 16)),
            (
                golden(0x04, &[unknown]),
                ReplicatedRecordError::UnknownControlKind { found: unknown },
            ),
            (golden(0x04, &[0x01; 16]), wrong_length(0x04, 33, 32)),
            (golden(0x04, &[0x01; 18]), wrong_length(0x04, 33, 34)),
            (golden(0x04, &short_checkpoint), wrong_length(0x04, 50, 49)),
            (golden(0x04, &long_checkpoint), wrong_length(0x04, 50, 51)),
        ];
        for (payload, error) in refused {
            let decoded = ReplicatedRecord::decode(&payload).map(|_| ());
            assert_eq!(decoded, Err(error), "{payload:02x?}");
        }
    }

    /// A flusher assignment is tag `0x04`, the log id, kind `0x01` and the
    /// replica UUID (RFC 0065 §6.5).
    #[test]
    #[ignore = "superseded: the highest kind is 0x02, the checkpoint (RFC 0065 §6.5); see \
                a_flusher_assignment_and_a_checkpoint_encode_to_the_documented_bytes"]
    fn a_flusher_assignment_encodes_to_the_documented_bytes() {
        let replica: [u8; 16] = core::array::from_fn(|i| u8::try_from(i).expect("small"));
        let assignment = ReplicatedRecord::Entry {
            log_id: ID,
            body: EntryBody::Control(Control::FlusherAssignment(replica)),
        };
        let mut tail = vec![0x01];
        tail.extend_from_slice(&replica);
        round_trip(assignment, &golden(0x04, &tail));
        assert_eq!(Control::HIGHEST_KIND, 0x01);
        assert_eq!(Control::FlusherAssignment(replica).kind(), 0x01);
    }

    /// A checkpoint is kind `0x02`: the proposer's UUID, the cut's term and
    /// index, and the highest kind at the cut (RFC 0065 §6.5).
    #[test]
    #[ignore = "superseded: the highest kind is 0x03, the id block (RFC 0065 §6.5); see \
                the_control_entries_encode_to_the_documented_bytes"]
    fn a_flusher_assignment_and_a_checkpoint_encode_to_the_documented_bytes() {
        let replica: [u8; 16] = core::array::from_fn(|i| u8::try_from(i).expect("small"));
        let assignment = ReplicatedRecord::Entry {
            log_id: ID,
            body: EntryBody::Control(Control::FlusherAssignment(replica)),
        };
        let mut tail = vec![0x01];
        tail.extend_from_slice(&replica);
        round_trip(assignment, &golden(0x04, &tail));

        let checkpoint = Control::Checkpoint {
            proposer: replica,
            cut: RaftLogId {
                term: 0x21,
                index: 0x0302,
            },
            highest_kind: 0x02,
        };
        let entry = ReplicatedRecord::Entry {
            log_id: ID,
            body: EntryBody::Control(checkpoint),
        };
        let mut tail = vec![0x02];
        tail.extend_from_slice(&replica);
        tail.extend_from_slice(&[0x21, 0, 0, 0, 0, 0, 0, 0]);
        tail.extend_from_slice(&[0x02, 0x03, 0, 0, 0, 0, 0, 0]);
        tail.push(0x02);
        assert_eq!(tail.len(), 1 + 33);
        round_trip(entry, &golden(0x04, &tail));
        assert_eq!(Control::HIGHEST_KIND, 0x02);
        assert_eq!(Control::FlusherAssignment(replica).kind(), 0x01);
        assert_eq!(checkpoint.kind(), 0x02);
    }

    fn control(control: Control) -> ReplicatedRecord<'static> {
        ReplicatedRecord::Entry {
            log_id: ID,
            body: EntryBody::Control(control),
        }
    }

    /// The flusher assignment, the checkpoint and the id block, kind `0x03`:
    /// `after` and `through`, each a `u64` little-endian (RFC 0065 §6.5).
    #[test]
    #[ignore = "superseded: the highest kind is 0x04, the miner configuration (RFC 0065 §6.5); \
                see every_control_kind_encodes_to_the_documented_bytes"]
    fn the_control_entries_encode_to_the_documented_bytes() {
        let replica: [u8; 16] = core::array::from_fn(|i| u8::try_from(i).expect("small"));
        let mut tail = vec![0x01];
        tail.extend_from_slice(&replica);
        let assignment = Control::FlusherAssignment(replica);
        round_trip(control(assignment), &golden(0x04, &tail));

        let checkpoint = Control::Checkpoint {
            proposer: replica,
            cut: RaftLogId {
                term: 0x21,
                index: 0x0302,
            },
            highest_kind: 0x03,
        };
        let mut tail = vec![0x02];
        tail.extend_from_slice(&replica);
        tail.extend_from_slice(&[0x21, 0, 0, 0, 0, 0, 0, 0]);
        tail.extend_from_slice(&[0x02, 0x03, 0, 0, 0, 0, 0, 0]);
        tail.push(0x03);
        round_trip(control(checkpoint), &golden(0x04, &tail));

        let block = Control::IdBlock {
            after: 0x0201,
            through: 0x7fff_ffff_ffff_ffff,
        };
        let mut tail = vec![0x03, 0x01, 0x02, 0, 0, 0, 0, 0, 0];
        tail.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]);
        round_trip(control(block), &golden(0x04, &tail));

        assert_eq!(Control::HIGHEST_KIND, 0x03);
        let kinds = [assignment, checkpoint, block].map(Control::kind);
        assert_eq!(kinds, [0x01, 0x02, 0x03]);
    }

    fn miner_config() -> MinerConfigV1 {
        MinerConfigV1 {
            similarity_threshold: 0.7,
            similarity_floor: 0.4,
            param_byte_limit: 256,
            prefix_depth: 2,
            max_node_children: 100,
            max_templates: 20_000,
            max_line_tokens: 512,
            upstream_templates: UpstreamTemplates::Observe,
            upstream_template_byte_limit: 8192,
            upstream_association_limit: 4,
        }
    }

    /// Every kind through the miner configuration, `0x04`: its 28 bytes
    /// are the fields in order, each `f32` as its IEEE 754 bits and
    /// `upstream_templates` `1` for observe (RFC 0065 §6.5).
    #[test]
    fn every_control_kind_encodes_to_the_documented_bytes() {
        let replica: [u8; 16] = core::array::from_fn(|i| u8::try_from(i).expect("small"));
        let mut tail = vec![0x01];
        tail.extend_from_slice(&replica);
        let assignment = Control::FlusherAssignment(replica);
        round_trip(control(assignment), &golden(0x04, &tail));

        let checkpoint = Control::Checkpoint {
            proposer: replica,
            cut: RaftLogId {
                term: 0x21,
                index: 0x0302,
            },
            highest_kind: 0x04,
        };
        let mut tail = vec![0x02];
        tail.extend_from_slice(&replica);
        tail.extend_from_slice(&[0x21, 0, 0, 0, 0, 0, 0, 0]);
        tail.extend_from_slice(&[0x02, 0x03, 0, 0, 0, 0, 0, 0]);
        tail.push(0x04);
        round_trip(control(checkpoint), &golden(0x04, &tail));

        let block = Control::IdBlock {
            after: 0x0201,
            through: 0x7fff_ffff_ffff_ffff,
        };
        let mut tail = vec![0x03, 0x01, 0x02, 0, 0, 0, 0, 0, 0];
        tail.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]);
        round_trip(control(block), &golden(0x04, &tail));

        let config = Control::MinerConfig(miner_config());
        let tail = [
            0x04, 0x33, 0x33, 0x33, 0x3f, 0xcd, 0xcc, 0xcc, 0x3e, 0x00, 0x01, 0x00, 0x00, 0x02,
            0x64, 0x00, 0x20, 0x4e, 0x00, 0x00, 0x00, 0x02, 0x01, 0x00, 0x20, 0x00, 0x00, 0x04,
            0x00,
        ];
        assert_eq!(tail.len(), 1 + 28);
        round_trip(control(config), &golden(0x04, &tail));

        assert_eq!(Control::HIGHEST_KIND, 0x04);
        let kinds = [assignment, checkpoint, block, config].map(Control::kind);
        assert_eq!(kinds, [0x01, 0x02, 0x03, 0x04]);
    }

    fn miner_config_payload(config: MinerConfigV1) -> Vec<u8> {
        let mut tail = vec![0x04];
        tail.extend_from_slice(&config.to_bytes());
        golden(0x04, &tail)
    }

    /// A miner configuration is 28 bytes with `upstream_templates` `0`, `1`
    /// or `2`; a body one byte short or long, or another mode, is refused.
    #[test]
    fn a_miner_configuration_of_the_wrong_length_or_mode_is_refused() {
        for mode in [
            UpstreamTemplates::Ignore,
            UpstreamTemplates::Observe,
            UpstreamTemplates::Adopt,
        ] {
            let config = MinerConfigV1 {
                upstream_templates: mode,
                ..miner_config()
            };
            let payload = miner_config_payload(config);
            assert_eq!(payload[17 + 1 + 21], mode.byte(), "{mode:?}");
            round_trip(control(Control::MinerConfig(config)), &payload);
        }
        let decode = |bytes: &[u8]| ReplicatedRecord::decode(bytes).map(|_| ());
        let mut long = miner_config_payload(miner_config());
        long.push(0);
        let short = &long[..long.len() - 2];
        assert_eq!(decode(short), Err(wrong_length(0x04, 45, 44)));
        assert_eq!(decode(&long), Err(wrong_length(0x04, 45, 46)));
        let mut mode = miner_config_payload(miner_config());
        mode[17 + 1 + 21] = 3;
        let invalid = ReplicatedRecordError::InvalidUpstreamTemplates { found: 3 };
        assert_eq!(decode(&mode), Err(invalid));
    }

    /// Two configurations are equal when their bytes are, so a NaN equals
    /// itself and `0.0` differs from `-0.0`.
    #[test]
    fn miner_configurations_compare_by_their_bits() {
        let with = |similarity_threshold| MinerConfigV1 {
            similarity_threshold,
            ..miner_config()
        };
        assert_eq!(with(f32::NAN), with(f32::NAN));
        assert_ne!(with(0.0), with(-0.0));
        assert_ne!(with(0.7), with(0.75));
    }

    fn id_block_payload(after: u64, through: u64) -> Vec<u8> {
        let mut tail = vec![0x03];
        tail.extend_from_slice(&after.to_le_bytes());
        tail.extend_from_slice(&through.to_le_bytes());
        golden(0x04, &tail)
    }

    /// An id block is `after < through <= 2^63 - 1` in 16 bytes; any other
    /// body is refused on decode, and never encoded.
    #[test]
    fn an_id_block_outside_the_id_domain_or_of_the_wrong_length_is_refused() {
        let max = MAX_TEMPLATE_ID;
        for (after, through) in [(0, 1), (0, max), (max - 1, max)] {
            let block = Control::IdBlock { after, through };
            round_trip(control(block), &id_block_payload(after, through));
        }
        for (after, through) in [(5, 5), (6, 5), (0, max + 1), (max, u64::MAX)] {
            let invalid = ReplicatedRecordError::InvalidIdBlock { after, through };
            let decoded = ReplicatedRecord::decode(&id_block_payload(after, through)).map(|_| ());
            assert_eq!(decoded, Err(invalid.clone()), "({after}, {through}]");
            let encoded = control(Control::IdBlock { after, through }).encode();
            assert_eq!(encoded, Err(invalid), "({after}, {through}]");
        }
        let mut long = id_block_payload(0, 1);
        long.push(0);
        let short = &long[..long.len() - 2];
        let decode = |bytes: &[u8]| ReplicatedRecord::decode(bytes).map(|_| ());
        assert_eq!(decode(short), Err(wrong_length(0x04, 33, 32)));
        assert_eq!(decode(&long), Err(wrong_length(0x04, 33, 34)));
    }

    #[test]
    fn a_malformed_normal_body_is_never_encoded() {
        let record = ReplicatedRecord::Entry {
            log_id: ID,
            body: EntryBody::Batch {
                time: BatchTime::default(),
                batch: &[1],
            },
        };
        assert!(matches!(
            record.encode(),
            Err(ReplicatedRecordError::TenantBatch(_))
        ));
    }

    #[test]
    fn only_normal_entries_and_tenant_frames_carry_a_tenant() {
        let batch = TenantBatch::encode("acme", b"p").expect("batch");
        let normal = ReplicatedRecord::Entry {
            log_id: ID,
            body: EntryBody::Batch {
                time: BatchTime::MAX,
                batch: &batch,
            },
        }
        .encode()
        .expect("encode");
        let kind = FrameKind::ReplicatedEntry;
        assert_eq!(tenant_batch_payload(kind, &normal), Some(&batch[..]));
        assert_eq!(
            tenant_batch_payload(FrameKind::TenantOtlpBatch, &batch),
            Some(&batch[..])
        );
        assert_eq!(tenant_batch_payload(kind, &golden(0x01, &[])), None);
        assert_eq!(tenant_batch_payload(kind, &golden(0x03, &batch)), None);
        assert_eq!(tenant_batch_payload(FrameKind::OtlpBatch, &batch), None);
    }

    fn any_record() -> impl Strategy<Value = (u8, u64, u64, Vec<u8>)> {
        (
            0u8..11,
            any::<u64>(),
            any::<u64>(),
            proptest::collection::vec(any::<u8>(), 0..64),
        )
    }

    proptest! {
        #[test]
        fn every_encoded_record_decodes_to_itself((pick, term, index, bytes) in any_record()) {
            let log_id = RaftLogId { term, index };
            let batch = TenantBatch::encode("t", &bytes).expect("batch");
            let record = match pick {
                0 => ReplicatedRecord::Entry { log_id, body: EntryBody::Blank },
                1 => {
                    let time = BatchTime::from_nanos(term >> 1).expect("below 2^63");
                    ReplicatedRecord::Entry { log_id, body: EntryBody::Batch { time, batch: &batch } }
                }
                2 => ReplicatedRecord::Entry { log_id, body: EntryBody::Membership(&bytes) },
                3 => ReplicatedRecord::TruncateAfter(Some(log_id)),
                4 => ReplicatedRecord::TruncateAfter(None),
                5 => ReplicatedRecord::Purge(log_id),
                6 => {
                    let replica = core::array::from_fn(|i| bytes.get(i).copied().unwrap_or(0));
                    let control = Control::FlusherAssignment(replica);
                    ReplicatedRecord::Entry { log_id, body: EntryBody::Control(control) }
                }
                7 => {
                    let proposer = core::array::from_fn(|i| bytes.get(i).copied().unwrap_or(0));
                    let highest_kind = bytes.first().copied().unwrap_or(0);
                    let cut = RaftLogId { term: index, index: term };
                    let control = Control::Checkpoint { proposer, cut, highest_kind };
                    ReplicatedRecord::Entry { log_id, body: EntryBody::Control(control) }
                }
                8 => {
                    let after = term % MAX_TEMPLATE_ID;
                    let through = after + 1 + index % (MAX_TEMPLATE_ID - after);
                    let control = Control::IdBlock { after, through };
                    ReplicatedRecord::Entry { log_id, body: EntryBody::Control(control) }
                }
                9 => {
                    let mut body = [0; MinerConfigV1::LEN];
                    for (to, from) in body.iter_mut().zip(bytes.iter().chain(&term.to_le_bytes())) {
                        *to = *from;
                    }
                    body[21] %= 3;
                    let config = MinerConfigV1::from_bytes(body).expect("a mode below 3");
                    let control = Control::MinerConfig(config);
                    ReplicatedRecord::Entry { log_id, body: EntryBody::Control(control) }
                }
                _ => ReplicatedRecord::Committed(log_id),
            };
            let encoded = record.encode().expect("encode");
            prop_assert_eq!(ReplicatedRecord::decode(&encoded), Ok(record));
        }

        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
            let _ = ReplicatedRecord::decode(&bytes);
        }
    }
}
