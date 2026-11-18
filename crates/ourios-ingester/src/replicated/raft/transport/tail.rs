//! `Tail` (RFC 0065 §6.7): a querier asks a tail replica for a tenant's
//! **unpublished** rows at or above the group's checkpoint, as of a
//! quorum-confirmed read point, with the definitions of only the templates
//! those rows reference, and the keys of the tenant's files published
//! above the checkpoint as of the same point.
//!
//! Only a caller whose client certificate names an identity in the
//! replica's `tail_clients` (a URI or DNS subject alternative name) gets
//! rows; any other, another replica included, is refused as denied before
//! the replica reads anything (RFC0065.13). The call carries the query's
//! tenant, already authorized by the querier (RFC 0047), and the replica
//! returns only that tenant's rows.
//!
//! A refusal names itself in the `ourios-tail-refused` metadata of its
//! status. A call may always be asked again: it changes nothing.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request, Status};
use tonic_prost::ProstCodec;

use super::trace::{Call, Peer, Traced, Tracing, inject_current};
use super::wire::MAX_MESSAGE_BYTES;
use super::{TAIL, VERSION};

const REFUSED: &str = "ourios-tail-refused";
const OLDEST: &str = "ourios-tail-oldest";

/// A querier's tail read of one group for one tenant.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct TailRequest {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(uint32, tag = "2")]
    pub group: u32,
    #[prost(string, tag = "3")]
    pub tenant: String,
    /// The checkpoint cut the querier read: rows at or above it.
    #[prost(uint64, tag = "4")]
    pub from: u64,
    /// The query's time range, inclusive start and exclusive end, in Unix
    /// nanoseconds; absent bounds select every row.
    #[prost(uint64, optional, tag = "5")]
    pub start_unix_nano: Option<u64>,
    #[prost(uint64, optional, tag = "6")]
    pub end_unix_nano: Option<u64>,
}

impl Traced for TailRequest {}

/// One template a returned row references, as the audit stream records it.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct TemplateDefinition {
    #[prost(uint64, tag = "1")]
    pub template_id: u64,
    #[prost(uint32, tag = "2")]
    pub template_version: u32,
    /// The template's tokens, as `ourios_miner::tree::format_template`
    /// writes them.
    #[prost(string, tag = "3")]
    pub template: String,
}

/// The tail as of the read point.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct TailResponse {
    /// The read point the rows are served at.
    #[prost(uint64, tag = "1")]
    pub read: u64,
    /// The rows, an Arrow IPC stream in the record files' schema.
    #[prost(bytes = "vec", tag = "2")]
    pub rows: Vec<u8>,
    #[prost(message, repeated, tag = "3")]
    pub templates: Vec<TemplateDefinition>,
    /// The tenant's files published above `from` as of the read point.
    #[prost(string, repeated, tag = "4")]
    pub published: Vec<String>,
}

/// What a tail replica is asked, the request without its framing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailQuery {
    pub group: u32,
    pub tenant: String,
    pub from: u64,
    /// `[start, end)` in Unix nanoseconds, when the query has one.
    pub range: Option<(u64, u64)>,
}

impl TailQuery {
    fn of(request: &TailRequest) -> Self {
        let range = match (request.start_unix_nano, request.end_unix_nano) {
            (None, None) => None,
            (start, end) => Some((start.unwrap_or(0), end.unwrap_or(u64::MAX))),
        };
        Self {
            group: request.group,
            tenant: request.tenant.clone(),
            from: request.from,
            range,
        }
    }

    fn request(&self) -> TailRequest {
        TailRequest {
            version: VERSION,
            group: self.group,
            tenant: self.tenant.clone(),
            from: self.from,
            start_unix_nano: self.range.map(|(start, _)| start),
            end_unix_nano: self.range.map(|(_, end)| end),
        }
    }
}

/// What a tail replica answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailAnswer {
    pub read: u64,
    pub rows: Vec<u8>,
    pub templates: Vec<TemplateDefinition>,
    pub published: Vec<String>,
}

impl From<TailAnswer> for TailResponse {
    fn from(answer: TailAnswer) -> Self {
        Self {
            read: answer.read,
            rows: answer.rows,
            templates: answer.templates,
            published: answer.published,
        }
    }
}

impl From<TailResponse> for TailAnswer {
    fn from(response: TailResponse) -> Self {
        Self {
            read: response.read,
            rows: response.rows,
            templates: response.templates,
            published: response.published,
        }
    }
}

/// Why a tail replica gave no rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// It does not mine the group.
    NotHosted,
    /// `from` is below the oldest row it keeps: the querier restarts from
    /// a newer checkpoint.
    TooOld { oldest: u64 },
    /// It has no read point, or cannot serve the group's tail.
    Unavailable(String),
    /// The caller is not one of its `tail_clients`.
    Denied,
}

/// What a tail read comes to.
pub type Tailing = Pin<Box<dyn Future<Output = Result<TailAnswer, Refusal>> + Send>>;

/// The node's side of `Tail`.
pub trait TailTarget: Send + Sync + 'static {
    fn tail(&self, query: TailQuery) -> Tailing;
}

/// Who a tail replica serves: the identities its `tail_clients` lists.
#[derive(Debug, Clone)]
pub struct Clients(Admit);

#[derive(Debug, Clone)]
enum Admit {
    Only(BTreeSet<String>),
    #[cfg(test)]
    Anyone,
}

