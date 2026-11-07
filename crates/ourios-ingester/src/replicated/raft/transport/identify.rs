//! `Identify` (RFC 0065 §6.8): a leader asks a configured peer which
//! replica runs under its `node_id`. The call names no group and changes
//! nothing; the leader acts on the answer only through a committed
//! membership change (§6.1).

use std::time::Duration;

use tonic::codegen::http::uri::PathAndQuery;
use tonic::transport::Channel;
use tonic::{Request, Status};
use tonic_prost::ProstCodec;
use uuid::Uuid;

use super::super::types::NodeId;
use super::trace::{Call, Peer, Traced, Tracing};
use super::wire::MAX_MESSAGE_BYTES;
use super::{IDENTIFY, VERSION};

/// The highest control-entry kind this release reads (§6.5); `0` while it
/// reads none.
pub const HIGHEST_CONTROL_KIND: u8 = 0;

#[derive(Clone, PartialEq, prost::Message)]
pub struct IdentifyRequest {
    #[prost(uint32, tag = "1")]
    pub version: u32,
}

impl Traced for IdentifyRequest {}

#[derive(Clone, PartialEq, prost::Message)]
pub struct IdentifyResponse {
    #[prost(string, tag = "1")]
    pub node_id: String,
    /// The replica's UUID, 16 big-endian bytes.
    #[prost(bytes = "vec", tag = "2")]
    pub replica: Vec<u8>,
    #[prost(uint32, tag = "3")]
    pub highest_control_kind: u32,
}

/// Who a node is: its configured `node_id`, its replica UUID, and the
/// highest control kind it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub node_id: String,
    pub replica: NodeId,
    pub highest_control_kind: u8,
}

impl Identity {
    /// This release's identity for the replica `replica` under `node_id`.
    #[must_use]
    pub fn new(node_id: String, replica: NodeId) -> Self {
        Self {
            node_id,
            replica,
            highest_control_kind: HIGHEST_CONTROL_KIND,
        }
    }
}

/// Why a peer was not identified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentifyError {
    /// No answer: the peer has no address, or the call failed or timed
    /// out. It is identified on a later pass.
    Unanswered(String),
    /// The peer answered for another `node_id`, or with a malformed
    /// identity; the answer is ignored.
    Mismatched(String),
}

/// Serve one `Identify` call as `identity`.
pub(super) fn serve(
    identity: Option<Identity>,
    request: &IdentifyRequest,
) -> Result<IdentifyResponse, Status> {
    if request.version != VERSION {
        let message = format!("peer protocol version {} is not {VERSION}", request.version);
        return Err(Status::failed_precondition(message));
    }
    let Some(identity) = identity else {
        return Err(Status::unavailable("this node has no identity yet"));
    };
    Ok(IdentifyResponse {
        node_id: identity.node_id,
        replica: identity.replica.uuid().as_bytes().to_vec(),
        highest_control_kind: u32::from(identity.highest_control_kind),
    })
}

/// Ask the peer at `addr`, configured as `node_id`, who it is, within
/// `timeout`, under the call's gRPC client span.
pub(super) async fn call(
    channel: Channel,
    (node_id, addr, tracing): (&str, &str, Tracing),
    timeout: Duration,
) -> Result<Identity, IdentifyError> {
    let trace = Call::client(tracing, IDENTIFY, Peer::parse(addr), true);
    let answer = tracing::Instrument::instrument(ask(channel, timeout), trace.span()).await;
    trace.finish(
        answer
            .as_ref()
            .map_or_else(Status::code, |_| tonic::Code::Ok),
    );
    let answer = answer.map_err(|status| IdentifyError::Unanswered(status.to_string()))?;
    identity_of(node_id, answer)
}

async fn ask(channel: Channel, timeout: Duration) -> Result<IdentifyResponse, Status> {
    let mut grpc = tonic::client::Grpc::new(channel)
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES);
    let asking = async {
        grpc.ready()
            .await
            .map_err(|e| Status::unavailable(e.to_string()))?;
        let mut request = Request::new(IdentifyRequest { version: VERSION });
        request.set_timeout(timeout);
        let codec = ProstCodec::<IdentifyRequest, IdentifyResponse>::default();
        grpc.unary(request, PathAndQuery::from_static(IDENTIFY), codec)
            .await
    };
    match tokio::time::timeout(timeout, asking).await {
        Ok(answer) => answer.map(tonic::Response::into_inner),
        Err(_) => Err(Status::deadline_exceeded("Identify timed out")),
    }
}

/// The identity `answer` gives, if it is for `called`.
fn identity_of(called: &str, answer: IdentifyResponse) -> Result<Identity, IdentifyError> {
    if answer.node_id != called {
        return Err(IdentifyError::Mismatched(format!(
            "{called} answered as {}",
            answer.node_id
        )));
    }
    let replica = Uuid::from_slice(&answer.replica)
        .map_err(|e| IdentifyError::Mismatched(format!("{called}: a replica UUID {e}")))?;
    let highest_control_kind = u8::try_from(answer.highest_control_kind).map_err(|_| {
        IdentifyError::Mismatched(format!(
            "{called}: control kind {} is out of range",
            answer.highest_control_kind
        ))
    })?;
    Ok(Identity {
        node_id: answer.node_id,
        replica: NodeId::from(replica),
        highest_control_kind,
    })
}

#[cfg(test)]
mod tests;
