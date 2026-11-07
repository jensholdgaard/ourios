//! A group log's records in `ourios-wal`'s `0x04` codec.
//!
//! A membership entry's body, little-endian:
//!
//! ```text
//! | configs: u16 | per config: voters: u16 | voter id: [u8; 16]*   |
//! | nodes: u16   | per node: id: [u8; 16] | len: u16 | node_id: UTF-8 |
//! ```
//!
//! A node id is the replica's UUID in its 16 big-endian bytes. The
//! membership is rebuilt with `Membership::new`, so a voter without a
//! node is refused as `openraft` refuses it.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use openraft::EntryPayload;
use openraft::vote::leader_id_std::CommittedLeaderId;
use ourios_wal::{EntryBody, RaftLogId, ReplicatedRecord};
use uuid::Uuid;

use super::types::{Entry, LogId, Membership, NodeId, TenantBatchBytes};

/// A record as replay applies it.
#[derive(Debug, PartialEq)]
pub(super) enum Replayed {
    Entry(Entry),
    TruncateAfter(Option<LogId>),
    Purge(LogId),
    Committed(LogId),
}

fn invalid(detail: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail.to_string())
}

pub(super) fn wal_id(id: &LogId) -> RaftLogId {
    RaftLogId {
        term: id.leader_id.term,
        index: id.index,
    }
}

pub(super) fn raft_id(id: RaftLogId) -> LogId {
    LogId {
        leader_id: CommittedLeaderId::new(id.term),
        index: id.index,
    }
}

/// The frame payload of one log entry.
pub(super) fn encode_entry(entry: &Entry) -> io::Result<Vec<u8>> {
    let membership;
    let body = match &entry.payload {
        EntryPayload::Blank => EntryBody::Blank,
        EntryPayload::Normal(batch) => EntryBody::Normal(batch.as_bytes()),
        EntryPayload::Membership(m) => {
            membership = encode_membership(m)?;
            EntryBody::Membership(&membership)
        }
    };
    let log_id = wal_id(&entry.log_id);
    ReplicatedRecord::Entry { log_id, body }
        .encode()
        .map_err(invalid)
}

/// The frame payload of a truncate, purge or committed marker.
pub(super) fn encode_marker(record: ReplicatedRecord<'_>) -> io::Result<Vec<u8>> {
    record.encode().map_err(invalid)
}

pub(super) fn decode(payload: &[u8]) -> io::Result<Replayed> {
    match ReplicatedRecord::decode(payload).map_err(invalid)? {
        ReplicatedRecord::Entry { log_id, body } => {
            let payload = match body {
                EntryBody::Blank => EntryPayload::Blank,
                EntryBody::Normal(bytes) => {
                    EntryPayload::Normal(TenantBatchBytes::new(bytes.to_vec()).map_err(invalid)?)
                }
                EntryBody::Membership(bytes) => EntryPayload::Membership(decode_membership(bytes)?),
                // Not applied yet (RFC 0065 §6.5): refuse the log rather
                // than skip an entry that mining will depend on.
                EntryBody::Control(control) => {
                    return Err(invalid(format!(
                        "a control entry this replica does not apply yet: {control:?}"
                    )));
                }
            };
            let log_id = raft_id(log_id);
            Ok(Replayed::Entry(Entry { log_id, payload }))
        }
        ReplicatedRecord::TruncateAfter(id) => Ok(Replayed::TruncateAfter(id.map(raft_id))),
        ReplicatedRecord::Purge(id) => Ok(Replayed::Purge(raft_id(id))),
        ReplicatedRecord::Committed(id) => Ok(Replayed::Committed(raft_id(id))),
    }
}

fn put_len(out: &mut Vec<u8>, len: usize, what: &str) -> io::Result<()> {
    let len = u16::try_from(len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("too many {what}")))?;
    out.extend_from_slice(&len.to_le_bytes());
    Ok(())
}

