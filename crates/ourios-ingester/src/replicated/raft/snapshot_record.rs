//! A group's installed snapshot, kept across restarts (RFC 0065 §6.8):
//! `openraft` purges the log up to an installed snapshot, so the state
//! machine must reopen applied through it.
//!
//! `wal_root/groups/<group>/snapshot`, little-endian:
//!
//! ```text
//! | version: u8 = 1 |
//! | last log id: present: u8 | term: u64 | index: u64 |
//! | membership log id: present: u8 | term: u64 | index: u64 |
//! | membership: the group log's membership body |
//! | crc32c of everything before it: u32 |
//! ```
//!
//! An absent id is written as zeros after its `present` byte.

use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::Path;

use openraft::type_config::alias::SnapshotMetaOf;

use super::codec::{decode_membership, encode_membership, raft_id, wal_id};
use super::gate::SyncGate;
use super::types::{LogId, RaftTypes};
use super::{sync_through, writer_gone};
use ourios_wal::RaftLogId;

pub(super) type SnapshotMeta = SnapshotMetaOf<RaftTypes>;

const FILE: &str = "snapshot";
const VERSION: u8 = 1;
const ID_LEN: usize = 1 + 8 + 8;
const CRC_LEN: usize = 4;

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

pub(super) fn encode(meta: &SnapshotMeta) -> io::Result<Vec<u8>> {
    let mut out = vec![VERSION];
    put_id(&mut out, meta.last_log_id.as_ref());
    put_id(&mut out, meta.last_membership.log_id().as_ref());
    out.extend_from_slice(&encode_membership(meta.last_membership.membership())?);
    let crc = crc32c::crc32c(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    Ok(out)
}

pub(super) fn decode(bytes: &[u8]) -> io::Result<SnapshotMeta> {
    let Some(body_len) = bytes.len().checked_sub(CRC_LEN) else {
        return Err(invalid("truncated"));
    };
    let (body, crc) = bytes.split_at(body_len);
    if u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]]) != crc32c::crc32c(body) {
        return Err(invalid("CRC mismatch"));
    }
    match body.first() {
        Some(&VERSION) => {}
        Some(other) => return Err(invalid(format!("unknown version {other}"))),
        None => return Err(invalid("truncated")),
    }
    let ids = 1 + 2 * ID_LEN;
    if body.len() < ids {
        return Err(invalid("truncated"));
    }
    let last_log_id = take_id(&body[1..=ID_LEN])?;
    let membership_id = take_id(&body[1 + ID_LEN..ids])?;
    let membership = decode_membership(&body[ids..])?;
    Ok(SnapshotMeta {
        last_log_id,
        last_membership: openraft::StoredMembership::new(membership_id, membership),
    })
}

/// The record in `dir`, or `None` when the group has never installed a
/// snapshot. Blocking.
pub(super) fn read(dir: &Path) -> io::Result<Option<SnapshotMeta>> {
    match fs::read(dir.join(FILE)) {
        Ok(bytes) => decode(&bytes).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Replace the record in `dir` with `meta`: tmp, fsync, rename, directory
/// fsync, with both fsyncs behind `gate`. Blocking.
pub(super) fn write(dir: &Path, meta: &SnapshotMeta, gate: &SyncGate) -> io::Result<()> {
    let bytes = encode(meta)?;
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

    use super::super::types::{Membership, NodeId};
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

    #[test]
    fn a_record_round_trips_through_its_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read(dir.path()).expect("read"), None);
        write(dir.path(), &meta(), &SyncGate::default()).expect("write");
        assert_eq!(read(dir.path()).expect("read"), Some(meta()));
    }

    #[test]
    fn a_record_with_a_flipped_bit_is_refused() {
        let mut bytes = encode(&meta()).expect("encode");
        bytes[5] ^= 1;
        let e = decode(&bytes).expect_err("refused");
        assert!(e.to_string().contains("CRC mismatch"), "{e}");
    }

    #[test]
    fn a_record_of_another_version_is_refused() {
        let mut bytes = encode(&meta()).expect("encode");
        bytes[0] = 2;
        let body = bytes.len() - CRC_LEN;
        let crc = crc32c::crc32c(&bytes[..body]);
        bytes[body..].copy_from_slice(&crc.to_le_bytes());
        let e = decode(&bytes).expect_err("refused");
        assert!(e.to_string().contains("unknown version 2"), "{e}");
    }

    #[test]
    fn a_crash_before_the_fsync_leaves_the_previous_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gate = SyncGate::default();
        write(dir.path(), &meta(), &gate).expect("write");
        let mut later = meta();
        later.last_log_id = Some(LogId {
            leader_id: CommittedLeaderId::new(4),
            index: 20,
        });
        gate.crash();
        write(dir.path(), &later, &gate).expect_err("the crash stops the write");
        assert_eq!(read(dir.path()).expect("read"), Some(meta()));
    }
}