impl Clients {
    /// The callers whose certificates name one of `identities`.
    pub fn only<S: Into<String>>(identities: impl IntoIterator<Item = S>) -> Self {
        Self(Admit::Only(
            identities.into_iter().map(Into::into).collect(),
        ))
    }

    /// Every caller, certificate or not: plaintext transport tests.
    #[cfg(test)]
    pub(crate) fn anyone() -> Self {
        Self(Admit::Anyone)
    }

    /// Whether a caller whose certificate names `names` is admitted.
    #[must_use]
    pub fn admits(&self, names: &BTreeSet<String>) -> bool {
        match &self.0 {
            Admit::Only(listed) => !listed.is_disjoint(names),
            #[cfg(test)]
            Admit::Anyone => true,
        }
    }
}

/// The URI and DNS subject alternative names of the client certificate
/// that opened the connection; none on a plaintext one. The certificate is
/// already verified against the peer CA by the handshake.
pub(super) fn client_identities<B>(request: &tonic::codegen::http::Request<B>) -> BTreeSet<String> {
    use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};

    let Some(info) = request.extensions().get::<TlsConnectInfo<TcpConnectInfo>>() else {
        return BTreeSet::new();
    };
    let Some(certs) = info.peer_certs() else {
        return BTreeSet::new();
    };
    certs
        .first()
        .map(|leaf| subject_alt_names(leaf.as_ref()))
        .unwrap_or_default()
}

/// The URI and DNS names in the subject alternative names of the DER
/// certificate `der`; none if it does not parse.
fn subject_alt_names(der: &[u8]) -> BTreeSet<String> {
    use x509_parser::extensions::GeneralName;

    let Ok((_, cert)) = x509_parser::parse_x509_certificate(der) else {
        return BTreeSet::new();
    };
    let Ok(Some(san)) = cert.subject_alternative_name() else {
        return BTreeSet::new();
    };
    san.value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::URI(uri) => Some((*uri).to_owned()),
            GeneralName::DNSName(dns) => Some((*dns).to_owned()),
            _ => None,
        })
        .collect()
}

/// Serve one `Tail` call with `target`, the caller already `admitted` or
/// not by the gate.
pub(super) async fn serve(
    target: Option<Arc<dyn TailTarget>>,
    admitted: bool,
    request: TailRequest,
) -> Result<TailResponse, Status> {
    if request.version != VERSION {
        let message = format!("peer protocol version {} is not {VERSION}", request.version);
        return Err(refused(Status::failed_precondition(message), "not_hosted"));
    }
    if !admitted {
        return Err(refusal(Refusal::Denied));
    }
    let Some(target) = target else {
        return Err(refusal(Refusal::NotHosted));
    };
    target
        .tail(TailQuery::of(&request))
        .await
        .map(TailResponse::from)
        .map_err(refusal)
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
            Status::not_found("the group is not mined here"),
            "not_hosted",
        ),
        Refusal::TooOld { oldest } => {
            let mut status = refused(
                Status::out_of_range("the checkpoint is below the oldest row kept"),
                "too_old",
            );
            status.metadata_mut().insert(OLDEST, oldest.into());
            status
        }
        Refusal::Unavailable(why) => refused(Status::unavailable(why), "unavailable"),
        Refusal::Denied => refused(
            Status::permission_denied("not one of the tail's clients"),
            "denied",
        ),
    }
}

/// What a tail read of one replica came to, as the querier sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Answered(TailAnswer),
    NotHosted,
    TooOld {
        oldest: u64,
    },
    /// No answer to serve: no read point, a failed call or a timeout.
    Unavailable(String),
    Denied,
    /// The call was never sent.
    Unreached(String),
}

/// Ask `addr` over `channel` for `query`'s tail within `patience`, under
/// its gRPC client span.
pub(super) async fn traced_call(
    channel: Channel,
    (addr, tracing): (&str, Tracing),
    query: &TailQuery,
    patience: Duration,
) -> Outcome {
    let trace = Call::client(tracing, TAIL, Peer::parse(addr), true);
    let asking = call(channel, query.request(), patience);
    let (outcome, code) = tracing::Instrument::instrument(asking, trace.span()).await;
    trace.finish(code);
    outcome
}

async fn call(channel: Channel, request: TailRequest, patience: Duration) -> (Outcome, Code) {
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
        let codec = ProstCodec::<TailRequest, TailResponse>::default();
        match grpc
            .unary(request, PathAndQuery::from_static(TAIL), codec)
            .await
        {
            Ok(response) => (Outcome::Answered(response.into_inner().into()), Code::Ok),
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
            let late = Outcome::Unavailable("the tail did not answer in time".to_owned());
            (late, Code::DeadlineExceeded)
        })
}

/// An answer's status: a refusal only if the replica marked it as one, or
/// an older node that does not serve `Tail` at all.
fn classify(status: &Status) -> Outcome {
    let metadata = status.metadata();
    let text = |key| metadata.get(key).and_then(|v| v.to_str().ok());
    match (text(REFUSED), status.code()) {
        (Some("not_hosted"), _) | (None, Code::Unimplemented) => Outcome::NotHosted,
        (Some("too_old"), _) => Outcome::TooOld {
            oldest: text(OLDEST).and_then(|v| v.parse().ok()).unwrap_or(0),
        },
        (Some("denied"), _) => Outcome::Denied,
        _ => Outcome::Unavailable(status.message().to_owned()),
    }
}

#[cfg(test)]
mod tests;
