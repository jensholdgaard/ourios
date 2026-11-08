//! The `openraft` type configuration of one replicated group (RFC 0065
//! §4.3, §6.1).
//!
//! A log id is `(term, index)`: the standard single-leader-per-term
//! election, so frame kind `0x04` carries no node id. The node id is the
//! replica's `UUIDv7` identity and the node data its `node_id`, so the
//! group's membership pairs the two. The application data is a raw
//! `TenantOtlpBatch` payload (§6.7) or a control entry (§6.5).

use std::fmt;

use ourios_wal::{Control, RaftLogId, TenantBatch, TenantBatchError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A replica's Raft node id: the `UUIDv7` in its `replica.id` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(Uuid);

impl NodeId {
    #[must_use]
    pub fn uuid(self) -> Uuid {
        self.0
    }
}

impl From<Uuid> for NodeId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

/// `openraft`'s storage suite names its nodes by integer.
#[cfg(test)]
impl From<u64> for NodeId {
    fn from(n: u64) -> Self {
        Self(Uuid::from_u64_pair(0, n))
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(f)
    }
}

/// One `TenantOtlpBatch` payload (RFC 0046 §3.3), tenant prefix included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<u8>", into = "Vec<u8>")]
pub struct TenantBatchBytes(Vec<u8>);

impl TenantBatchBytes {
    /// # Errors
    ///
    /// [`TenantBatchError`] when the tenant prefix is malformed.
    pub fn new(payload: Vec<u8>) -> Result<Self, TenantBatchError> {
        TenantBatch::decode(&payload)?;
        Ok(Self(payload))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl TryFrom<Vec<u8>> for TenantBatchBytes {
    type Error = TenantBatchError;

    fn try_from(payload: Vec<u8>) -> Result<Self, Self::Error> {
        Self::new(payload)
    }
}

impl From<TenantBatchBytes> for Vec<u8> {
    fn from(batch: TenantBatchBytes) -> Self {
        batch.0
    }
}

impl fmt::Display for TenantBatchBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TenantOtlpBatch({} bytes)", self.0.len())
    }
}

/// What a group's log entry applies: a tenant's batch, or a control entry
/// the leader appends beside the batches (RFC 0065 §6.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupEntry {
    Batch(TenantBatchBytes),
    Control(#[serde(with = "ControlDef")] Control),
}

/// `openraft`'s `serde` feature wants the entry serialisable; the log and
/// the peer transport carry it in `ourios-wal`'s codec instead.
#[derive(Serialize, Deserialize)]
#[serde(remote = "Control")]
enum ControlDef {
    FlusherAssignment([u8; 16]),
    Checkpoint {
        proposer: [u8; 16],
        #[serde(with = "RaftLogIdDef")]
        cut: RaftLogId,
        highest_kind: u8,
    },
    IdBlock {
        after: u64,
        through: u64,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "RaftLogId")]
struct RaftLogIdDef {
    term: u64,
    index: u64,
}

impl From<TenantBatchBytes> for GroupEntry {
    fn from(batch: TenantBatchBytes) -> Self {
        Self::Batch(batch)
    }
}

impl fmt::Display for GroupEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Batch(batch) => batch.fmt(f),
            Self::Control(Control::FlusherAssignment(replica)) => {
                write!(f, "FlusherAssignment({})", Uuid::from_bytes(*replica))
            }
            Self::Control(Control::Checkpoint {
                proposer,
                cut,
                highest_kind,
            }) => write!(
                f,
                "Checkpoint({} at {}-{}, kind {highest_kind})",
                Uuid::from_bytes(*proposer),
                cut.term,
                cut.index
            ),
            Self::Control(Control::IdBlock { after, through }) => {
                write!(f, "IdBlock({after}, {through}]")
            }
        }
    }
}

openraft::declare_raft_types!(
    /// One replicated group's Raft types.
    pub RaftTypes:
        D = GroupEntry,
        R = (),
        NodeId = NodeId,
        Node = String,
        Term = u64,
        LeaderId = openraft::impls::leader_id_std::LeaderId<u64, NodeId>,
);

pub type Entry = <RaftTypes as openraft::RaftTypeConfig>::Entry;
pub type LogId = openraft::type_config::alias::LogIdOf<RaftTypes>;
pub type Membership = <RaftTypes as openraft::RaftTypeConfig>::Membership;
pub type Vote = <RaftTypes as openraft::RaftTypeConfig>::Vote;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_with_a_malformed_tenant_prefix_is_refused() {
        let good = TenantBatch::encode("acme", b"p").expect("encode");
        assert_eq!(TenantBatchBytes::new(good.clone()).map(Vec::from), Ok(good));
        assert_eq!(
            TenantBatchBytes::new(vec![0, 0]),
            Err(TenantBatchError::EmptyTenant)
        );
        assert!(serde_json::from_str::<TenantBatchBytes>("[0,0]").is_err());
    }

    #[test]
    fn a_flusher_assignment_displays_its_replica() {
        let uuid = Uuid::from_u128(0x0190_b3c8_1a2b_7c3d_9e4f_5060_7080_a0b0);
        let entry = GroupEntry::Control(Control::FlusherAssignment(*uuid.as_bytes()));
        assert_eq!(
            entry.to_string(),
            "FlusherAssignment(0190b3c8-1a2b-7c3d-9e4f-50607080a0b0)"
        );
        let json = serde_json::to_string(&entry).expect("serialise");
        assert_eq!(serde_json::from_str::<GroupEntry>(&json).ok(), Some(entry));
    }

    #[test]
    fn a_checkpoint_displays_its_proposer_and_cut() {
        let uuid = Uuid::from_u128(0x0190_b3c8_1a2b_7c3d_9e4f_5060_7080_a0b0);
        let entry = GroupEntry::Control(Control::Checkpoint {
            proposer: *uuid.as_bytes(),
            cut: RaftLogId { term: 3, index: 41 },
            highest_kind: 2,
        });
        assert_eq!(
            entry.to_string(),
            "Checkpoint(0190b3c8-1a2b-7c3d-9e4f-50607080a0b0 at 3-41, kind 2)"
        );
        let json = serde_json::to_string(&entry).expect("serialise");
        assert_eq!(serde_json::from_str::<GroupEntry>(&json).ok(), Some(entry));
    }

    #[test]
    fn an_id_block_displays_its_range() {
        let entry = GroupEntry::Control(Control::IdBlock {
            after: 100,
            through: 200,
        });
        assert_eq!(entry.to_string(), "IdBlock(100, 200]");
        let json = serde_json::to_string(&entry).expect("serialise");
        assert_eq!(serde_json::from_str::<GroupEntry>(&json).ok(), Some(entry));
    }

    #[test]
    fn a_node_id_displays_as_its_uuid() {
        let uuid = Uuid::from_u128(0x0190_b3c8_1a2b_7c3d_9e4f_5060_7080_a0b0);
        assert_eq!(
            NodeId::from(uuid).to_string(),
            "0190b3c8-1a2b-7c3d-9e4f-50607080a0b0"
        );
    }
}
