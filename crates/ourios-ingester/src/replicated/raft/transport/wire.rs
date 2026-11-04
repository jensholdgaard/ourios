//! The `ourios.wal.v1.Replication` messages (RFC 0065 §6.8).
//!
//! The messages are declared here, not generated, and every field is
//! explicit, so the wire never moves with `openraft`'s serde layout. An
//! entry travels as its frame kind `0x04` payload, the bytes the group
//! log writes, and a membership in that codec's membership encoding.

use std::fmt;
use std::io;

use openraft::impls::leader_id_std::LeaderId;
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, SnapshotResponse, VoteRequest, VoteResponse,
};
use openraft::type_config::alias::{SnapshotMetaOf, StoredMembershipOf};
use prost::Message as _;
use uuid::Uuid;

use super::super::codec::{self, Replayed};
use super::super::types::{Entry, LogId, NodeId, RaftTypes, Vote};

/// A request or response the wire cannot carry, or one that arrived
/// malformed.
#[derive(Debug)]
pub struct Malformed(String);

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed replication message: {}", self.0)
    }
}

impl std::error::Error for Malformed {}

impl From<io::Error> for Malformed {
    fn from(e: io::Error) -> Self {
        Self(e.to_string())
    }
}

/// A response that decodes to its `openraft` counterpart.
pub trait Reply {
    type Raft;

    /// # Errors
    ///
    /// [`Malformed`] when a field is missing or does not decode.
    fn into_raft(self) -> Result<Self::Raft, Malformed>;
}

fn required<T>(field: Option<T>, name: &str) -> Result<T, Malformed> {
    field.ok_or_else(|| Malformed(format!("{name} is missing")))
}

#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub struct WireLogId {
    #[prost(uint64, tag = "1")]
    pub term: u64,
    #[prost(uint64, tag = "2")]
    pub index: u64,
}

impl From<&LogId> for WireLogId {
    fn from(id: &LogId) -> Self {
        let id = codec::wal_id(id);
        Self {
            term: id.term,
            index: id.index,
        }
    }
}

impl From<WireLogId> for LogId {
    fn from(id: WireLogId) -> Self {
        codec::raft_id(ourios_wal::RaftLogId {
            term: id.term,
            index: id.index,
        })
    }
}

fn wire_id(id: Option<&LogId>) -> Option<WireLogId> {
    id.map(WireLogId::from)
}

