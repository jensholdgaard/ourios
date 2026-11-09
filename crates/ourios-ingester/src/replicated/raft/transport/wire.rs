//! The `ourios.wal.v1.Replication` messages (RFC 0065 §6.8).
//!
//! The messages are declared here, not generated, and every field is
//! explicit, so the wire never moves with `openraft`'s serde layout. An
//! entry travels as its frame kind `0x04` payload, the bytes the group
//! log writes, and a membership in that codec's membership encoding.

use std::borrow::Borrow;
use std::fmt;
use std::io;

use openraft::impls::leader_id_std::LeaderId;
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, AppendError, AppendResult, SnapshotResponse,
    StreamAppendError, StreamAppendResult, VoteRequest, VoteResponse,
};
use openraft::type_config::alias::{SnapshotMetaOf, SnapshotOf};
use prost::Message as _;
use uuid::Uuid;

use super::super::codec::{self, Replayed};
use super::super::snapshot_record::{self, GroupSnapshot, GroupState};
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

/// The vote of an append or snapshot, which only an elected leader sends:
/// `openraft` asserts that the vote it follows is committed.
fn leader_vote_of(vote: Option<WireVote>) -> Result<Vote, Malformed> {
    let vote = vote_of(vote)?;
    if vote.committed {
        Ok(vote)
    } else {
        Err(Malformed(format!(
            "{vote} is not a leader's committed vote"
        )))
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
    if bytes.len() > ourios_wal::MAX_FRAME_BYTES {
        return Err(Malformed(format!(
            "an entry of {} bytes exceeds the WAL frame limit",
            bytes.len()
        )));
    }
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
        let mut pieces = Pieces::new(to, rpc);
        let request = pieces.piece()?;
        let cut = if pieces.done() { None } else { pieces.prev };
        Ok((request, cut))
    }

    /// The requests that carry all of `rpc`'s entries in order, each
    /// holding the longest run that fits [`MAX_MESSAGE_BYTES`] and
    /// following the one before it, so each is an append of its own.
    ///
    /// # Errors
    ///
    /// [`Malformed`] when an entry has no frame encoding.
    #[cfg(test)]
    pub fn split(
        to: Recipient,
        rpc: &AppendEntriesRequest<RaftTypes>,
    ) -> Result<Vec<Self>, Malformed> {
        Pieces::new(to, rpc).collect()
    }

    /// # Errors
    ///
    /// [`Malformed`] when a field is missing or an entry does not decode.
    pub fn into_raft(self) -> Result<AppendEntriesRequest<RaftTypes>, Malformed> {
        let rpc = AppendEntriesRequest {
            vote: leader_vote_of(self.vote)?,
            prev_log_id: raft_id(self.prev_log_id),
            entries: self
                .entries
                .iter()
                .map(|bytes| decode_entry(bytes))
                .collect::<Result<_, _>>()?,
            leader_commit: raft_id(self.leader_commit),
        };
        check_segment(&rpc)?;
        Ok(rpc)
    }
}

/// An append cut into messages under [`MAX_MESSAGE_BYTES`], encoded one
/// piece at a time so that only the piece in hand is held encoded. `R`
/// is the append, owned or borrowed.
pub struct Pieces<R> {
    to: Recipient,
    rpc: R,
    /// The last log id of the pieces so far.
    prev: Option<LogId>,
    /// The first entry no piece carries yet.
    next: usize,
    /// Whether a piece has been taken: an append without entries is one.
    started: bool,
}

