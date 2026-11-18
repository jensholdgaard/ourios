//! `ReadIndex` (RFC 0065 §6.7, "Read your acks"): a tail replica asks its
//! group's leader for a **quorum-confirmed** read point, the log id the
//! leader returns only after confirming its leadership with a quorum
//! (`ReadIndex`, not a lease), so a deposed leader or one whose commit point
//! lags cannot answer. The replica then serves its tail once it has
//! applied and mined through that id.
//!
//! A refusal names itself in the `ourios-read-refused` metadata of its
//! status. Anything else, the call's deadline and a lost answer leave the
//! replica without a confirmed point, so it answers "unavailable" rather
//! than serve a possibly stale tail. A read may always be asked again: it
//! appends nothing.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request, Status};
use tonic_prost::ProstCodec;

use super::super::types::LogId;
use super::trace::{Call, Peer, Traced, Tracing, inject_current};
use super::wire::{MAX_MESSAGE_BYTES, WireLogId};
use super::{READ_INDEX, VERSION};

const REFUSED: &str = "ourios-read-refused";
const LEADER: &str = "ourios-leader";

/// A tail replica's request for its group's read point.
#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub struct ReadIndexRequest {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(uint32, tag = "2")]
    pub group: u32,
}

impl Traced for ReadIndexRequest {}

/// The confirmed read point; absent while the group's log is empty.
#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub struct ReadIndexResponse {
    #[prost(message, optional, tag = "1")]
    pub read: Option<WireLogId>,
}

/// Why a node gave no read point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// It does not host the group.
    NotHosted,
    /// It does not lead the group, naming the leader it knows of.
    NotLeader { leader: Option<String> },
    /// It could not confirm its leadership with a quorum.
    Unconfirmed(String),
}

/// What a leader's read comes to: the read point, or why there is none.
pub type Reading = Pin<Box<dyn Future<Output = Result<LogId, Refusal>> + Send>>;

/// The node's side of `ReadIndex`.
pub trait ReadIndexTarget: Send + Sync + 'static {
    /// `group`'s read point, once this node has confirmed with a quorum
    /// that it still leads the group.
    fn read_index(&self, group: u32) -> Reading;
}

/// Serve one `ReadIndex` call with `target`, the node's groups.
pub(super) async fn serve(
    target: Option<Arc<dyn ReadIndexTarget>>,
    request: ReadIndexRequest,
) -> Result<ReadIndexResponse, Status> {
    if request.version != VERSION {
        let message = format!("peer protocol version {} is not {VERSION}", request.version);
        return Err(refused(Status::failed_precondition(message), "not_hosted"));
    }
    let Some(target) = target else {
        return Err(refusal(Refusal::NotHosted));
    };
    match target.read_index(request.group).await {
        Ok(read) => Ok(ReadIndexResponse {
            read: Some(WireLogId::from(&read)),
        }),
        Err(refused) => Err(refusal(refused)),
    }
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
        Refusal::Unconfirmed(why) => refused(Status::unavailable(why), "unconfirmed"),
    }
}

/// What a read asked of one node came to, as the tail replica sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The quorum-confirmed read point.
    Read(LogId),
    /// The node does not lead the group, naming the leader it knows of.
    NotLeader { leader: Option<String> },
    /// The node does not host the group.
    NotHosted,
    /// The call was never sent.
    Unreached(String),
    /// No confirmed read point: the leader could not confirm, the call
    /// failed or timed out.
    Unavailable(String),
}

/// Ask `addr` over `channel` for `request`'s read point within `patience`,
/// under its gRPC client span.
pub(super) async fn traced_call(
    channel: Channel,
    (addr, tracing): (&str, Tracing),
    request: ReadIndexRequest,
    patience: Duration,
) -> Outcome {
    let trace = Call::client(tracing, READ_INDEX, Peer::parse(addr), true);
    let (outcome, code) =
        tracing::Instrument::instrument(call(channel, request, patience), trace.span()).await;
    trace.finish(code);
    outcome
}

/// Ask over `channel`, the connection and the answer together bounded by
/// `patience`.
async fn call(channel: Channel, request: ReadIndexRequest, patience: Duration) -> (Outcome, Code) {
    let asking = async {
        let mut grpc = tonic::client::Grpc::new(channel)
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES);
        if let Err(e) = grpc.ready().await {
            return (Outcome::Unreached(e.to_string()), Code::Unavailable);
        }
        let mut request = Request::new(request);
        inject_current(&mut request);
        request.set_timeout(patience);
        let codec = ProstCodec::<ReadIndexRequest, ReadIndexResponse>::default();
        match grpc
            .unary(request, PathAndQuery::from_static(READ_INDEX), codec)
            .await
        {
            Ok(response) => {
                let outcome = response.into_inner().read.map_or_else(
                    || Outcome::Unavailable("the group's log is empty".to_owned()),
                    |read| Outcome::Read(LogId::from(read)),
                );
                (outcome, Code::Ok)
            }
            Err(status) if super::connect::never_sent(&status) => (
                Outcome::Unreached(status.message().to_owned()),
                status.code(),
            ),
            Err(status) => (classify(&status), status.code()),
        }
    };
    tokio::time::timeout(patience, asking)
        .await
        .unwrap_or_else(|_| {
            let late = Outcome::Unavailable("the read point was not confirmed in time".to_owned());
            (late, Code::DeadlineExceeded)
        })
}

/// An answer's status: a refusal only if the node marked it as one, or an
/// older node that does not serve `ReadIndex` at all.
fn classify(status: &Status) -> Outcome {
    let metadata = status.metadata();
    let text = |key| metadata.get(key).and_then(|v| v.to_str().ok());
    match (text(REFUSED), status.code()) {
        (Some("not_leader"), _) => Outcome::NotLeader {
            leader: text(LEADER).map(str::to_owned),
        },
        (Some("not_hosted"), _) | (None, Code::Unimplemented) => Outcome::NotHosted,
        _ => Outcome::Unavailable(status.message().to_owned()),
    }
}

/// The log id `RaftLogId` names, for tests that build read points by hand.
#[cfg(test)]
fn log_id(term: u64, index: u64) -> LogId {
    super::super::codec::raft_id(ourios_wal::RaftLogId { term, index })
}

#[cfg(test)]
mod tests;