fn raft_id(id: Option<WireLogId>) -> Option<LogId> {
    id.map(LogId::from)
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct WireVote {
    #[prost(uint64, tag = "1")]
    pub term: u64,
    /// The candidate's UUID, 16 big-endian bytes.
    #[prost(bytes = "vec", tag = "2")]
    pub voted_for: Vec<u8>,
    #[prost(bool, tag = "3")]
    pub committed: bool,
}

impl From<&Vote> for WireVote {
    fn from(vote: &Vote) -> Self {
        Self {
            term: vote.leader_id.term,
            voted_for: vote.leader_id.voted_for.uuid().as_bytes().to_vec(),
            committed: vote.committed,
        }
    }
}

impl TryFrom<WireVote> for Vote {
    type Error = Malformed;

    fn try_from(vote: WireVote) -> Result<Self, Malformed> {
        let uuid = Uuid::from_slice(&vote.voted_for).map_err(|e| Malformed(e.to_string()))?;
        Ok(Self {
            leader_id: LeaderId {
                term: vote.term,
                voted_for: NodeId::from(uuid),
            },
            committed: vote.committed,
        })
    }
}

fn vote_of(vote: Option<WireVote>) -> Result<Vote, Malformed> {
    required(vote, "vote")?.try_into()
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireMembership {
    #[prost(message, optional, tag = "1")]
    pub log_id: Option<WireLogId>,
    #[prost(bytes = "vec", tag = "2")]
    pub config: Vec<u8>,
}

impl WireMembership {
    fn new(stored: &StoredMembershipOf<RaftTypes>) -> Result<Self, Malformed> {
        Ok(Self {
            log_id: wire_id(stored.log_id().as_ref()),
            config: codec::encode_membership(stored.membership())?,
        })
    }

    fn into_raft(self) -> Result<StoredMembershipOf<RaftTypes>, Malformed> {
        let membership = codec::decode_membership(&self.config)?;
        Ok(StoredMembershipOf::<RaftTypes>::new(
            raft_id(self.log_id),
            membership,
        ))
    }
}

/// The largest message a peer sends or accepts: one maximum-sized WAL
/// frame, which an entry's payload never exceeds, plus room for the
/// envelope (header, vote, log ids, field tags).
pub const MAX_MESSAGE_BYTES: usize = ourios_wal::MAX_FRAME_BYTES + 64 * 1024;

/// The group and replica a request is addressed to.
#[derive(Clone, Copy, Debug)]
pub struct Recipient {
    pub group: u32,
    pub node: NodeId,
}

/// What every request carries before its body: the protocol version
/// (B22), the group, and the replica it is meant for.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Header {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(uint32, tag = "2")]
    pub group: u32,
    /// The recipient's UUID, 16 big-endian bytes.
    #[prost(bytes = "vec", tag = "3")]
    pub target: Vec<u8>,
}

impl Header {
    fn to(recipient: Recipient) -> Self {
        Self {
            version: super::VERSION,
            group: recipient.group,
            target: recipient.node.uuid().as_bytes().to_vec(),
        }
    }
}

fn decode_entry(bytes: &[u8]) -> Result<Entry, Malformed> {
    match codec::decode(bytes)? {
        Replayed::Entry(entry) => Ok(entry),
        other => Err(Malformed(format!("expected an entry, got {other:?}"))),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct AppendRequest {
    #[prost(message, optional, tag = "1")]
    pub header: Option<Header>,
    #[prost(message, optional, tag = "2")]
    pub vote: Option<WireVote>,
    #[prost(message, optional, tag = "3")]
    pub prev_log_id: Option<WireLogId>,
    /// Each entry's frame kind `0x04` payload.
    #[prost(bytes = "vec", repeated, tag = "4")]
    pub entries: Vec<Vec<u8>>,
    #[prost(message, optional, tag = "5")]
    pub leader_commit: Option<WireLogId>,
}

impl AppendRequest {
    /// The request for `rpc`, holding the longest prefix of its entries
    /// that fits [`MAX_MESSAGE_BYTES`] and always at least one, with the
    /// last log id it carries when that prefix is not all of them.
    ///
    /// # Errors
    ///
    /// [`Malformed`] when an entry has no frame encoding.
    pub fn bounded(
        to: Recipient,
        rpc: &AppendEntriesRequest<RaftTypes>,
    ) -> Result<(Self, Option<LogId>), Malformed> {
        let mut request = Self {
            header: Some(Header::to(to)),
            vote: Some(WireVote::from(&rpc.vote)),
            prev_log_id: wire_id(rpc.prev_log_id.as_ref()),
            entries: Vec::new(),
            leader_commit: wire_id(rpc.leader_commit.as_ref()),
        };
        let mut len = request.encoded_len();
        let mut last = None;
        for entry in &rpc.entries {
            let bytes = codec::encode_entry(entry)?;
            len += prost::encoding::bytes::encoded_len(4, &bytes);
            if len > MAX_MESSAGE_BYTES && last.is_some() {
                return Ok((request, last));
            }
            request.entries.push(bytes);
            last = Some(entry.log_id);
        }
        Ok((request, None))
    }

    /// # Errors
    ///
    /// [`Malformed`] when a field is missing or an entry does not decode.
    pub fn into_raft(self) -> Result<AppendEntriesRequest<RaftTypes>, Malformed> {
        Ok(AppendEntriesRequest {
            vote: vote_of(self.vote)?,
            prev_log_id: raft_id(self.prev_log_id),
            entries: self
                .entries
                .iter()
                .map(|bytes| decode_entry(bytes))
                .collect::<Result<_, _>>()?,
            leader_commit: raft_id(self.leader_commit),
        })
    }
}

/// `PartialSuccess`'s matched log id, which may itself be absent.
#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub struct Matched {
    #[prost(message, optional, tag = "1")]
    pub log_id: Option<WireLogId>,
}

#[derive(Clone, PartialEq, Eq, prost::Oneof)]
pub enum AppendOutcome {
    #[prost(message, tag = "1")]
    Success(()),
    #[prost(message, tag = "2")]
    PartialSuccess(Matched),
    #[prost(message, tag = "3")]
    Conflict(()),
    #[prost(message, tag = "4")]
    HigherVote(WireVote),
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct AppendResponse {
    #[prost(oneof = "AppendOutcome", tags = "1, 2, 3, 4")]
    pub outcome: Option<AppendOutcome>,
}

impl From<&AppendEntriesResponse<RaftTypes>> for AppendResponse {
    fn from(response: &AppendEntriesResponse<RaftTypes>) -> Self {
        let outcome = match response {
            AppendEntriesResponse::Success => AppendOutcome::Success(()),
            AppendEntriesResponse::PartialSuccess(id) => AppendOutcome::PartialSuccess(Matched {
                log_id: wire_id(id.as_ref()),
            }),
            AppendEntriesResponse::Conflict => AppendOutcome::Conflict(()),
            AppendEntriesResponse::HigherVote(vote) => {
                AppendOutcome::HigherVote(WireVote::from(vote))
            }
        };
        Self {
            outcome: Some(outcome),
        }
    }
}

impl Reply for AppendResponse {
    type Raft = AppendEntriesResponse<RaftTypes>;

    fn into_raft(self) -> Result<AppendEntriesResponse<RaftTypes>, Malformed> {
        Ok(match required(self.outcome, "outcome")? {
            AppendOutcome::Success(()) => AppendEntriesResponse::Success,
            AppendOutcome::PartialSuccess(m) => {
                AppendEntriesResponse::PartialSuccess(raft_id(m.log_id))
            }
            AppendOutcome::Conflict(()) => AppendEntriesResponse::Conflict,
            AppendOutcome::HigherVote(vote) => AppendEntriesResponse::HigherVote(vote.try_into()?),
        })
    }
}

/// A vote or pre-vote request; the method tells them apart.
#[derive(Clone, PartialEq, prost::Message)]
pub struct WireVoteRequest {
    #[prost(message, optional, tag = "1")]
    pub header: Option<Header>,
    #[prost(message, optional, tag = "2")]
    pub vote: Option<WireVote>,
    #[prost(message, optional, tag = "3")]
    pub last_log_id: Option<WireLogId>,
    #[prost(message, optional, tag = "4")]
    pub leadership_transfer_from: Option<WireVote>,
}

impl WireVoteRequest {
    #[must_use]
    pub fn new(to: Recipient, rpc: &VoteRequest<RaftTypes>) -> Self {
        Self {
            header: Some(Header::to(to)),
            vote: Some(WireVote::from(&rpc.vote)),
            last_log_id: wire_id(rpc.last_log_id.as_ref()),
            leadership_transfer_from: rpc.leadership_transfer_from.as_ref().map(WireVote::from),
        }
    }

    /// # Errors
    ///
    /// [`Malformed`] when the vote is missing or malformed.
    pub fn into_raft(self) -> Result<VoteRequest<RaftTypes>, Malformed> {
        Ok(VoteRequest {
            vote: vote_of(self.vote)?,
            last_log_id: raft_id(self.last_log_id),
            leadership_transfer_from: self
                .leadership_transfer_from
                .map(Vote::try_from)
                .transpose()?,
        })
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireVoteResponse {
    #[prost(message, optional, tag = "1")]
    pub vote: Option<WireVote>,
    #[prost(bool, tag = "2")]
    pub vote_granted: bool,
    #[prost(message, optional, tag = "3")]
    pub last_log_id: Option<WireLogId>,
}

impl From<&VoteResponse<RaftTypes>> for WireVoteResponse {
    fn from(response: &VoteResponse<RaftTypes>) -> Self {
        Self {
            vote: Some(WireVote::from(&response.vote)),
            vote_granted: response.vote_granted,
            last_log_id: wire_id(response.last_log_id.as_ref()),
        }
    }
}

impl Reply for WireVoteResponse {
    type Raft = VoteResponse<RaftTypes>;

    fn into_raft(self) -> Result<VoteResponse<RaftTypes>, Malformed> {
        Ok(VoteResponse {
            vote: vote_of(self.vote)?,
            vote_granted: self.vote_granted,
            last_log_id: raft_id(self.last_log_id),
        })
    }
}

/// A metadata-only snapshot (§4.3): the checkpoint carries no data.
#[derive(Clone, PartialEq, prost::Message)]
pub struct SnapshotRequest {
    #[prost(message, optional, tag = "1")]
    pub header: Option<Header>,
    #[prost(message, optional, tag = "2")]
    pub vote: Option<WireVote>,
    #[prost(message, optional, tag = "3")]
    pub last_log_id: Option<WireLogId>,
    #[prost(message, optional, tag = "4")]
    pub membership: Option<WireMembership>,
}

impl SnapshotRequest {
    /// # Errors
    ///
    /// [`Malformed`] when the membership has no encoding.
    pub fn new(
        to: Recipient,
        vote: &Vote,
        meta: &SnapshotMetaOf<RaftTypes>,
    ) -> Result<Self, Malformed> {
        Ok(Self {
            header: Some(Header::to(to)),
            vote: Some(WireVote::from(vote)),
            last_log_id: wire_id(meta.last_log_id.as_ref()),
            membership: Some(WireMembership::new(&meta.last_membership)?),
        })
    }

    /// # Errors
    ///
    /// [`Malformed`] when a field is missing or does not decode.
    pub fn into_raft(self) -> Result<(Vote, SnapshotMetaOf<RaftTypes>), Malformed> {
        let meta = SnapshotMetaOf::<RaftTypes> {
            last_log_id: raft_id(self.last_log_id),
            last_membership: required(self.membership, "membership")?.into_raft()?,
        };
        Ok((vote_of(self.vote)?, meta))
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireSnapshotResponse {
    #[prost(message, optional, tag = "1")]
    pub vote: Option<WireVote>,
}

impl From<&SnapshotResponse<RaftTypes>> for WireSnapshotResponse {
    fn from(response: &SnapshotResponse<RaftTypes>) -> Self {
        Self {
            vote: Some(WireVote::from(&response.vote)),
        }
    }
}

impl Reply for WireSnapshotResponse {
    type Raft = SnapshotResponse<RaftTypes>;

    fn into_raft(self) -> Result<SnapshotResponse<RaftTypes>, Malformed> {
        Ok(SnapshotResponse::new(vote_of(self.vote)?))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use openraft::EntryPayload;
    use ourios_wal::TenantBatch;
    use proptest::prelude::*;
    use prost::Message;

    use super::*;
    use crate::replicated::raft::types::{Membership, TenantBatchBytes};

    fn through_wire<M: Message + Default>(message: &M) -> M {
        M::decode(message.encode_to_vec().as_slice()).expect("decode")
    }

    fn node(n: u64) -> NodeId {
        NodeId::from(Uuid::from_u64_pair(0x0190_b3c8_1a2b_7c3d, n))
    }

    fn log_id() -> impl Strategy<Value = LogId> {
        (any::<u64>(), any::<u64>())
            .prop_map(|(term, index)| LogId::from(WireLogId { term, index }))
    }

    fn vote() -> impl Strategy<Value = Vote> {
        (any::<u64>(), any::<u64>(), any::<bool>()).prop_map(|(term, n, committed)| Vote {
            leader_id: LeaderId {
                term,
                voted_for: node(n),
            },
            committed,
        })
    }

    fn membership() -> impl Strategy<Value = Membership> {
        proptest::collection::btree_set(0..8_u64, 1..5).prop_map(|ids| {
            let voters: BTreeSet<_> = ids.iter().copied().map(node).collect();
            let nodes: BTreeMap<_, _> = ids
                .iter()
                .map(|&n| (node(n), format!("ourios-receiver-{n}:4320")))
                .collect();
            Membership::new(vec![voters], nodes).expect("membership")
        })
    }

    fn entry() -> impl Strategy<Value = Entry> {
        let normal = proptest::collection::vec(any::<u8>(), 0..64).prop_map(|body| {
            let batch = TenantBatch::encode("acme", &body).expect("batch");
            EntryPayload::Normal(TenantBatchBytes::new(batch).expect("valid"))
        });
        let payload = prop_oneof![
            Just(EntryPayload::Blank),
            normal,
            membership().prop_map(EntryPayload::Membership),
        ];
        (log_id(), payload).prop_map(|(log_id, payload)| Entry { log_id, payload })
    }

    fn stored_membership() -> impl Strategy<Value = StoredMembershipOf<RaftTypes>> {
        prop_oneof![
            Just(StoredMembershipOf::<RaftTypes>::default()),
            (proptest::option::of(log_id()), membership())
                .prop_map(|(id, m)| StoredMembershipOf::<RaftTypes>::new(id, m)),
        ]
    }

    fn recipient() -> impl Strategy<Value = Recipient> {
        (any::<u32>(), any::<u64>()).prop_map(|(group, n)| Recipient {
            group,
            node: node(n),
        })
    }

    fn append_response() -> impl Strategy<Value = AppendEntriesResponse<RaftTypes>> {
        prop_oneof![
            Just(AppendEntriesResponse::Success),
            proptest::option::of(log_id()).prop_map(AppendEntriesResponse::PartialSuccess),
            Just(AppendEntriesResponse::Conflict),
            vote().prop_map(AppendEntriesResponse::HigherVote),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: ourios_testgen::proptest_cases(256),
            ..ProptestConfig::default()
        })]

        #[test]
        fn an_append_request_round_trips(
            to in recipient(),
            vote in vote(),
            prev_log_id in proptest::option::of(log_id()),
            entries in proptest::collection::vec(entry(), 0..6),
            leader_commit in proptest::option::of(log_id()),
        ) {
            let rpc = AppendEntriesRequest::<RaftTypes> { vote, prev_log_id, entries, leader_commit };
            let (request, cut) = AppendRequest::bounded(to, &rpc).expect("encode");
            prop_assert_eq!(cut, None);
            let wire = through_wire(&request);
            prop_assert_eq!(wire.header.clone(), Some(Header::to(to)));
            let back = wire.into_raft().expect("decode");
            prop_assert_eq!(back.vote, rpc.vote);
            prop_assert_eq!(back.prev_log_id, rpc.prev_log_id);
            prop_assert_eq!(back.entries, rpc.entries);
            prop_assert_eq!(back.leader_commit, rpc.leader_commit);
        }

        #[test]
        fn an_append_response_round_trips(response in append_response()) {
            let wire = through_wire(&AppendResponse::from(&response));
            prop_assert_eq!(wire.into_raft().expect("decode"), response);
        }

        #[test]
        fn a_vote_request_round_trips(
            to in recipient(),
            vote in vote(),
            last_log_id in proptest::option::of(log_id()),
            transfer in proptest::option::of(vote()),
        ) {
            let rpc = VoteRequest::<RaftTypes> { vote, last_log_id, leadership_transfer_from: transfer };
            let wire = through_wire(&WireVoteRequest::new(to, &rpc));
            prop_assert_eq!(wire.header.clone(), Some(Header::to(to)));
            prop_assert_eq!(wire.into_raft().expect("decode"), rpc);
        }

        #[test]
        fn a_vote_response_round_trips(
            vote in vote(),
            granted in any::<bool>(),
            last_log_id in proptest::option::of(log_id()),
        ) {
            let response = VoteResponse::<RaftTypes>::new(vote, last_log_id, granted);
            let wire = through_wire(&WireVoteResponse::from(&response));
            prop_assert_eq!(wire.into_raft().expect("decode"), response);
        }

        #[test]
        fn a_snapshot_request_round_trips(
            to in recipient(),
            vote in vote(),
            last_log_id in proptest::option::of(log_id()),
            last_membership in stored_membership(),
        ) {
            let meta = SnapshotMetaOf::<RaftTypes> { last_log_id, last_membership };
            let wire = through_wire(&SnapshotRequest::new(to, &vote, &meta).expect("encode"));
            prop_assert_eq!(wire.header.clone(), Some(Header::to(to)));
            prop_assert_eq!(wire.into_raft().expect("decode"), (vote, meta));
        }

        #[test]
        fn a_snapshot_response_round_trips(vote in vote()) {
            let response = SnapshotResponse::<RaftTypes>::new(vote);
            let wire = through_wire(&WireSnapshotResponse::from(&response));
            prop_assert_eq!(wire.into_raft().expect("decode"), response);
        }
    }

    fn large_entry(index: u64, bytes: usize) -> Entry {
        let batch = TenantBatch::encode("acme", &vec![7; bytes]).expect("batch");
        Entry {
            log_id: LogId::from(WireLogId { term: 1, index }),
            payload: EntryPayload::Normal(TenantBatchBytes::new(batch).expect("valid")),
        }
    }

    fn append_of(entries: Vec<Entry>) -> AppendEntriesRequest<RaftTypes> {
        AppendEntriesRequest {
            vote: Vote::new_committed(1, node(1)),
            prev_log_id: None,
            entries,
            leader_commit: None,
        }
    }

    fn to() -> Recipient {
        Recipient {
            group: 1,
            node: node(2),
        }
    }

    #[test]
    fn an_append_holds_the_longest_prefix_under_the_byte_budget() {
        let seven_mib = 7 * 1024 * 1024;
        let rpc = append_of((0..3).map(|i| large_entry(i, seven_mib)).collect());
        let (request, cut) = AppendRequest::bounded(to(), &rpc).expect("encode");
        assert_eq!(request.entries.len(), 2);
        assert_eq!(cut, Some(rpc.entries[1].log_id));
        assert!(request.encoded_len() <= MAX_MESSAGE_BYTES);
    }

    #[test]
    fn a_maximum_sized_entry_fits_one_message() {
        let frame = ourios_wal::MAX_FRAME_BYTES;
        let rpc = append_of(vec![large_entry(0, frame - 1024)]);
        let (request, cut) = AppendRequest::bounded(to(), &rpc).expect("encode");
        assert_eq!((request.entries.len(), cut), (1, None));
        assert!(request.encoded_len() <= MAX_MESSAGE_BYTES);
    }

    #[test]
    fn a_message_without_its_vote_or_outcome_is_refused() {
        assert!(WireVoteRequest::default().into_raft().is_err());
        assert!(WireVoteResponse::default().into_raft().is_err());
        assert!(AppendRequest::default().into_raft().is_err());
        assert!(AppendResponse::default().into_raft().is_err());
        assert!(SnapshotRequest::default().into_raft().is_err());
        let short = WireVote {
            term: 1,
            voted_for: vec![1, 2, 3],
            committed: false,
        };
        assert!(Vote::try_from(short).is_err());
    }

    #[test]
    fn an_entry_slot_holding_a_marker_is_refused() {
        let marker =
            codec::encode_marker(ourios_wal::ReplicatedRecord::Purge(ourios_wal::RaftLogId {
                term: 1,
                index: 2,
            }))
            .expect("marker");
        assert!(decode_entry(&marker).is_err());
    }
}
