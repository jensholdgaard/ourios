//! `Forward` (RFC 0065 §6.8): one export, carried from the node a client
//! sent it to, to the node the sender believes leads its group.
//!
//! An answer either commits the export, refuses it in a way that proves
//! nothing was appended, or leaves its outcome uncertain. A refusal says so
//! in the `ourios-forward-refused` metadata of its status; any status
//! without it, the call's deadline and a lost answer are uncertain, and
//! the sender must not resubmit the export (§6.6, §9 Q4).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request, Status};
use tonic_prost::ProstCodec;

use super::super::types::TenantBatchBytes;
use super::trace::{Call, Peer, Traced, Tracing};
use super::wire::MAX_MESSAGE_BYTES;
use super::{FORWARD, VERSION};

const REFUSED: &str = "ourios-forward-refused";
const LEADER: &str = "ourios-leader";
const ERROR_TYPE: &str = "ourios-error-type";

/// One forwarded export: the group, its `TenantOtlpBatch` payload and the
/// credential its client presented, if any. It names no target replica.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ForwardRequest {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(uint32, tag = "2")]
    pub group: u32,
    #[prost(bytes = "vec", tag = "3")]
    pub payload: Vec<u8>,
    /// The client's `Authorization` header value, as it sent it.
    #[prost(string, optional, tag = "4")]
    pub credential: Option<String>,
}

impl Traced for ForwardRequest {}

/// A committed export's position in its group's log.
#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub struct ForwardResponse {
    #[prost(uint64, tag = "1")]
    pub seq: u64,
}

/// An export as the leader receives it.
#[derive(Debug)]
pub struct Forwarded {
    pub group: u32,
    pub batch: TenantBatchBytes,
    pub credential: Option<String>,
}

/// Why a leader refused an export before appending it: the rejection its
/// client would have got had it sent the export there, passed back
/// unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// The credential is missing or invalid.
    Unauthenticated,
    /// The credential could not be checked, for example because its
    /// issuer was unreachable; retryable.
    Unavailable,
    /// The credential's tenant binding does not allow the tenant.
    PermissionDenied,
    /// The export does not fit one WAL frame.
    TooLarge,
}

/// A leader's rejection as its client would have got it: the code's
/// class, its `error.type`, and the status message and details, carried
/// back unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub rejection: Rejection,
    pub error_type: String,
    pub message: String,
    pub details: Vec<u8>,
}

/// How a node answers an export it did not commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// It does not lead the group; nothing was appended.
    NotLeader { leader: Option<String> },
    /// It does not host the group; nothing was appended.
    NotHosted,
    /// Its leader refused the export before appending it.
    Rejected(Rejected),
    /// The export may have been appended: the commit timed out, or a leader
    /// change discarded the entry after appending it.
    Failed(String),
}

type Committing = Pin<Box<dyn Future<Output = Result<u64, Refusal>> + Send>>;

/// The node's side of `Forward`: commit a forwarded export as if its
/// client had sent it here, without forwarding it again.
pub trait ForwardTarget: Send + Sync + 'static {
    fn commit(&self, export: Forwarded) -> Committing;
}

/// Serve one `Forward` call with `target`, the node's ingest.
pub(super) async fn serve(
    target: Option<Arc<dyn ForwardTarget>>,
    request: ForwardRequest,
) -> Result<ForwardResponse, Status> {
    let ForwardRequest {
        version,
        group,
        payload,
        credential,
    } = request;
    if version != VERSION {
        let message = format!("peer protocol version {version} is not {VERSION}");
        return Err(refused(Status::failed_precondition(message), "not_hosted"));
    }
    let Some(target) = target else {
        return Err(refusal(Refusal::NotHosted));
    };
    let batch = TenantBatchBytes::new(payload)
        .map_err(|e| Status::invalid_argument(format!("a forwarded export: {e}")))?;
    let export = Forwarded {
        group,
        batch,
        credential,
    };
    match target.commit(export).await {
        Ok(seq) => Ok(ForwardResponse { seq }),
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
        Refusal::NotLeader { leader } => {
            let mut status = refused(Status::unavailable("not the group's leader"), "not_leader");
            if let Some(hint) = leader.and_then(|l| MetadataValue::try_from(l).ok()) {
                status.metadata_mut().insert(LEADER, hint);
            }
            status
        }
        Refusal::NotHosted => refused(
            Status::not_found("the group is not hosted here"),
            "not_hosted",
        ),
        Refusal::Rejected(Rejected {
            rejection,
            error_type,
            message,
            details,
        }) => {
            let status = Status::with_details(rejection.code(), message, details.into());
            let mut status = refused(status, "rejected");
            if let Ok(error_type) = MetadataValue::try_from(error_type) {
                status.metadata_mut().insert(ERROR_TYPE, error_type);
            }
            status
        }
        Refusal::Failed(reason) => Status::unavailable(reason),
    }
}

