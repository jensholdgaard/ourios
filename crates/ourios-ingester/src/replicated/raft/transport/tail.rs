//! `Tail` and `TailScan` (RFC 0065 §6.7): how a querier reads a group's
//! **unpublished** rows for one tenant from a tail replica.
//!
//! - `Tail` fixes a quorum-confirmed read point and answers with it and the
//!   keys of the tenant's files published above the querier's checkpoint
//!   as of that point.
//! - `TailScan` returns the tenant's rows at or above the checkpoint as of
//!   a given read point, a page at a time: each page's rows, an Arrow IPC
//!   stream in the record files' schema, encode to at most [`PAGE_BYTES`],
//!   with the definitions of only the templates they reference and the
//!   position to continue after. Any tail replica of the group serves any
//!   page, once it has mined through the read point, so a backlog of any
//!   size is read without one answer outgrowing the message limit.
//!
//! Only a caller whose client certificate names an identity in the
//! replica's `tail_clients` (a URI or DNS subject alternative name) may
//! call either; any other, another replica included, is denied by the
//! server's gate before anything of its request is decoded (RFC0065.13),
//! and a caller that is listed may make no other peer call. A call carries
//! the query's tenant, already authorized by the querier (RFC 0047), and
//! the replica returns only that tenant's rows.
//!
//! A refusal names itself in the `ourios-tail-refused` metadata of its
//! status. Either call may be asked again: neither changes anything.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tonic::codegen::http::uri::PathAndQuery;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request, Status};
use tonic_prost::ProstCodec;

use super::VERSION;
use super::trace::{Call, Peer, Traced, Tracing, inject_current};
use super::wire::MAX_MESSAGE_BYTES;

pub use gate::Clients;
pub(super) use gate::client_identities;

mod gate;

const REFUSED: &str = "ourios-tail-refused";
const OLDEST: &str = "ourios-tail-oldest";

/// The most a `TailScan` page's rows may encode to: half the message
/// limit, leaving the rest for its template definitions and framing.
pub const PAGE_BYTES: usize = MAX_MESSAGE_BYTES / 2;

/// A querier's request for a group's read point and published keys.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct TailRequest {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(uint32, tag = "2")]
    pub group: u32,
    #[prost(string, tag = "3")]
    pub tenant: String,
    /// The checkpoint cut the querier read.
    #[prost(uint64, tag = "4")]
    pub from: u64,
}

impl Traced for TailRequest {}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct TailResponse {
    /// The quorum-confirmed read point.
    #[prost(uint64, tag = "1")]
    pub read: u64,
    /// The tenant's files published above `from` as of `read`.
    #[prost(string, repeated, tag = "2")]
    pub published: Vec<String>,
}

/// A row's ingest position in its group's log (RFC 0065 §6.6), where a
/// page ends and the next begins.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, prost::Message)]
pub struct WirePosition {
    #[prost(uint64, tag = "1")]
    pub seq: u64,
    #[prost(uint32, tag = "2")]
    pub index: u32,
}

/// A querier's request for one page of a group's tail rows.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct TailScanRequest {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(uint32, tag = "2")]
    pub group: u32,
    #[prost(string, tag = "3")]
    pub tenant: String,
    /// The checkpoint cut the querier read: rows at or above it.
    #[prost(uint64, tag = "4")]
    pub from: u64,
    /// The read point `Tail` gave: rows at or below it.
    #[prost(uint64, tag = "5")]
    pub read: u64,
    /// The query's time range, inclusive start and exclusive end, in Unix
    /// nanoseconds; absent bounds select every row.
    #[prost(uint64, optional, tag = "6")]
    pub start_unix_nano: Option<u64>,
    #[prost(uint64, optional, tag = "7")]
    pub end_unix_nano: Option<u64>,
    /// The position the previous page ended at; absent for the first.
    #[prost(message, optional, tag = "8")]
    pub after: Option<WirePosition>,
}

impl Traced for TailScanRequest {}

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

