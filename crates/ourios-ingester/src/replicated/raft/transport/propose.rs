//! `Propose` (RFC 0065 §6.8): what only a group's flusher sends its
//! leader, a report of its id allocator and, optionally, a control entry
//! for the leader to append, today a checkpoint (`0x02`).
//!
//! A refusal names itself in the `ourios-propose-refused` metadata of its
//! status and proves nothing was appended. Any other status, the call's
//! deadline and a lost answer are uncertain; unlike a forwarded export a
//! proposal may be sent again, since a second copy is refused as `stale`
//! or ignored by every replica.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ourios_wal::Control;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request, Status};
use tonic_prost::ProstCodec;
use uuid::Uuid;

use super::super::codec::{raft_id, wal_id};
use super::super::types::{LogId, NodeId};
use super::forward::Deadlines;
use super::trace::{Call, Peer, Traced, Tracing, inject_current};
use super::wire::{MAX_MESSAGE_BYTES, WireLogId};
use super::{PROPOSE, VERSION};

const REFUSED: &str = "ourios-propose-refused";
const LEADER: &str = "ourios-leader";

/// A flusher's report and, optionally, the control entry it proposes.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ProposeRequest {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(uint32, tag = "2")]
    pub group: u32,
    /// The sender's replica UUID, 16 bytes big-endian.
    #[prost(bytes = "vec", tag = "3")]
    pub proposer: Vec<u8>,
    /// The last entry its group miner followed; absent before one.
    #[prost(message, optional, tag = "4")]
    pub mined: Option<WireLogId>,
    /// The id blocks it holds and has not started.
    #[prost(uint32, tag = "5")]
    pub unstarted: u32,
    /// A control entry's kind byte and body.
    #[prost(bytes = "vec", optional, tag = "6")]
    pub entry: Option<Vec<u8>>,
}

impl Traced for ProposeRequest {}

/// The proposed entry's committed log id; absent without one.
#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub struct ProposeResponse {
    #[prost(message, optional, tag = "1")]
    pub appended: Option<WireLogId>,
}

/// A proposed checkpoint: its cut and the log's highest kind there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProposedCheckpoint {
    pub cut: LogId,
    pub highest_kind: u8,
}

/// A well-formed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub proposer: NodeId,
    pub mined: Option<LogId>,
    pub unstarted: u32,
    pub checkpoint: Option<ProposedCheckpoint>,
}

impl Report {
    /// The request as sent: `checkpoint`, if any, carries `proposer`.
    #[must_use]
    pub fn request(&self, group: u32) -> ProposeRequest {
        let proposer = *self.proposer.uuid().as_bytes();
        let entry = self.checkpoint.map(|checkpoint| {
            Control::Checkpoint {
                proposer,
                cut: wal_id(&checkpoint.cut),
                highest_kind: checkpoint.highest_kind,
            }
            .to_bytes()
        });
        ProposeRequest {
            version: VERSION,
            group,
            proposer: proposer.to_vec(),
            mined: self.mined.as_ref().map(WireLogId::from),
            unstarted: self.unstarted,
            entry,
        }
    }
}

/// A proposal as the leader receives it: its group, and its report or
/// why the request is malformed, which the leader refuses only after
/// checking that it hosts and leads the group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposed {
    pub group: u32,
    pub report: Result<Report, String>,
}

/// Why a leader refused a proposal; nothing was appended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// It does not host the group.
    NotHosted,
    /// It does not lead the group, naming the leader it knows of.
    NotLeader { leader: Option<String> },
    /// The request is malformed.
    Malformed(String),
    /// The proposer is not the flusher the leader's log names.
    NotFlusher,
    /// The report or the checkpoint is behind the leader's log.
    Stale(String),
    /// Not a refusal: the entry may have been appended.
    Failed(String),
}

type Proposing = Pin<Box<dyn Future<Output = Result<Option<LogId>, Refusal>> + Send>>;

/// The node's side of `Propose`.
pub trait ProposeTarget: Send + Sync + 'static {
    /// Check `proposed`, count its report, and append its entry, if any;
    /// the entry's committed log id.
    fn propose(&self, proposed: Proposed) -> Proposing;
}

/// Serve one `Propose` call with `target`, the node's groups.
pub(super) async fn serve(
    target: Option<Arc<dyn ProposeTarget>>,
    request: ProposeRequest,
) -> Result<ProposeResponse, Status> {
    if request.version != VERSION {
        let message = format!("peer protocol version {} is not {VERSION}", request.version);
        return Err(refused(Status::failed_precondition(message), "not_hosted"));
    }
    let Some(target) = target else {
        return Err(refusal(Refusal::NotHosted));
    };
    let proposed = Proposed {
        group: request.group,
        report: parse(&request),
    };
    match target.propose(proposed).await {
        Ok(appended) => Ok(ProposeResponse {
            appended: appended.as_ref().map(WireLogId::from),
        }),
        Err(refused) => Err(refusal(refused)),
    }
}

/// The report `request` carries, or why it is malformed: a proposer that
/// is not 16 bytes, or an entry that is not a checkpoint carrying it.
fn parse(request: &ProposeRequest) -> Result<Report, String> {
    let proposer = <[u8; 16]>::try_from(request.proposer.as_slice())
        .map_err(|_| format!("a proposer of {} bytes, not 16", request.proposer.len()))?;
    let checkpoint = match request.entry.as_deref().map(Control::from_bytes) {
        None => None,
        Some(Ok(Control::Checkpoint {
            proposer: named,
            cut,
            highest_kind,
        })) if named == proposer => Some(ProposedCheckpoint {
            cut: raft_id(cut),
            highest_kind,
        }),
        Some(Ok(Control::Checkpoint { .. })) => {
            return Err("a checkpoint naming another proposer".to_owned());
        }
        Some(Ok(other)) => return Err(format!("a control entry of kind {:#04x}", other.kind())),
        Some(Err(e)) => return Err(format!("an entry that does not decode: {e}")),
    };
    Ok(Report {
        proposer: NodeId::from(Uuid::from_bytes(proposer)),
        mined: request.mined.map(LogId::from),
        unstarted: request.unstarted,
        checkpoint,
    })
}