impl<R> Pieces<R>
where
    R: Borrow<AppendEntriesRequest<RaftTypes>>,
{
    pub fn new(to: Recipient, rpc: R) -> Self {
        Self {
            to,
            prev: rpc.borrow().prev_log_id,
            rpc,
            next: 0,
            started: false,
        }
    }

    fn done(&self) -> bool {
        self.started && self.next == self.rpc.borrow().entries.len()
    }

    /// The next piece: the entries after the last piece that fit, and at
    /// least one while any remain.
    fn piece(&mut self) -> Result<AppendRequest, Malformed> {
        let rpc = self.rpc.borrow();
        let mut request = AppendRequest {
            header: Some(Header::to(self.to)),
            vote: Some(WireVote::from(&rpc.vote)),
            prev_log_id: wire_id(self.prev.as_ref()),
            entries: Vec::new(),
            leader_commit: wire_id(rpc.leader_commit.as_ref()),
        };
        let (mut prev, mut next) = (self.prev, self.next);
        let mut len = request.encoded_len();
        for entry in &rpc.entries[next..] {
            let bytes = codec::encode_entry(entry)?;
            len += prost::encoding::bytes::encoded_len(4, &bytes);
            if len > MAX_MESSAGE_BYTES && !request.entries.is_empty() {
                break;
            }
            request.entries.push(bytes);
            prev = Some(entry.log_id);
            next += 1;
        }
        (self.prev, self.next, self.started) = (prev, next, true);
        Ok(request)
    }
}

impl<R> Iterator for Pieces<R>
where
    R: Borrow<AppendEntriesRequest<RaftTypes>>,
{
    type Item = Result<AppendRequest, Malformed>;

    /// `None` once every entry is carried; an `Err` leaves the pieces
    /// where they were.
    fn next(&mut self) -> Option<Self::Item> {
        if self.done() {
            return None;
        }
        Some(self.piece())
    }
}

/// Refuse a log id from a term after the sender's vote: no leader can
/// send one, and `openraft` asserts against it.
fn within_vote(id: Option<&LogId>, vote: &Vote) -> Result<(), Malformed> {
    match id {
        Some(id) if id.leader_id.term > vote.leader_id.term => Err(Malformed(format!(
            "log id {id} is after the sender's vote {vote}"
        ))),
        _ => Ok(()),
    }
}