/// One page of tail rows.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct TailScanResponse {
    /// The page's rows, an Arrow IPC stream in the record files' schema,
    /// in position order; at most [`PAGE_BYTES`].
    #[prost(bytes = "vec", tag = "1")]
    pub rows: Vec<u8>,
    #[prost(message, repeated, tag = "2")]
    pub templates: Vec<TemplateDefinition>,
    /// Where the next page starts after; absent on the last page.
    #[prost(message, optional, tag = "3")]
    pub next: Option<WirePosition>,
}

/// A `Tail` read, the request without its framing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailQuery {
    pub group: u32,
    pub tenant: String,
    pub from: u64,
}

/// A group's read point and its tenant's published keys above `from`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailPoint {
    pub read: u64,
    pub published: Vec<String>,
}

/// A `TailScan` read, the request without its framing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanQuery {
    pub group: u32,
    pub tenant: String,
    pub from: u64,
    pub read: u64,
    /// `[start, end)` in Unix nanoseconds, when the query has one.
    pub range: Option<(u64, u64)>,
    pub after: Option<WirePosition>,
}

/// One page of rows, its templates, and where the next starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanPage {
    pub rows: Vec<u8>,
    pub templates: Vec<TemplateDefinition>,
    pub next: Option<WirePosition>,
}

impl TailQuery {
    pub(super) fn request(&self) -> TailRequest {
        TailRequest {
            version: VERSION,
            group: self.group,
            tenant: self.tenant.clone(),
            from: self.from,
        }
    }
}

impl ScanQuery {
    fn of(request: &TailScanRequest) -> Self {
        let range = match (request.start_unix_nano, request.end_unix_nano) {
            (None, None) => None,
            (start, end) => Some((start.unwrap_or(0), end.unwrap_or(u64::MAX))),
        };
        Self {
            group: request.group,
            tenant: request.tenant.clone(),
            from: request.from,
            read: request.read,
            range,
            after: request.after,
        }
    }

    pub(super) fn request(&self) -> TailScanRequest {
        TailScanRequest {
            version: VERSION,
            group: self.group,
            tenant: self.tenant.clone(),
            from: self.from,
            read: self.read,
            start_unix_nano: self.range.map(|(start, _)| start),
            end_unix_nano: self.range.map(|(_, end)| end),
            after: self.after,
        }
    }
}

/// Why a tail replica gave no answer.
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

/// What a `Tail` read comes to.
pub type Tailing = Pin<Box<dyn Future<Output = Result<TailPoint, Refusal>> + Send>>;

/// What a `TailScan` read comes to.
pub type Scanning = Pin<Box<dyn Future<Output = Result<ScanPage, Refusal>> + Send>>;

/// The node's side of `Tail` and `TailScan`.
pub trait TailTarget: Send + Sync + 'static {
    /// The group's read point and published keys.
    fn tail(&self, query: TailQuery) -> Tailing;
    /// One page of rows, each page's rows at most [`PAGE_BYTES`].
    fn scan(&self, query: ScanQuery) -> Scanning;
}

/// Serve one `Tail` call with `target`, its caller admitted by the
/// server's gate before the request was decoded.
pub(super) async fn serve(
    target: Option<Arc<dyn TailTarget>>,
    request: TailRequest,
) -> Result<TailResponse, Status> {
    let target = hosted(target, request.version)?;
    let query = TailQuery {
        group: request.group,
        tenant: request.tenant,
        from: request.from,
    };
    let point = target.tail(query).await.map_err(refusal)?;
    Ok(TailResponse {
        read: point.read,
        published: point.published,
    })
}

/// Serve one `TailScan` call with `target`, as [`serve`] does.
pub(super) async fn serve_scan(
    target: Option<Arc<dyn TailTarget>>,
    request: TailScanRequest,
) -> Result<TailScanResponse, Status> {
    let target = hosted(target, request.version)?;
    let page = target
        .scan(ScanQuery::of(&request))
        .await
        .map_err(refusal)?;
    Ok(TailScanResponse {
        rows: page.rows,
        templates: page.templates,
        next: page.next,
    })
}

/// `target`, for a request on this protocol `version`.
fn hosted(
    target: Option<Arc<dyn TailTarget>>,
    version: u32,
) -> Result<Arc<dyn TailTarget>, Status> {
    if version != VERSION {
        let message = format!("peer protocol version {version} is not {VERSION}");
        return Err(refused(Status::failed_precondition(message), "not_hosted"));
    }
    target.ok_or_else(|| refusal(Refusal::NotHosted))
}

