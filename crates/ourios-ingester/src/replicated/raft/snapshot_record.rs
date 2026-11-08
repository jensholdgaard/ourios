//! A group's snapshot, kept across restarts (RFC 0065 §6.8):
//! `openraft` purges the log up to an installed or built snapshot, so the
//! state machine must reopen applied through it.
//!
//! `wal_root/groups/<group>/snapshot`, format version 2, every integer
//! little-endian:
//!
//! ```text
//! | version: u8 = 2 |
//! | last log id: present: u8 | term: u64 | index: u64 |
//! | membership log id: present: u8 | term: u64 | index: u64 |
//! | membership: the group log's membership body |
//! | flusher: present: u8 | uuid, 16 bytes big-endian |
//! | highest kind: u8 |
//! | miner configuration: kind: u8, 0 when none has committed | that kind's body |
//! |   (kind 0x04: the 28-byte body of the miner configuration entry)     |
//! | latest id block: after: u64 | through: u64, both 0 when none |
//! | crc32c of everything before it: u32 |
//! ```
//!
//! An absent id or flusher is written as zeros after its `present` byte.
//! The data an `InstallSnapshot` carries is these same bytes. Version 1,
//! which ended at the membership, is not read.

use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::Path;

use openraft::type_config::alias::{SnapshotMetaOf, SnapshotOf};
use uuid::Uuid;

use super::codec::{encode_membership, raft_id, take_membership, wal_id};
use super::gate::SyncGate;
pub use super::miner_config::GroupMinerConfig;
use super::types::{LogId, NodeId, RaftTypes};
use super::{OWNER_WAIT, sync_through, take_ownership, writer_gone};
use ourios_wal::{Control, MinerConfigV1, RaftLogId};

pub(super) type SnapshotMeta = SnapshotMetaOf<RaftTypes>;

const FILE: &str = "snapshot";
const LOCK_FILE: &str = "snapshot.lock";
const VERSION: u8 = 2;
const ID_LEN: usize = 1 + 8 + 8;
const FLUSHER_LEN: usize = 1 + 16;
const ID_BLOCK_LEN: usize = 8 + 8;
const NO_MINER_CONFIG: u8 = 0;
const MINER_CONFIG_V1: u8 = 0x04;
const CRC_LEN: usize = 4;

/// An id block `(after, through]` of RFC 0059 §3.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdBlock {
    after: u64,
    through: u64,
}

impl IdBlock {
    /// `None` unless `after < through <= 2^63 - 1`.
    #[must_use]
    pub fn new(after: u64, through: u64) -> Option<Self> {
        (after < through && through <= i64::MAX.unsigned_abs()).then_some(Self { after, through })
    }

    #[must_use]
    pub fn after(self) -> u64 {
        self.after
    }

    #[must_use]
    pub fn through(self) -> u64 {
        self.through
    }
}

/// What a snapshot holds of the group besides `openraft`'s metadata: the
/// data `InstallSnapshot` carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupState {
    /// The replica the last flusher assignment named.
    pub flusher: Option<NodeId>,
    /// The highest control kind in the log, `0` when it holds none.
    pub highest_kind: u8,
    /// The latest id block applied.
    pub id_block: Option<IdBlock>,
    /// The miner configuration in force.
    pub miner_config: Option<GroupMinerConfig>,
}

/// A group's snapshot: `openraft`'s metadata and the group's state there.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct GroupSnapshot {
    pub(super) meta: SnapshotMeta,
    pub(super) state: GroupState,
}

impl From<SnapshotOf<RaftTypes, GroupState>> for GroupSnapshot {
    fn from(snapshot: SnapshotOf<RaftTypes, GroupState>) -> Self {
        Self {
            meta: snapshot.meta,
            state: snapshot.snapshot,
        }
    }
}

impl From<GroupSnapshot> for SnapshotOf<RaftTypes, GroupState> {
    fn from(snapshot: GroupSnapshot) -> Self {
        Self {
            meta: snapshot.meta,
            snapshot: snapshot.state,
        }
    }
}

fn invalid(detail: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("group snapshot record: {detail}"),
    )
}

fn put_id(out: &mut Vec<u8>, id: Option<&LogId>) {
    let RaftLogId { term, index } = id.map_or(RaftLogId { term: 0, index: 0 }, wal_id);
    out.push(u8::from(id.is_some()));
    out.extend_from_slice(&term.to_le_bytes());
    out.extend_from_slice(&index.to_le_bytes());
}

