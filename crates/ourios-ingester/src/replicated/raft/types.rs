//! The `openraft` type configuration of one replicated group (RFC 0065
//! §4.3, §6.1).
//!
//! A log id is `(term, index)`: the standard single-leader-per-term
//! election, so frame kind `0x04` carries no node id. The node id is the
//! replica's `UUIDv7` identity and the node data its `node_id`, so the
//! group's membership pairs the two. The application data is a raw
//! `TenantOtlpBatch` payload with the leader's time (§6.5, §6.7) or a
//! control entry (§6.5).

use std::fmt;

use ourios_wal::{
    BatchTime, Control, MinerConfigV1, RaftLogId, TenantBatch, TenantBatchError, UpstreamTemplates,
};
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

/// A tenant's batch and the leader's wall clock when it submitted it,
/// which stamps every audit event mining the batch emits (RFC 0065 §6.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedBatch {
    #[serde(with = "batch_time")]
    pub time: BatchTime,
    pub batch: TenantBatchBytes,
}

mod batch_time {
    use ourios_wal::BatchTime;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    #[allow(clippy::trivially_copy_pass_by_ref)] // serde's `with` passes a reference
    pub fn serialize<S: Serializer>(time: &BatchTime, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(time.nanos())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<BatchTime, D::Error> {
        let nanos = u64::deserialize(deserializer)?;
        BatchTime::from_nanos(nanos)
            .ok_or_else(|| D::Error::custom(format!("batch time {nanos} is above 2^63 - 1")))
    }
}

/// What a group's log entry applies: a tenant's batch, or a control entry
/// the leader appends beside the batches (RFC 0065 §6.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupEntry {
    Batch(TimedBatch),
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
    MinerConfig(#[serde(with = "MinerConfigV1Def")] MinerConfigV1),
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "MinerConfigV1")]
struct MinerConfigV1Def {
    similarity_threshold: f32,
    similarity_floor: f32,
    param_byte_limit: u32,
    prefix_depth: u8,
    max_node_children: u16,
    max_templates: u32,
    max_line_tokens: u16,
    #[serde(with = "UpstreamTemplatesDef")]
    upstream_templates: UpstreamTemplates,
    upstream_template_byte_limit: u32,
    upstream_association_limit: u16,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "UpstreamTemplates")]
enum UpstreamTemplatesDef {
    Ignore,
    Observe,
    Adopt,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "RaftLogId")]
struct RaftLogIdDef {
    term: u64,
    index: u64,
}

impl GroupEntry {
    /// `batch`, stamped with `time`.
    #[must_use]
    pub fn batch(time: BatchTime, batch: TenantBatchBytes) -> Self {
        Self::Batch(TimedBatch { time, batch })
    }
}

/// A batch at the epoch, for tests that do not look at its time.
#[cfg(test)]
impl From<TenantBatchBytes> for GroupEntry {
    fn from(batch: TenantBatchBytes) -> Self {
        Self::batch(BatchTime::default(), batch)
    }
}

impl fmt::Display for GroupEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Batch(timed) => timed.batch.fmt(f),
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
            Self::Control(Control::MinerConfig(config)) => write!(
                f,
                "MinerConfig(algorithm 1, prefix_depth {}, similarity_threshold {})",
                config.prefix_depth, config.similarity_threshold
            ),
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

    /// A batch's time survives `serde`, and one above `2^63 - 1` does not
    /// deserialise.
    #[test]
    fn a_timed_batch_round_trips_and_refuses_a_time_out_of_range() {
        let batch = TenantBatchBytes::new(TenantBatch::encode("acme", b"p").expect("encode"))
            .expect("valid");
        let entry = GroupEntry::batch(BatchTime::MAX, batch);
        let json = serde_json::to_string(&entry).expect("serialise");
        assert_eq!(serde_json::from_str::<GroupEntry>(&json).ok(), Some(entry));
        let above = json.replace(
            &BatchTime::MAX.nanos().to_string(),
            &(BatchTime::MAX.nanos() + 1).to_string(),
        );
        assert!(
            serde_json::from_str::<GroupEntry>(&above).is_err(),
            "{above}"
        );
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
    fn a_miner_configuration_displays_its_algorithm_and_round_trips() {
        let entry = GroupEntry::Control(Control::MinerConfig(MinerConfigV1 {
            similarity_threshold: 0.7,
            similarity_floor: 0.4,
            param_byte_limit: 256,
            prefix_depth: 2,
            max_node_children: 100,
            max_templates: 20_000,
            max_line_tokens: 512,
            upstream_templates: UpstreamTemplates::Adopt,
            upstream_template_byte_limit: 8192,
            upstream_association_limit: 4,
        }));
        assert_eq!(
            entry.to_string(),
            "MinerConfig(algorithm 1, prefix_depth 2, similarity_threshold 0.7)"
        );
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
