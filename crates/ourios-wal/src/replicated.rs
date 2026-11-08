//! The `ReplicatedEntry` (`0x04`) payload codec (RFC 0065 §4.3, §4.4).
//!
//! A replicated group's WAL holds its Raft log. Every record is one
//! `0x04` frame whose payload starts with a tag byte; integers are
//! little-endian, as everywhere in RFC 0008.
//!
//! ```text
//! | tag: u8 | body                                                          |
//! | 0x01 Blank          | term: u64_le | index: u64_le                      |
//! | 0x02 Normal         | term | index | TenantOtlpBatch payload (RFC 0046) |
//! | 0x03 Membership     | term | index | membership bytes                   |
//! | 0x04 Control        | term | index | kind: u8 | kind's body             |
//! | 0x10 TruncateAfter  | term | index   (replay drops entries above index) |
//! | 0x11 TruncateAll    | (empty)        (replay drops every entry)         |
//! | 0x12 Purge          | term | index   (replay drops entries at or below) |
//! | 0x13 Committed      | term | index                                      |
//! ```
//!
//! Tags `0x01..=0x04` are log entries; `0x10..=0x13` are the markers
//! that replay honours in WAL order, so conflict truncation and purge
//! need no tail-truncate primitive. A `Normal` entry carries the tenant
//! exactly as a `TenantOtlpBatch` frame does, so RFC 0052's ledger keys
//! on it. The membership bytes are the consensus layer's own encoding;
//! this codec only bounds them. A `Control` entry is one the leader
//! appends beside the batches (RFC 0065 §6.5), a kind byte and its body:
//!
//! ```text
//! | 0x01 FlusherAssignment | replica: uuid                                   |
//! | 0x02 Checkpoint        | proposer: uuid | cut term: u64_le | cut index: u64_le |
//! |                        | highest kind: u8                                |
//! | 0x03 IdBlock           | after: u64_le | through: u64_le                   |
//! ```
//!
//! A UUID is 16 bytes big-endian; every other kind is reserved. An id
//! block is RFC 0059 §3.3's `(after, through]`, with `after < through <=
//! 2^63 - 1`. Any other tag, an unknown control kind, a fixed-size body of
//! the wrong length, an id block outside that range, or a `Normal` body
//! whose tenant prefix is malformed is a [`ReplicatedRecordError`], so a
//! replay refuses the log rather than skip an entry it cannot apply.

use crate::{FrameKind, TenantBatch, TenantBatchError};

const TAG_BLANK: u8 = 0x01;
const TAG_NORMAL: u8 = 0x02;
const TAG_MEMBERSHIP: u8 = 0x03;
const TAG_CONTROL: u8 = 0x04;
const KIND_FLUSHER_ASSIGNMENT: u8 = 0x01;
const KIND_CHECKPOINT: u8 = 0x02;
const KIND_ID_BLOCK: u8 = 0x03;
const REPLICA_LEN: usize = 16;
const CHECKPOINT_LEN: usize = REPLICA_LEN + LOG_ID_LEN + 1;
const ID_BLOCK_LEN: usize = 16;
/// The highest template id (RFC 0059 §3.3).
const MAX_TEMPLATE_ID: u64 = i64::MAX.unsigned_abs();
const TAG_TRUNCATE_AFTER: u8 = 0x10;
const TAG_TRUNCATE_ALL: u8 = 0x11;
const TAG_PURGE: u8 = 0x12;
const TAG_COMMITTED: u8 = 0x13;

const LOG_ID_LEN: usize = 16;
/// Bytes before an entry's own payload: the tag and the log id.
const ENTRY_PREFIX_LEN: usize = 1 + LOG_ID_LEN;

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

/// What a log entry carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryBody<'a> {
    /// The empty entry a new leader commits.
    Blank,
    /// A `TenantOtlpBatch` payload, tenant prefix included.
    Normal(&'a [u8]),
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
}

impl Control {
    /// The highest control kind this codec reads; every lower one is read
    /// too (RFC 0065 §6.8).
    pub const HIGHEST_KIND: u8 = KIND_ID_BLOCK;