/// Refuse what a leader never sends and `openraft` or the group log
/// would fail on rather than reject: entries that do not follow
/// `prev_log_id` one index at a time in non-decreasing terms, or a log id
/// after the sender's vote.
fn check_segment(rpc: &AppendEntriesRequest<RaftTypes>) -> Result<(), Malformed> {
    within_vote(rpc.prev_log_id.as_ref(), &rpc.vote)?;
    let mut last = rpc.prev_log_id;
    for entry in &rpc.entries {
        let id = entry.log_id;
        let next = last.map_or(Some(0), |l| l.index.checked_add(1));
        let term_back = last.is_some_and(|l| id.leader_id.term < l.leader_id.term);
        if next != Some(id.index) || term_back {
            return Err(Malformed(format!("entry {id} does not follow {last:?}")));
        }
        within_vote(Some(&id), &rpc.vote)?;
        last = Some(id);
    }
    Ok(())
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

/// The leader's reading of `response` to an append that carried the log
/// from `prev` through `last`: a success matches through `last`, and a
/// conflict is on `prev`. The wire carries no conflict hint.
///
/// # Errors
///
/// [`Malformed`] for a conflict on an append without a `prev_log_id`,
/// which every log holds.
pub fn append_result(
    response: &AppendEntriesResponse<RaftTypes>,
    prev: Option<LogId>,
    last: Option<LogId>,
) -> Result<AppendResult<RaftTypes>, Malformed> {
    Ok(match response {
        AppendEntriesResponse::Success => Ok(last),
        AppendEntriesResponse::PartialSuccess(id) => Ok(*id),
        AppendEntriesResponse::Conflict => Err(AppendError::Conflict {
            log_id: required(prev, "a conflict's prev_log_id")?,
            hint: None,
        }),
        AppendEntriesResponse::HigherVote(vote) => Err(AppendError::HigherVote(*vote)),
    })
}

/// A `StreamAppend` answer to one request of the stream, in the order
/// the requests were sent.
#[derive(Clone, PartialEq, Eq, prost::Oneof)]
pub enum StreamOutcome {
    /// The follower's log matches through this log id.
    #[prost(message, tag = "1")]
    Matched(Matched),
    /// The follower's log does not hold this `prev_log_id`.
    #[prost(message, tag = "2")]
    Conflict(WireLogId),
    #[prost(message, tag = "3")]
    HigherVote(WireVote),
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct StreamAppendResponse {
    #[prost(oneof = "StreamOutcome", tags = "1, 2, 3")]
    pub outcome: Option<StreamOutcome>,
}

impl From<&StreamAppendResult<RaftTypes>> for StreamAppendResponse {
    fn from(result: &StreamAppendResult<RaftTypes>) -> Self {
        let outcome = match result {
            Ok(id) => StreamOutcome::Matched(Matched {
                log_id: wire_id(id.as_ref()),
            }),
            Err(StreamAppendError::Conflict(id)) => StreamOutcome::Conflict(WireLogId::from(id)),
            Err(StreamAppendError::HigherVote(vote)) => {
                StreamOutcome::HigherVote(WireVote::from(vote))
            }
        };
        Self {
            outcome: Some(outcome),
        }
    }
}

impl Reply for StreamAppendResponse {
    type Raft = StreamAppendResult<RaftTypes>;

    fn into_raft(self) -> Result<StreamAppendResult<RaftTypes>, Malformed> {
        Ok(match required(self.outcome, "outcome")? {
            StreamOutcome::Matched(m) => Ok(raft_id(m.log_id)),
            StreamOutcome::Conflict(id) => Err(StreamAppendError::Conflict(id.into())),
            StreamOutcome::HigherVote(vote) => Err(StreamAppendError::HigherVote(vote.try_into()?)),
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

/// A snapshot (§6.1): the group's snapshot record, whose bytes carry the
/// metadata and the group state at it.
#[derive(Clone, PartialEq, prost::Message)]
pub struct SnapshotRequest {
    #[prost(message, optional, tag = "1")]
    pub header: Option<Header>,
    #[prost(message, optional, tag = "2")]
    pub vote: Option<WireVote>,
    /// Tags 3 and 4 held the metadata alone, before the record carried it.
    #[prost(bytes = "vec", tag = "5")]
    pub record: Vec<u8>,
}

impl SnapshotRequest {
    /// # Errors
    ///
    /// [`Malformed`] when the membership has no encoding.
    pub fn new(
        to: Recipient,
        vote: &Vote,
        snapshot: SnapshotOf<RaftTypes, GroupState>,
    ) -> Result<Self, Malformed> {
        Ok(Self {
            header: Some(Header::to(to)),
            vote: Some(WireVote::from(vote)),
            record: snapshot_record::encode(&GroupSnapshot::from(snapshot))?,
        })
    }

    /// # Errors
    ///
    /// [`Malformed`] when a field is missing or does not decode.
    pub fn into_raft(self) -> Result<(Vote, SnapshotOf<RaftTypes, GroupState>), Malformed> {
        let snapshot = snapshot_record::decode(&self.record)?;
        let vote = leader_vote_of(self.vote)?;
        within_vote(snapshot.meta.last_log_id.as_ref(), &vote)?;
        membership_within(&snapshot.meta)?;
        Ok((vote, snapshot.into()))
    }
}

/// Refuse a snapshot whose membership comes from past its last log id:
/// the snapshot would install a membership as committed that its log
/// does not hold.
fn membership_within(meta: &SnapshotMetaOf<RaftTypes>) -> Result<(), Malformed> {
    let membership = *meta.last_membership.log_id();
    let beyond = match (membership, meta.last_log_id) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(m), Some(last)) => m.leader_id.term > last.leader_id.term || m.index > last.index,
    };
    if beyond {
        return Err(Malformed(format!(
            "snapshot membership {membership:?} is past its last log id {:?}",
            meta.last_log_id
        )));
    }
    Ok(())
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
    use openraft::type_config::alias::StoredMembershipOf;
    use ourios_wal::{Control, TenantBatch};
    use proptest::prelude::*;
    use prost::Message;

    use super::*;
    use crate::replicated::raft::snapshot_record::GroupMinerConfig;
    use crate::replicated::raft::types::{GroupEntry, Membership, TenantBatchBytes};

    fn bare(meta: SnapshotMetaOf<RaftTypes>) -> SnapshotOf<RaftTypes, GroupState> {
        SnapshotOf::<RaftTypes, GroupState> {
            meta,
            snapshot: GroupState::default(),
        }
    }

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

    fn leader_vote() -> impl Strategy<Value = Vote> {
        vote().prop_map(|mut vote| {
            vote.committed = true;
            vote
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

    /// A miner configuration `MinerConfig` accepts.
    fn miner_config() -> impl Strategy<Value = ourios_wal::MinerConfigV1> {
        let mode = prop_oneof![
            Just(ourios_wal::UpstreamTemplates::Ignore),
            Just(ourios_wal::UpstreamTemplates::Observe),
            Just(ourios_wal::UpstreamTemplates::Adopt),
        ];
        (0.4f32..=1.0, 0..=8u8, 1..=1024u32, 1..=u16::MAX, mode).prop_map(
            |(similarity_threshold, prefix_depth, param_byte_limit, max_node_children, mode)| {
                ourios_wal::MinerConfigV1 {
                    similarity_threshold,
                    similarity_floor: 0.4,
                    param_byte_limit,
                    prefix_depth,
                    max_node_children,
                    max_templates: 20_000,
                    max_line_tokens: 512,
                    upstream_templates: mode,
                    upstream_template_byte_limit: 8192,
                    upstream_association_limit: 4,
                }
            },
        )
    }

    fn entry() -> impl Strategy<Value = Entry> {
        let normal = proptest::collection::vec(any::<u8>(), 0..64).prop_map(|body| {
            let batch = TenantBatch::encode("acme", &body).expect("batch");
            EntryPayload::Normal(TenantBatchBytes::new(batch).expect("valid").into())
        });
        let flusher = any::<[u8; 16]>().prop_map(|replica| {
            EntryPayload::Normal(GroupEntry::Control(Control::FlusherAssignment(replica)))
        });
        let checkpoint = (any::<[u8; 16]>(), any::<(u64, u64)>(), any::<u8>()).prop_map(
            |(proposer, (term, index), highest_kind)| {
                let cut = ourios_wal::RaftLogId { term, index };
                let control = Control::Checkpoint {
                    proposer,
                    cut,
                    highest_kind,
                };
                EntryPayload::Normal(GroupEntry::Control(control))
            },
        );
        let id_block = (0..i64::MAX.unsigned_abs(), any::<u64>()).prop_map(|(after, span)| {
            let through = after + 1 + span % (i64::MAX.unsigned_abs() - after);
            let control = Control::IdBlock { after, through };
            EntryPayload::Normal(GroupEntry::Control(control))
        });
        let configuration = miner_config().prop_map(|config| {
            EntryPayload::Normal(GroupEntry::Control(Control::MinerConfig(config)))
        });
        let payload = prop_oneof![
            Just(EntryPayload::Blank),
            normal,
            flusher,
            checkpoint,
            id_block,
            configuration,
            membership().prop_map(EntryPayload::Membership),
        ];
        (log_id(), payload).prop_map(|(log_id, payload)| Entry { log_id, payload })
    }

    fn log(term: u64, index: u64) -> LogId {
        LogId::from(WireLogId { term, index })
    }

    /// A vote and entries a leader could send under it: consecutive
    /// after `prev`, in non-decreasing terms no later than the vote's.
    fn segment() -> impl Strategy<Value = (Vote, Option<LogId>, Vec<Entry>)> {
        let prev = proptest::option::of((any::<u64>(), 0..u64::MAX / 2));
        let entries = proptest::collection::vec((entry(), 0..3_u64), 0..6);
        (leader_vote(), prev, entries).prop_map(|(vote, prev, entries)| {
            let top = vote.leader_id.term;
            let prev = prev.map(|(term, index)| log(term.min(top), index));
            let mut term = prev.map_or(0, |p| p.leader_id.term);
            let first = prev.map_or(0, |p| p.index + 1);
            let entries = entries
                .into_iter()
                .zip(first..)
                .map(|((entry, step), index)| {
                    term = term.saturating_add(step).min(top);
                    Entry {
                        log_id: log(term, index),
                        payload: entry.payload,
                    }
                })
                .collect();
            (vote, prev, entries)
        })
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

    fn stream_result() -> impl Strategy<Value = StreamAppendResult<RaftTypes>> {
        prop_oneof![
            proptest::option::of(log_id()).prop_map(Ok),
            log_id().prop_map(|id| Err(StreamAppendError::Conflict(id))),
            vote().prop_map(|v| Err(StreamAppendError::HigherVote(v))),
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
            (vote, prev_log_id, entries) in segment(),
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
        fn a_stream_append_response_round_trips(result in stream_result()) {
            let wire = through_wire(&StreamAppendResponse::from(&result));
            prop_assert_eq!(wire.into_raft().expect("decode"), result);
        }

        #[test]
        fn a_split_append_carries_every_entry_in_follow_on_pieces(
            to in recipient(),
            (vote, prev_log_id, entries) in segment(),
            leader_commit in proptest::option::of(log_id()),
        ) {
            let rpc = AppendEntriesRequest::<RaftTypes> { vote, prev_log_id, entries, leader_commit };
            let pieces = AppendRequest::split(to, &rpc).expect("encode");
            prop_assert_eq!(pieces.len(), 1);
            let back = through_wire(&pieces[0]).into_raft().expect("decode");
            prop_assert_eq!(back.prev_log_id, rpc.prev_log_id);
            prop_assert_eq!(back.entries, rpc.entries);
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
            vote in leader_vote(),
            last_log_id in proptest::option::of(log_id()),
            last_membership in stored_membership(),
            flusher in proptest::option::of(any::<u64>()),
            highest_kind in 0..=Control::HIGHEST_KIND,
            miner_config in proptest::option::of(miner_config()),
        ) {
            let last_log_id = last_log_id
                .map(|id| log(id.leader_id.term.min(vote.leader_id.term), id.index));
            let within = |m: &LogId| last_log_id
                .is_some_and(|l| m.leader_id.term <= l.leader_id.term && m.index <= l.index);
            let membership_id = (*last_membership.log_id()).filter(within);
            let last_membership = StoredMembershipOf::<RaftTypes>::new(
                membership_id,
                last_membership.membership().clone(),
            );
            let meta = SnapshotMetaOf::<RaftTypes> { last_log_id, last_membership };
            let miner_config = miner_config.map(|c| GroupMinerConfig::new(c).expect("valid"));
            let state = GroupState {
                flusher: flusher.map(node),
                highest_kind,
                id_block: None,
                miner_config,
            };
            let snapshot = SnapshotOf::<RaftTypes, GroupState> { meta: meta.clone(), snapshot: state };
            let wire = through_wire(&SnapshotRequest::new(to, &vote, snapshot).expect("encode"));
            prop_assert_eq!(wire.header.clone(), Some(Header::to(to)));
            let (back_vote, back) = wire.into_raft().expect("decode");
            prop_assert_eq!((back_vote, back.meta, back.snapshot), (vote, meta, state));
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
            payload: EntryPayload::Normal(TenantBatchBytes::new(batch).expect("valid").into()),
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

    /// Each piece follows the one before it, so the follower checks each
    /// on its own, and together they carry every entry once.
    #[test]
    fn an_append_over_the_byte_budget_splits_into_follow_on_pieces() {
        let seven_mib = 7 * 1024 * 1024;
        let rpc = AppendEntriesRequest {
            leader_commit: Some(log(1, 4)),
            ..append_of((0..5).map(|i| large_entry(i, seven_mib)).collect())
        };
        let pieces = AppendRequest::split(to(), &rpc).expect("encode");
        assert_eq!(
            pieces.iter().map(|p| p.entries.len()).collect::<Vec<_>>(),
            [2, 2, 1]
        );
        let mut prev = None;
        let mut carried = Vec::new();
        for piece in pieces {
            assert!(piece.encoded_len() <= MAX_MESSAGE_BYTES);
            assert_eq!(piece.header, Some(Header::to(to())));
            let back = through_wire(&piece).into_raft().expect("decode");
            assert_eq!(back.prev_log_id, prev);
            assert_eq!(
                (back.vote, back.leader_commit),
                (rpc.vote, rpc.leader_commit)
            );
            prev = back.entries.last().map(|e| e.log_id);
            carried.extend(back.entries);
        }
        assert_eq!(carried, rpc.entries);
    }

    /// The stream's chunker owns its append and encodes a piece only
    /// when asked for one, cutting where the borrowed split does.
    #[test]
    fn an_owned_append_is_cut_one_piece_at_a_time() {
        let seven_mib = 7 * 1024 * 1024;
        let rpc = append_of((0..5).map(|i| large_entry(i, seven_mib)).collect());
        let split = AppendRequest::split(to(), &rpc).expect("encode");
        let mut pieces = Pieces::new(to(), rpc);
        for expected in split {
            assert_eq!(pieces.next().expect("a piece").expect("encode"), expected);
        }
        assert!(pieces.next().is_none());
    }

    #[test]
    fn an_empty_append_is_one_empty_piece() {
        let rpc = AppendEntriesRequest {
            prev_log_id: Some(log(1, 3)),
            ..append_of(Vec::new())
        };
        let pieces = AppendRequest::split(to(), &rpc).expect("encode");
        assert_eq!(pieces.len(), 1);
        assert!(pieces[0].entries.is_empty());
        assert_eq!(pieces[0].prev_log_id, Some(WireLogId { term: 1, index: 3 }));
    }

    fn blank(term: u64, index: u64) -> Entry {
        Entry {
            log_id: log(term, index),
            payload: EntryPayload::Blank,
        }
    }

    fn decoded(prev: Option<LogId>, entries: Vec<Entry>) -> Result<(), Malformed> {
        let rpc = AppendEntriesRequest {
            vote: Vote::new_committed(3, node(1)),
            prev_log_id: prev,
            entries,
            leader_commit: None,
        };
        let (request, _) = AppendRequest::bounded(to(), &rpc).expect("encode");
        request.into_raft().map(drop)
    }

    #[test]
    fn an_append_no_leader_would_send_is_refused() {
        let refused = [
            (None, vec![blank(1, 1)]),
            (Some(log(1, 4)), vec![blank(1, 6)]),
            (Some(log(2, 4)), vec![blank(1, 5)]),
            (None, vec![blank(1, 0), blank(1, 0)]),
            (None, vec![blank(4, 0)]),
            (Some(log(4, 4)), vec![]),
            (Some(log(1, u64::MAX)), vec![blank(1, 0)]),
        ];
        for (prev, entries) in refused {
            let shown = format!("{prev:?} {entries:?}");
            assert!(decoded(prev, entries).is_err(), "{shown}");
        }
        assert!(decoded(Some(log(1, 4)), vec![blank(1, 5), blank(3, 6)]).is_ok());
    }

    #[test]
    fn a_snapshot_after_the_senders_vote_is_refused() {
        let vote = Vote::new_committed(3, node(1));
        let meta = |term| SnapshotMetaOf::<RaftTypes> {
            last_log_id: Some(log(term, 9)),
            last_membership: StoredMembershipOf::<RaftTypes>::default(),
        };
        let request = |term| SnapshotRequest::new(to(), &vote, bare(meta(term))).expect("encode");
        assert!(request(4).into_raft().is_err());
        assert!(request(3).into_raft().is_ok());
    }

    #[test]
    fn an_append_or_snapshot_under_an_uncommitted_vote_is_refused() {
        let candidate = Vote::new(3, node(1));
        let append = AppendEntriesRequest::<RaftTypes> {
            vote: candidate,
            prev_log_id: None,
            entries: Vec::new(),
            leader_commit: None,
        };
        let (request, _) = AppendRequest::bounded(to(), &append).expect("encode");
        assert!(request.into_raft().is_err());
        let meta = SnapshotMetaOf::<RaftTypes>::default();
        let snapshot = SnapshotRequest::new(to(), &candidate, bare(meta)).expect("encode");
        assert!(snapshot.into_raft().is_err());
    }

    #[test]
    fn a_snapshot_membership_past_its_last_log_id_is_refused() {
        let vote = Vote::new_committed(5, node(1));
        let membership = Membership::new(
            vec![BTreeSet::from([node(1)])],
            BTreeMap::from([(node(1), "a".to_owned())]),
        )
        .expect("membership");
        let decodes = |last: Option<LogId>, at: Option<LogId>| {
            let meta = SnapshotMetaOf::<RaftTypes> {
                last_log_id: last,
                last_membership: StoredMembershipOf::<RaftTypes>::new(at, membership.clone()),
            };
            let request = SnapshotRequest::new(to(), &vote, bare(meta)).expect("encode");
            request.into_raft().is_ok()
        };
        let cases = [
            (None, Some(log(1, 1)), false),
            (Some(log(2, 5)), Some(log(3, 4)), false),
            (Some(log(2, 5)), Some(log(2, 6)), false),
            (Some(log(2, 5)), Some(log(2, 5)), true),
            (Some(log(2, 5)), Some(log(1, 3)), true),
        ];
        for (last, at, ok) in cases {
            assert_eq!(decodes(last, at), ok, "{last:?} {at:?}");
        }
    }

    #[test]
    fn an_entry_over_the_frame_limit_is_refused_before_decoding() {
        let oversized = vec![0; ourios_wal::MAX_FRAME_BYTES + 1];
        let error = decode_entry(&oversized).expect_err("refused");
        assert!(error.to_string().contains("frame limit"), "{error}");
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
    fn a_stream_answer_without_its_outcome_or_with_a_bad_vote_is_refused() {
        assert!(StreamAppendResponse::default().into_raft().is_err());
        let short = StreamAppendResponse {
            outcome: Some(StreamOutcome::HigherVote(WireVote {
                term: 1,
                voted_for: vec![1, 2, 3],
                committed: true,
            })),
        };
        assert!(short.into_raft().is_err());
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

    #[test]
    fn an_append_result_takes_its_log_ids_from_the_append_sent() {
        let id = |index| LogId::from(WireLogId { term: 1, index });
        let (prev, last) = (Some(id(3)), Some(id(5)));
        let vote = Vote::new(2, node(1));
        let cases = [
            (AppendEntriesResponse::Success, Ok(last)),
            (
                AppendEntriesResponse::PartialSuccess(Some(id(4))),
                Ok(Some(id(4))),
            ),
            (
                AppendEntriesResponse::Conflict,
                Err(AppendError::Conflict {
                    log_id: id(3),
                    hint: None,
                }),
            ),
            (
                AppendEntriesResponse::HigherVote(vote),
                Err(AppendError::HigherVote(vote)),
            ),
        ];
        for (response, expected) in cases {
            let result = append_result(&response, prev, last).expect("well formed");
            assert_eq!(result, expected, "{response}");
        }
    }

    #[test]
    fn a_conflict_on_an_append_from_the_empty_log_is_refused() {
        let last = Some(LogId::from(WireLogId { term: 1, index: 1 }));
        assert!(append_result(&AppendEntriesResponse::Conflict, None, last).is_err());
    }
}
