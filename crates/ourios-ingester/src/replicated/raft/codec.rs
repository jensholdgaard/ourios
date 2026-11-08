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
//! node is refused as `openraft` refuses it, and a miner configuration
//! entry whose body `MinerConfig` refuses is refused as a wrong length is,
//! read or written (RFC 0065 §6.5).

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use openraft::EntryPayload;
use openraft::vote::leader_id_std::CommittedLeaderId;
use ourios_wal::{Control, EntryBody, RaftLogId, ReplicatedRecord};
use uuid::Uuid;

use super::miner_config::GroupMinerConfig;
use super::types::{Entry, GroupEntry, LogId, Membership, NodeId, TenantBatchBytes};

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

/// `control`, unless it is a miner configuration `MinerConfig` refuses.
fn checked(control: Control) -> io::Result<Control> {
    match control {
        Control::MinerConfig(body) => GroupMinerConfig::new(body)
            .map(|_| control)
            .map_err(|e| invalid(format!("miner configuration: {e}"))),
        Control::FlusherAssignment(_) | Control::Checkpoint { .. } | Control::IdBlock { .. } => {
            Ok(control)
        }
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
        EntryPayload::Normal(GroupEntry::Batch(batch)) => EntryBody::Normal(batch.as_bytes()),
        EntryPayload::Normal(GroupEntry::Control(control)) => {
            EntryBody::Control(checked(*control)?)
        }
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
                    let batch = TenantBatchBytes::new(bytes.to_vec()).map_err(invalid)?;
                    EntryPayload::Normal(GroupEntry::Batch(batch))
                }
                EntryBody::Membership(bytes) => EntryPayload::Membership(decode_membership(bytes)?),
                EntryBody::Control(control) => {
                    EntryPayload::Normal(GroupEntry::Control(checked(control)?))
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
    let (membership, rest) = take_membership(bytes)?;
    if !rest.is_empty() {
        return Err(invalid("trailing bytes after the membership"));
    }
    Ok(membership)
}

/// The membership body at the start of `bytes`, and the bytes after it.
pub(super) fn take_membership(bytes: &[u8]) -> io::Result<(Membership, &[u8])> {
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
    let membership = Membership::new(configs, nodes).map_err(invalid)?;
    Ok((membership, c.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ourios_wal::{Control, ReplicatedRecordError, TenantBatch};

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
        let normal = EntryPayload::Normal(GroupEntry::Batch(
            TenantBatchBytes::new(batch).expect("valid"),
        ));
        let flusher = Control::FlusherAssignment(*id(4).uuid().as_bytes());
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
            EntryPayload::Normal(GroupEntry::Control(flusher)),
            EntryPayload::Membership(membership),
        ] {
            round_trip(&Entry {
                log_id: log_id(3, 7),
                payload,
            });
        }
    }

    #[test]
    fn a_flusher_assignment_is_the_wal_codecs_control_record() {
        let flusher = Control::FlusherAssignment(*id(4).uuid().as_bytes());
        let entry = Entry {
            log_id: log_id(3, 7),
            payload: EntryPayload::Normal(GroupEntry::Control(flusher)),
        };
        let record = ReplicatedRecord::Entry {
            log_id: RaftLogId { term: 3, index: 7 },
            body: EntryBody::Control(flusher),
        };
        assert_eq!(
            encode_entry(&entry).expect("encode"),
            record.encode().expect("encode")
        );
    }

    #[test]
    fn an_unknown_control_kind_is_refused() {
        let flusher = Control::FlusherAssignment([7; 16]);
        let mut bytes = ReplicatedRecord::Entry {
            log_id: RaftLogId { term: 1, index: 1 },
            body: EntryBody::Control(flusher),
        }
        .encode()
        .expect("encode");
        bytes[17] = Control::HIGHEST_KIND + 1;
        let e = decode(&bytes).expect_err("an unknown kind");
        let unknown = ReplicatedRecordError::UnknownControlKind {
            found: Control::HIGHEST_KIND + 1,
        };
        assert_eq!(e.to_string(), unknown.to_string());
    }

    fn miner_config(similarity_threshold: f32) -> ourios_wal::MinerConfigV1 {
        ourios_wal::MinerConfigV1 {
            similarity_threshold,
            similarity_floor: 0.4,
            param_byte_limit: 256,
            prefix_depth: 2,
            max_node_children: 100,
            max_templates: 20_000,
            max_line_tokens: 512,
            upstream_templates: ourios_wal::UpstreamTemplates::Observe,
            upstream_template_byte_limit: 8192,
            upstream_association_limit: 4,
        }
    }

    /// A configuration `MinerConfig` accepts round-trips; one it refuses is
    /// neither written nor read, though the WAL codec alone takes it.
    #[test]
    fn a_miner_configuration_minerconfig_refuses_is_refused() {
        let entry = |config| Entry {
            log_id: log_id(3, 7),
            payload: EntryPayload::Normal(GroupEntry::Control(Control::MinerConfig(config))),
        };
        round_trip(&entry(miner_config(0.7)));

        let refused = Control::MinerConfig(miner_config(0.0));
        let e = encode_entry(&entry(miner_config(0.0))).expect_err("not written");
        assert!(e.to_string().contains("similarity_threshold"), "{e}");
        let bytes = ReplicatedRecord::Entry {
            log_id: RaftLogId { term: 3, index: 7 },
            body: EntryBody::Control(refused),
        }
        .encode()
        .expect("the WAL codec checks only the length and mode");
        let e = decode(&bytes).expect_err("not read");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(e.to_string().contains("similarity_threshold"), "{e}");
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