    /// The entry's kind byte.
    #[must_use]
    pub const fn kind(self) -> u8 {
        match self {
            Self::FlusherAssignment(_) => KIND_FLUSHER_ASSIGNMENT,
            Self::Checkpoint { .. } => KIND_CHECKPOINT,
            Self::IdBlock { .. } => KIND_ID_BLOCK,
        }
    }

    const fn body_len(self) -> usize {
        match self {
            Self::FlusherAssignment(_) => REPLICA_LEN,
            Self::Checkpoint { .. } => CHECKPOINT_LEN,
            Self::IdBlock { .. } => ID_BLOCK_LEN,
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
    /// The largest payload a log entry can carry: its frame, prefix
    /// included, must fit [`MAX_FRAME_BYTES`](crate::MAX_FRAME_BYTES).
    pub const MAX_ENTRY_PAYLOAD_BYTES: usize = crate::MAX_FRAME_BYTES - ENTRY_PREFIX_LEN;

    /// The frame payload for this record.
    ///
    /// # Errors
    ///
    /// [`ReplicatedRecordError::TenantBatch`] when a `Normal` body is not
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
                    EntryBody::Normal(bytes) => {
                        TenantBatch::decode(bytes).map_err(ReplicatedRecordError::TenantBatch)?;
                        (TAG_NORMAL, bytes)
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
                body: EntryBody::Normal(bytes) | EntryBody::Membership(bytes),
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
            TAG_BLANK | TAG_NORMAL | TAG_MEMBERSHIP | TAG_CONTROL => decode_entry(tag, body),
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
        TAG_NORMAL => {
            TenantBatch::decode(rest).map_err(ReplicatedRecordError::TenantBatch)?;
            EntryBody::Normal(rest)
        }
        TAG_CONTROL => EntryBody::Control(Control::read(rest)?),
        _ => EntryBody::Membership(rest),
    };
    Ok(ReplicatedRecord::Entry { log_id, body })
}

/// The `TenantOtlpBatch` payload a frame carries, if any: the whole
/// payload of a `0x03` frame, or the body of a `0x04` `Normal` entry.
/// The ledger reads tenant membership through this, on append and on
/// rebuild alike, so the two cannot disagree.
pub(crate) fn tenant_batch_payload(kind: FrameKind, payload: &[u8]) -> Option<&[u8]> {
    match kind {
        FrameKind::TenantOtlpBatch => Some(payload),
        FrameKind::ReplicatedEntry if payload.first() == Some(&TAG_NORMAL) => {
            payload.get(ENTRY_PREFIX_LEN..)
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
    /// A `Normal` body whose tenant prefix is malformed.
    TenantBatch(TenantBatchError),
    /// A control entry of a kind this build does not read.
    UnknownControlKind { found: u8 },
    /// An id block that is not `after < through <= 2^63 - 1`.
    InvalidIdBlock { after: u64, through: u64 },
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
            Self::TenantBatch(e) => write!(f, "ReplicatedEntry Normal body: {e}"),
            Self::UnknownControlKind { found } => {
                write!(f, "unknown ReplicatedEntry control kind 0x{found:02x}")
            }
            Self::InvalidIdBlock { after, through } => write!(
                f,
                "ReplicatedEntry id block ({after}, {through}] is not after < through <= 2^63 - 1"
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
    fn entries_encode_to_the_documented_bytes() {
        let batch = TenantBatch::encode("acme", b"proto").expect("batch");
        let blank = ReplicatedRecord::Entry {
            log_id: ID,
            body: EntryBody::Blank,
        };
        round_trip(blank, &golden(0x01, &[]));
        let normal = ReplicatedRecord::Entry {
            log_id: ID,
            body: EntryBody::Normal(&batch),
        };
        round_trip(normal, &golden(0x02, b"\x04\x00acmeproto"));
        let membership = ReplicatedRecord::Entry {
            log_id: ID,
            body: EntryBody::Membership(b"m"),
        };
        round_trip(membership, &golden(0x03, b"m"));
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
            body: EntryBody::Normal(&[1]),
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
            body: EntryBody::Normal(&batch),
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
            0u8..10,
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
                1 => ReplicatedRecord::Entry { log_id, body: EntryBody::Normal(&batch) },
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