pub(super) fn encode_membership(m: &Membership) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let configs = m.get_joint_config();
    put_len(&mut out, configs.len(), "membership configs")?;
    for voters in configs {
        put_len(&mut out, voters.len(), "voters")?;
        for id in voters {
            out.extend_from_slice(id.uuid().as_bytes());
        }
    }
    let nodes: Vec<_> = m.nodes().collect();
    put_len(&mut out, nodes.len(), "nodes")?;
    for (id, node_id) in nodes {
        out.extend_from_slice(id.uuid().as_bytes());
        put_len(&mut out, node_id.len(), "node_id bytes")?;
        out.extend_from_slice(node_id.as_bytes());
    }
    Ok(out)
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if n > self.0.len() {
            return Err(invalid("membership body is truncated"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn len(&mut self) -> io::Result<usize> {
        let bytes = self.take(2)?;
        Ok(usize::from(u16::from_le_bytes([bytes[0], bytes[1]])))
    }

    fn node_id(&mut self) -> io::Result<NodeId> {
        let bytes = self.take(16)?;
        let uuid = Uuid::from_slice(bytes).map_err(invalid)?;
        Ok(NodeId::from(uuid))
    }
}

pub(super) fn decode_membership(bytes: &[u8]) -> io::Result<Membership> {
    let mut c = Cursor(bytes);
    let mut configs = Vec::new();
    for _ in 0..c.len()? {
        let voters = (0..c.len()?)
            .map(|_| c.node_id())
            .collect::<io::Result<BTreeSet<_>>>()?;
        configs.push(voters);
    }
    let mut nodes = BTreeMap::new();
    for _ in 0..c.len()? {
        let id = c.node_id()?;
        let len = c.len()?;
        let node_id = std::str::from_utf8(c.take(len)?).map_err(invalid)?;
        nodes.insert(id, node_id.to_owned());
    }
    if !c.0.is_empty() {
        return Err(invalid("trailing bytes after the membership"));
    }
    Membership::new(configs, nodes).map_err(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ourios_wal::TenantBatch;

    fn id(n: u64) -> NodeId {
        NodeId::from(n)
    }

    fn log_id(term: u64, index: u64) -> LogId {
        raft_id(RaftLogId { term, index })
    }

    fn round_trip(entry: &Entry) {
        let bytes = encode_entry(entry).expect("encode");
        match decode(&bytes).expect("decode") {
            Replayed::Entry(back) => assert_eq!(&back, entry),
            other => panic!("expected an entry, got {other:?}"),
        }
    }

    #[test]
    fn every_payload_round_trips() {
        let batch = TenantBatch::encode("acme", b"proto").expect("batch");
        let normal = EntryPayload::Normal(TenantBatchBytes::new(batch).expect("valid"));
        let nodes = BTreeMap::from([
            (id(1), "ourios-receiver-0".to_owned()),
            (id(2), "ourios-receiver-1".to_owned()),
            (id(3), "ourios-tail-0".to_owned()),
        ]);
        let joint = vec![BTreeSet::from([id(1)]), BTreeSet::from([id(1), id(2)])];
        let membership = Membership::new(joint, nodes).expect("membership");
        for payload in [
            EntryPayload::Blank,
            normal,
            EntryPayload::Membership(membership),
        ] {
            round_trip(&Entry {
                log_id: log_id(3, 7),
                payload,
            });
        }
    }

    #[test]
    fn a_membership_encodes_to_the_documented_bytes() {
        let nodes = BTreeMap::from([(id(1), "a".to_owned())]);
        let m = Membership::new(vec![BTreeSet::from([id(1)])], nodes).expect("membership");
        let uuid = *Uuid::from_u64_pair(0, 1).as_bytes();
        let mut expected = vec![1, 0, 1, 0];
        expected.extend_from_slice(&uuid);
        expected.extend_from_slice(&[1, 0]);
        expected.extend_from_slice(&uuid);
        expected.extend_from_slice(&[1, 0, b'a']);
        assert_eq!(encode_membership(&m).expect("encode"), expected);
    }

    #[test]
    fn malformed_memberships_are_refused() {
        let uuid = *Uuid::from_u64_pair(0, 1).as_bytes();
        let mut voter_without_node = vec![1, 0, 1, 0];
        voter_without_node.extend_from_slice(&uuid);
        voter_without_node.extend_from_slice(&[0, 0]);
        assert!(decode_membership(&voter_without_node).is_err());
        assert!(decode_membership(&[1, 0, 1]).is_err(), "truncated");
        assert!(decode_membership(&[0, 0, 0, 0, 9]).is_err(), "trailing");
    }

    #[test]
    fn markers_round_trip() {
        let id = log_id(2, 9);
        let wal = wal_id(&id);
        let cases = [
            (
                ReplicatedRecord::TruncateAfter(Some(wal)),
                Replayed::TruncateAfter(Some(id)),
            ),
            (
                ReplicatedRecord::TruncateAfter(None),
                Replayed::TruncateAfter(None),
            ),
            (ReplicatedRecord::Purge(wal), Replayed::Purge(id)),
            (ReplicatedRecord::Committed(wal), Replayed::Committed(id)),
        ];
        for (record, expected) in cases {
            let bytes = encode_marker(record).expect("encode");
            assert_eq!(decode(&bytes).expect("decode"), expected);
        }
    }
}