fn put_state(out: &mut Vec<u8>, state: &GroupState) {
    out.push(u8::from(state.flusher.is_some()));
    out.extend_from_slice(state.flusher.map_or(Uuid::nil(), NodeId::uuid).as_bytes());
    out.push(state.highest_kind);
    match state.miner_config {
        None => out.push(NO_MINER_CONFIG),
        Some(config) => {
            out.push(MINER_CONFIG_V1);
            out.extend_from_slice(&config.body().to_bytes());
        }
    }
    let (after, through) = state
        .id_block
        .map_or((0, 0), |block| (block.after, block.through));
    out.extend_from_slice(&after.to_le_bytes());
    out.extend_from_slice(&through.to_le_bytes());
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

fn take_id(bytes: &[u8]) -> io::Result<Option<LogId>> {
    let id = RaftLogId {
        term: u64_at(bytes, 1),
        index: u64_at(bytes, 9),
    };
    match bytes[0] {
        0 => Ok(None),
        1 => Ok(Some(raft_id(id))),
        other => Err(invalid(format!("presence byte {other}"))),
    }
}

fn take_flusher(bytes: &[u8]) -> io::Result<Option<NodeId>> {
    let uuid = Uuid::from_slice(&bytes[1..]).map_err(invalid)?;
    match bytes[0] {
        0 => Ok(None),
        1 => Ok(Some(NodeId::from(uuid))),
        other => Err(invalid(format!("flusher presence byte {other}"))),
    }
}

fn take_id_block(bytes: &[u8]) -> io::Result<Option<IdBlock>> {
    match (u64_at(bytes, 0), u64_at(bytes, 8)) {
        (0, 0) => Ok(None),
        (after, through) => IdBlock::new(after, through)
            .map(Some)
            .ok_or_else(|| invalid(format!("id block ({after}, {through}]"))),
    }
}

fn take_state(bytes: &[u8]) -> io::Result<GroupState> {
    let fixed = FLUSHER_LEN + 2;
    if bytes.len() < fixed {
        return Err(invalid("truncated"));
    }
    let flusher = take_flusher(&bytes[..FLUSHER_LEN])?;
    let highest_kind = bytes[FLUSHER_LEN];
    if highest_kind > Control::HIGHEST_KIND {
        return Err(invalid(format!(
            "highest kind {highest_kind} is above {}, the highest this release reads",
            Control::HIGHEST_KIND
        )));
    }
    let rest = &bytes[fixed..];
    let (miner_config, id_block) = match bytes[FLUSHER_LEN + 1] {
        NO_MINER_CONFIG => (None, rest),
        MINER_CONFIG_V1 => match rest.split_first_chunk::<{ MinerConfigV1::LEN }>() {
            Some((body, rest)) => (Some(take_miner_config(*body)?), rest),
            None => return Err(invalid("truncated miner configuration")),
        },
        kind => return Err(invalid(format!("miner configuration kind {kind}"))),
    };
    if id_block.len() != ID_BLOCK_LEN {
        return Err(invalid(format!(
            "id block of {} bytes, not {ID_BLOCK_LEN}",
            id_block.len()
        )));
    }
    Ok(GroupState {
        flusher,
        highest_kind,
        id_block: take_id_block(id_block)?,
        miner_config,
    })
}

fn take_miner_config(body: [u8; MinerConfigV1::LEN]) -> io::Result<GroupMinerConfig> {
    let body = MinerConfigV1::from_bytes(body).map_err(invalid)?;
    GroupMinerConfig::new(body).map_err(invalid)
}

pub(super) fn encode(snapshot: &GroupSnapshot) -> io::Result<Vec<u8>> {
    let meta = &snapshot.meta;
    let mut out = vec![VERSION];
    put_id(&mut out, meta.last_log_id.as_ref());
    put_id(&mut out, meta.last_membership.log_id().as_ref());
    out.extend_from_slice(&encode_membership(meta.last_membership.membership())?);
    put_state(&mut out, &snapshot.state);
    let crc = crc32c::crc32c(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    Ok(out)
}

/// The record's bytes before its CRC, once the CRC and version check.
fn checked_body(bytes: &[u8]) -> io::Result<&[u8]> {
    let Some(body_len) = bytes.len().checked_sub(CRC_LEN) else {
        return Err(invalid("truncated"));
    };
    let (body, crc) = bytes.split_at(body_len);
    if u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]]) != crc32c::crc32c(body) {
        return Err(invalid("CRC mismatch"));
    }
    match body.first() {
        Some(&VERSION) => Ok(body),
        Some(other) => Err(invalid(format!("unknown version {other}"))),
        None => Err(invalid("truncated")),
    }
}