impl Rejection {
    fn code(self) -> Code {
        match self {
            Self::Unauthenticated => Code::Unauthenticated,
            Self::Unavailable => Code::Unavailable,
            Self::PermissionDenied => Code::PermissionDenied,
            Self::TooLarge => Code::ResourceExhausted,
        }
    }

    fn from_code(code: Code) -> Option<Self> {
        match code {
            Code::Unauthenticated => Some(Self::Unauthenticated),
            Code::Unavailable => Some(Self::Unavailable),
            Code::PermissionDenied => Some(Self::PermissionDenied),
            Code::ResourceExhausted => Some(Self::TooLarge),
            _ => None,
        }
    }
}

/// What a forward to one node came to, as the sender sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The node committed the export at `seq`.
    Committed(u64),
    /// The node does not lead the group, naming the leader it knows of;
    /// nothing was appended.
    NotLeader { leader: Option<String> },
    /// The node does not host the group; nothing was appended.
    NotHosted,
    /// The call was never sent: the connection was refused or not made
    /// within the connect timeout.
    Unreached(String),
    /// The leader refused the export before appending it.
    Rejected(Rejected),
    /// The export may have committed; it must not be sent again.
    Uncertain(String),
}

/// The deadlines of one forwarded call.
#[derive(Clone, Copy, Debug)]
pub(super) struct Deadlines {
    pub(super) connect: Duration,
    pub(super) commit: Duration,
}

/// Forward `request` to `addr` over `channel`, under its gRPC client span.
pub(super) async fn traced_call(
    channel: Channel,
    (addr, tracing): (&str, Tracing),
    request: ForwardRequest,
    deadlines: Deadlines,
) -> Outcome {
    let trace = Call::client(tracing, FORWARD, Peer::parse(addr), true);
    let (outcome, code) =
        tracing::Instrument::instrument(call(channel, request, deadlines), trace.span()).await;
    trace.finish(code);
    outcome
}

/// Forward `request` over `channel`, which connects within the connect
/// deadline or not at all. Once sent, the call is bounded by the commit
/// timeout plus the connect timeout (§6.8). Returns the outcome and the
/// call's own status code, which its span records.
async fn call(channel: Channel, request: ForwardRequest, deadlines: Deadlines) -> (Outcome, Code) {
    let Deadlines {
        connect: connect_timeout,
        commit: commit_timeout,
    } = deadlines;
    let mut grpc = tonic::client::Grpc::new(channel)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
    // `ready` does not wait for the connection: a failed connect surfaces
    // from the call itself, marked as never sent.
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
    // The peer stops work a little after the sender gives up, so the
    // sender's own deadline is the one that ends the call.
    request.set_timeout(deadline + connect_timeout);
    let codec = ProstCodec::<ForwardRequest, ForwardResponse>::default();
    let answer = grpc.unary(request, PathAndQuery::from_static(FORWARD), codec);
    match tokio::time::timeout(deadline, answer).await {
        Ok(Ok(response)) => (Outcome::Committed(response.into_inner().seq), Code::Ok),
        Ok(Err(status)) if super::connect::never_sent(&status) => (
            Outcome::Unreached(status.message().to_owned()),
            status.code(),
        ),
        Ok(Err(status)) => (classify(&status), status.code()),
        Err(_) => {
            let timed_out = Outcome::Uncertain("the forwarded call timed out".to_owned());
            (timed_out, Code::DeadlineExceeded)
        }
    }
}

/// An answer's status: a refusal only if the node marked it as one, or an
/// older node that does not serve `Forward` at all.
fn classify(status: &Status) -> Outcome {
    let metadata = status.metadata();
    let text = |key| metadata.get(key).and_then(|v| v.to_str().ok());
    match (text(REFUSED), status.code()) {
        (Some("not_leader"), _) => Outcome::NotLeader {
            leader: text(LEADER).map(str::to_owned),
        },
        (Some("not_hosted"), _) | (None, Code::Unimplemented) => Outcome::NotHosted,
        (Some("rejected"), code) => match Rejection::from_code(code) {
            Some(rejection) => Outcome::Rejected(Rejected {
                rejection,
                error_type: text(ERROR_TYPE).unwrap_or_default().to_owned(),
                message: status.message().to_owned(),
                details: status.details().to_vec(),
            }),
            None => Outcome::Uncertain(status.message().to_owned()),
        },
        _ => Outcome::Uncertain(status.message().to_owned()),
    }
}

#[cfg(test)]
mod tests;