fn refused(mut status: Status, why: &'static str) -> Status {
    status
        .metadata_mut()
        .insert(REFUSED, MetadataValue::from_static(why));
    status
}

fn refusal(refusal: Refusal) -> Status {
    match refusal {
        Refusal::NotHosted => refused(
            Status::not_found("the group is not hosted here"),
            "not_hosted",
        ),
        Refusal::NotLeader { leader } => {
            let mut status = refused(Status::unavailable("not the group's leader"), "not_leader");
            if let Some(hint) = leader.and_then(|l| MetadataValue::try_from(l).ok()) {
                status.metadata_mut().insert(LEADER, hint);
            }
            status
        }
        Refusal::Malformed(why) => refused(Status::invalid_argument(why), "malformed"),
        Refusal::NotFlusher => refused(
            Status::failed_precondition("the proposer is not the group's flusher"),
            "not_flusher",
        ),
        Refusal::Stale(why) => refused(Status::failed_precondition(why), "stale"),
        Refusal::Failed(why) => Status::unavailable(why),
    }
}

/// What a proposal to one node came to, as the flusher sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Accepted: its report counted and its entry, if any, committed at
    /// the log id given.
    Accepted { appended: Option<LogId> },
    /// The node does not lead the group, naming the leader it knows of.
    NotLeader { leader: Option<String> },
    /// The node does not host the group.
    NotHosted,
    /// The call was never sent.
    Unreached(String),
    /// The leader found the request malformed, a defect.
    Malformed(String),
    /// The leader's log names another flusher.
    NotFlusher,
    /// The report or checkpoint is behind the leader's log.
    Stale(String),
    /// The entry may have committed; the proposal may be sent again.
    Uncertain(String),
}

/// Send `request` to `addr` over `channel`, under its gRPC client span.
pub(super) async fn traced_call(
    channel: Channel,
    (addr, tracing): (&str, Tracing),
    request: ProposeRequest,
    deadlines: Deadlines,
) -> Outcome {
    let trace = Call::client(tracing, PROPOSE, Peer::parse(addr), true);
    let (outcome, code) =
        tracing::Instrument::instrument(call(channel, request, deadlines), trace.span()).await;
    trace.finish(code);
    outcome
}

/// Send `request` over `channel`, bounded as a forwarded call is: the
/// connect timeout, then the commit timeout plus the connect timeout.
async fn call(channel: Channel, request: ProposeRequest, deadlines: Deadlines) -> (Outcome, Code) {
    let Deadlines {
        connect: connect_timeout,
        commit: commit_timeout,
    } = deadlines;
    let mut grpc = tonic::client::Grpc::new(channel)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
    match tokio::time::timeout(connect_timeout, grpc.ready()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return (Outcome::Unreached(e.to_string()), Code::Unavailable),
        Err(_) => {
            let late = Outcome::Unreached("the channel was not ready in time".to_owned());
            return (late, Code::DeadlineExceeded);
        }
    }
    let deadline = commit_timeout + connect_timeout;
    let mut request = Request::new(request);
    inject_current(&mut request);
    request.set_timeout(deadline + connect_timeout);
    let codec = ProstCodec::<ProposeRequest, ProposeResponse>::default();
    let answer = grpc.unary(request, PathAndQuery::from_static(PROPOSE), codec);
    match tokio::time::timeout(deadline, answer).await {
        Ok(Ok(response)) => {
            let appended = response.into_inner().appended.map(LogId::from);
            (Outcome::Accepted { appended }, Code::Ok)
        }
        Ok(Err(status)) if super::connect::never_sent(&status) => (
            Outcome::Unreached(status.message().to_owned()),
            status.code(),
        ),
        Ok(Err(status)) => (classify(&status), status.code()),
        Err(_) => {
            let timed_out = Outcome::Uncertain("the proposal timed out".to_owned());
            (timed_out, Code::DeadlineExceeded)
        }
    }
}

/// An answer's status: a refusal only if the node marked it as one, or an
/// older node that does not serve `Propose` at all.
fn classify(status: &Status) -> Outcome {
    let metadata = status.metadata();
    let text = |key| metadata.get(key).and_then(|v| v.to_str().ok());
    let message = || status.message().to_owned();
    match (text(REFUSED), status.code()) {
        (Some("not_leader"), _) => Outcome::NotLeader {
            leader: text(LEADER).map(str::to_owned),
        },
        (Some("not_hosted"), _) | (None, Code::Unimplemented) => Outcome::NotHosted,
        (Some("malformed"), _) => Outcome::Malformed(message()),
        (Some("not_flusher"), _) => Outcome::NotFlusher,
        (Some("stale"), _) => Outcome::Stale(message()),
        _ => Outcome::Uncertain(message()),
    }
}

/// The log id `RaftLogId` names, for tests that build cuts by hand.
#[cfg(test)]
fn log_id(term: u64, index: u64) -> LogId {
    raft_id(ourios_wal::RaftLogId { term, index })
}

#[cfg(test)]
mod tests;