pub(super) fn decode(bytes: &[u8]) -> io::Result<GroupSnapshot> {
    let body = checked_body(bytes)?;
    let ids = 1 + 2 * ID_LEN;
    if body.len() < ids {
        return Err(invalid("truncated"));
    }
    let last_log_id = take_id(&body[1..=ID_LEN])?;
    let membership_id = take_id(&body[1 + ID_LEN..ids])?;
    let (membership, rest) = take_membership(&body[ids..])?;
    Ok(GroupSnapshot {
        meta: SnapshotMeta {
            last_log_id,
            last_membership: openraft::StoredMembership::new(membership_id, membership),
        },
        state: take_state(rest)?,
    })
}

/// The record in `dir`, or `None` when the group has no snapshot.
/// Blocking.
pub(super) fn read(dir: &Path) -> io::Result<Option<GroupSnapshot>> {
    match fs::read(dir.join(FILE)) {
        Ok(bytes) => decode(&bytes).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Replace the record in `dir` with `snapshot`: tmp, fsync, rename,
/// directory fsync, with both fsyncs behind `gate`. Blocking.
///
/// A record newer than `snapshot` is kept. `openraft` builds on a task
/// that can outlive its host's shutdown, so a build from a host already
/// replaced may arrive after the new host recorded a later snapshot;
/// the lock keeps the two writers apart.
pub(super) fn write(dir: &Path, snapshot: &GroupSnapshot, gate: &SyncGate) -> io::Result<()> {
    let _owner = take_ownership(&dir.join(LOCK_FILE), OWNER_WAIT, "snapshot record")?;
    let last = snapshot.meta.last_log_id;
    if read(dir)?.is_some_and(|recorded| recorded.meta.last_log_id > last) {
        return Ok(());
    }
    let bytes = encode(snapshot)?;
    let tmp = dir.join(format!("{FILE}.tmp"));
    let mut file = File::create(&tmp)?;
    file.write_all(&bytes)?;
    let durable = || {
        file.sync_all()?;
        fs::rename(&tmp, dir.join(FILE))?;
        File::open(dir)?.sync_all()
    };
    sync_through(gate, durable).unwrap_or_else(|| Err(writer_gone()))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use openraft::vote::leader_id_std::CommittedLeaderId;

    use super::super::types::Membership;
    use super::*;

    fn meta() -> SnapshotMeta {
        let id = |term, index| LogId {
            leader_id: CommittedLeaderId::new(term),
            index,
        };
        let (a, b) = (NodeId::from(1), NodeId::from(2));
        let nodes = BTreeMap::from([(a, "1".to_owned()), (b, "2".to_owned())]);
        let membership = Membership::new(vec![BTreeSet::from([a, b])], nodes).expect("membership");
        SnapshotMeta {
            last_log_id: Some(id(3, 9)),
            last_membership: openraft::StoredMembership::new(Some(id(2, 4)), membership),
        }
    }

    fn snapshot(meta: SnapshotMeta) -> GroupSnapshot {
        GroupSnapshot {
            meta,
            state: GroupState::default(),
        }
    }

    fn with_flusher() -> GroupSnapshot {
        GroupSnapshot {
            meta: meta(),
            state: GroupState {
                flusher: Some(NodeId::from(2)),
                highest_kind: 1,
                id_block: IdBlock::new(1 << 20, 2 << 20),
                miner_config: None,
            },
        }
    }

    fn resealed(mut bytes: Vec<u8>) -> Vec<u8> {
        let body = bytes.len() - CRC_LEN;
        let crc = crc32c::crc32c(&bytes[..body]);
        bytes[body..].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    #[test]
    fn a_record_round_trips_through_its_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read(dir.path()).expect("read"), None);
        write(dir.path(), &snapshot(meta()), &SyncGate::default()).expect("write");
        assert_eq!(read(dir.path()).expect("read"), Some(snapshot(meta())));
    }

    #[test]
    fn a_record_with_a_flusher_round_trips_through_its_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), &with_flusher(), &SyncGate::default()).expect("write");
        assert_eq!(read(dir.path()).expect("read"), Some(with_flusher()));
    }

    #[test]
    fn a_record_has_its_documented_bytes() {
        let a = NodeId::from(1);
        let membership = Membership::new(
            vec![BTreeSet::from([a])],
            BTreeMap::from([(a, "a".to_owned())]),
        )
        .expect("membership");
        let last = LogId {
            leader_id: CommittedLeaderId::new(3),
            index: 9,
        };
        let record = GroupSnapshot {
            meta: SnapshotMeta {
                last_log_id: Some(last),
                last_membership: openraft::StoredMembership::new(None, membership),
            },
            state: GroupState {
                flusher: Some(a),
                highest_kind: 1,
                id_block: IdBlock::new(5, 7),
                miner_config: None,
            },
        };
        let uuid = a.uuid().into_bytes();
        let mut expected = vec![2];
        expected.extend(
            [1].iter()
                .chain(&3_u64.to_le_bytes())
                .chain(&9_u64.to_le_bytes()),
        );
        expected.extend([0; ID_LEN]);
        expected.extend([1, 0, 1, 0].iter().chain(&uuid).chain(&[1, 0]));
        expected.extend(uuid.iter().chain(&[1, 0, b'a']));
        expected.extend([1].iter().chain(&uuid).chain(&[1, 0]));
        expected.extend(5_u64.to_le_bytes().iter().chain(&7_u64.to_le_bytes()));
        expected.extend(crc32c::crc32c(&expected).to_le_bytes());
        assert_eq!(encode(&record).expect("encode"), expected);
        assert_eq!(decode(&expected).expect("decode"), record);
    }

    #[test]
    fn a_record_with_a_flipped_bit_is_refused() {
        let mut bytes = encode(&snapshot(meta())).expect("encode");
        bytes[5] ^= 1;
        let e = decode(&bytes).expect_err("refused");
        assert!(e.to_string().contains("CRC mismatch"), "{e}");
    }

    #[test]
    #[ignore = "superseded: RFC 0065 snapshot record v2 is the version read; \
                see a_record_of_version_1_or_3_is_refused"]
    fn a_record_of_another_version_is_refused() {
        let mut bytes = encode(&snapshot(meta())).expect("encode");
        bytes[0] = 2;
        let body = bytes.len() - CRC_LEN;
        let crc = crc32c::crc32c(&bytes[..body]);
        bytes[body..].copy_from_slice(&crc.to_le_bytes());
        let e = decode(&bytes).expect_err("refused");
        assert!(e.to_string().contains("unknown version 2"), "{e}");
    }

    #[test]
    fn a_record_of_version_1_or_3_is_refused() {
        for version in [1, 3] {
            let mut bytes = encode(&with_flusher()).expect("encode");
            bytes[0] = version;
            let e = decode(&resealed(bytes)).expect_err("refused");
            assert!(
                e.to_string()
                    .contains(&format!("unknown version {version}")),
                "{e}"
            );
        }
    }

    #[test]
    #[ignore = "superseded: kind 0x04 is the miner configuration (RFC 0065 §6.5); see \
                a_record_with_a_miner_configuration_round_trips_and_another_kind_is_refused"]
    fn a_record_with_a_miner_configuration_kind_is_refused() {
        let mut bytes = encode(&with_flusher()).expect("encode");
        let kind = bytes.len() - CRC_LEN - ID_BLOCK_LEN - 1;
        bytes[kind] = 4;
        let e = decode(&resealed(bytes)).expect_err("refused");
        assert!(e.to_string().contains("miner configuration kind 4"), "{e}");
    }

    fn miner_config() -> MinerConfigV1 {
        MinerConfigV1 {
            similarity_threshold: 0.75,
            similarity_floor: 0.5,
            param_byte_limit: 512,
            prefix_depth: 3,
            max_node_children: 64,
            max_templates: 1_000,
            max_line_tokens: 256,
            upstream_templates: ourios_wal::UpstreamTemplates::Observe,
            upstream_template_byte_limit: 0,
            upstream_association_limit: 2,
        }
    }

    fn with_miner_config() -> GroupSnapshot {
        let mut snapshot = with_flusher();
        snapshot.state.highest_kind = 4;
        snapshot.state.miner_config = GroupMinerConfig::new(miner_config()).ok();
        snapshot
    }

    /// The configuration in force is kind `0x04` and its 28-byte body,
    /// between the highest kind and the id block.
    #[test]
    fn a_record_with_a_miner_configuration_round_trips_and_another_kind_is_refused() {
        let snapshot = with_miner_config();
        assert!(
            snapshot.state.miner_config.is_some(),
            "a valid configuration"
        );
        let bytes = encode(&snapshot).expect("encode");
        let at = bytes.len() - CRC_LEN - ID_BLOCK_LEN - MinerConfigV1::LEN - 1;
        assert_eq!(bytes[at - 1], 4, "the highest kind");
        assert_eq!(bytes[at], 4, "the configuration's kind");
        assert_eq!(
            bytes[at + 1..=at + MinerConfigV1::LEN],
            miner_config().to_bytes()
        );
        assert_eq!(decode(&bytes).expect("decode"), snapshot);
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), &snapshot, &SyncGate::default()).expect("write");
        assert_eq!(read(dir.path()).expect("read"), Some(snapshot));

        for kind in [3, 5] {
            let mut other = bytes.clone();
            other[at] = kind;
            let e = decode(&resealed(other)).expect_err("refused");
            let named = format!("miner configuration kind {kind}");
            assert!(e.to_string().contains(&named), "{e}");
        }
        let mut none = bytes;
        none[at] = 0;
        decode(&resealed(none)).expect_err("a configuration's bytes past kind 0");
    }

    /// A recorded configuration that `MinerConfig` refuses, or with an
    /// unknown `upstream_templates`, fails the open.
    #[test]
    fn a_record_with_a_configuration_minerconfig_refuses_is_refused() {
        let bytes = encode(&with_miner_config()).expect("encode");
        let body = bytes.len() - CRC_LEN - ID_BLOCK_LEN - MinerConfigV1::LEN;
        let refused = [
            MinerConfigV1 {
                similarity_floor: 0.9,
                ..miner_config()
            },
            MinerConfigV1 {
                prefix_depth: 9,
                ..miner_config()
            },
            MinerConfigV1 {
                max_templates: 0,
                ..miner_config()
            },
        ];
        for config in refused {
            let mut bytes = bytes.clone();
            bytes[body..body + MinerConfigV1::LEN].copy_from_slice(&config.to_bytes());
            decode(&resealed(bytes)).expect_err("refused");
        }
        let mut mode = bytes;
        mode[body + 21] = 3;
        let e = decode(&resealed(mode)).expect_err("refused");
        assert!(e.to_string().contains("upstream_templates 3"), "{e}");
    }

    #[test]
    fn a_record_with_a_kind_above_the_readable_one_is_refused() {
        let mut bytes = encode(&with_flusher()).expect("encode");
        let kind = bytes.len() - CRC_LEN - ID_BLOCK_LEN - 2;
        bytes[kind] = Control::HIGHEST_KIND + 1;
        let e = decode(&resealed(bytes)).expect_err("refused");
        let unread = format!("highest kind {}", Control::HIGHEST_KIND + 1);
        assert!(e.to_string().contains(&unread), "{e}");
    }

    #[test]
    fn a_record_with_an_invalid_id_block_is_refused() {
        let at = |bytes: &[u8]| bytes.len() - CRC_LEN - ID_BLOCK_LEN;
        let blocks: [(u64, u64); 3] = [(7, 7), (9, 7), (0, 1 << 63)];
        for (after, through) in blocks {
            let mut bytes = encode(&with_flusher()).expect("encode");
            let start = at(&bytes);
            bytes[start..start + 8].copy_from_slice(&after.to_le_bytes());
            bytes[start + 8..start + 16].copy_from_slice(&through.to_le_bytes());
            let e = decode(&resealed(bytes)).expect_err("refused");
            assert!(e.to_string().contains("id block"), "{e}");
        }
    }

    #[test]
    fn a_record_cut_short_or_with_trailing_bytes_is_refused() {
        let bytes = encode(&with_flusher()).expect("encode");
        let body = bytes.len() - CRC_LEN;
        let short = resealed(
            bytes[..body - 1]
                .iter()
                .chain(&[0; CRC_LEN])
                .copied()
                .collect(),
        );
        decode(&short).expect_err("short");
        let long = resealed(
            bytes[..body]
                .iter()
                .chain(&[0; 1 + CRC_LEN])
                .copied()
                .collect(),
        );
        decode(&long).expect_err("long");
    }

    #[test]
    fn an_older_snapshot_never_replaces_a_newer_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gate = SyncGate::default();
        let mut newer = meta();
        newer.last_log_id = Some(LogId {
            leader_id: CommittedLeaderId::new(4),
            index: 20,
        });
        write(dir.path(), &snapshot(newer.clone()), &gate).expect("write the newer");
        write(dir.path(), &snapshot(meta()), &gate).expect("an older write is not an error");
        assert_eq!(read(dir.path()).expect("read"), Some(snapshot(newer)));
    }

    #[test]
    fn a_crash_before_the_fsync_leaves_the_previous_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gate = SyncGate::default();
        write(dir.path(), &snapshot(meta()), &gate).expect("write");
        let mut later = meta();
        later.last_log_id = Some(LogId {
            leader_id: CommittedLeaderId::new(4),
            index: 20,
        });
        gate.crash();
        write(dir.path(), &snapshot(later), &gate).expect_err("the crash stops the write");
        assert_eq!(read(dir.path()).expect("read"), Some(snapshot(meta())));
    }
}