/// The refusal of a caller that is not one of the tail's clients.
pub(super) fn denied() -> Status {
    refusal(Refusal::Denied)
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

/// What a read of one tail replica came to, as the querier sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome<T> {
    Answered(T),
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

/// Where a call goes: the peer's address and a channel to it, or why it
/// cannot be reached.
pub(super) type Reach = Result<(String, Channel), String>;

/// One of the two tail calls: where it goes, what it sends, and what its
/// answer is read as.
pub trait TailCall {
    #[doc(hidden)]
    const PATH: &'static str;
    #[doc(hidden)]
    type Request: prost::Message + Send + Sync + 'static;
    #[doc(hidden)]
    type Response: prost::Message + Default + Send + Sync + 'static;
    /// What the caller gets back.
    type Answer: From<Self::Response>;
    #[doc(hidden)]
    fn request(&self) -> Self::Request;
}

impl TailCall for TailQuery {
    const PATH: &'static str = super::TAIL;
    type Request = TailRequest;
    type Response = TailResponse;
    type Answer = TailPoint;
    fn request(&self) -> TailRequest {
        TailQuery::request(self)
    }
}

impl TailCall for ScanQuery {
    const PATH: &'static str = super::TAIL_SCAN;
    type Request = TailScanRequest;
    type Response = TailScanResponse;
    type Answer = ScanPage;
    fn request(&self) -> TailScanRequest {
        ScanQuery::request(self)
    }
}

impl From<TailResponse> for TailPoint {
    fn from(response: TailResponse) -> Self {
        Self {
            read: response.read,
            published: response.published,
        }
    }
}

impl From<TailScanResponse> for ScanPage {
    fn from(response: TailScanResponse) -> Self {
        Self {
            rows: response.rows,
            templates: response.templates,
            next: response.next,
        }
    }
}

/// Send `request` to `path` on the peer `reach` names within `patience`,
/// under its gRPC client span, the answer read as `answer` says.
pub(super) async fn traced<Q: TailCall>(
    (reach, tracing): (Reach, Tracing),
    query: &Q,
    patience: Duration,
) -> Outcome<Q::Answer> {
    let (path, request) = (Q::PATH, query.request());
    let (addr, channel) = match reach {
        Ok(reached) => reached,
        Err(why) => return Outcome::Unreached(why),
    };
    let trace = Call::client(tracing, path, Peer::parse(&addr), true);
    let asking = call::<Q::Request, Q::Response>(channel, (path, request), patience);
    let (outcome, code) = tracing::Instrument::instrument(asking, trace.span()).await;
    trace.finish(code);
    outcome.map(Q::Answer::from)
}

impl<T> Outcome<T> {
    fn map<U>(self, f: impl FnOnce(T) -> U) -> Outcome<U> {
        match self {
            Self::Answered(answer) => Outcome::Answered(f(answer)),
            Self::NotHosted => Outcome::NotHosted,
            Self::TooOld { oldest } => Outcome::TooOld { oldest },
            Self::Unavailable(why) => Outcome::Unavailable(why),
            Self::Denied => Outcome::Denied,
            Self::Unreached(why) => Outcome::Unreached(why),
        }
    }
}

/// Send `request` to `path` over `channel`, the connection and the answer
/// together bounded by `patience`.
async fn call<Req, Resp>(
    channel: Channel,
    (path, request): (&'static str, Req),
    patience: Duration,
) -> (Outcome<Resp>, Code)
where
    Req: prost::Message + Send + Sync + 'static,
    Resp: prost::Message + Default + Send + Sync + 'static,
{
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
        let codec = ProstCodec::<Req, Resp>::default();
        match grpc
            .unary(request, PathAndQuery::from_static(path), codec)
            .await
        {
            Ok(response) => (Outcome::Answered(response.into_inner()), Code::Ok),
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
/// an older node that does not serve the call at all.
fn classify<T>(status: &Status) -> Outcome<T> {
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
