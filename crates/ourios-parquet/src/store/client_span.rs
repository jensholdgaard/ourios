//! Object-store `CLIENT` spans (RFC 0038 §3.7.3): one `S3.{Operation}` span
//! per HTTP request the S3 backend makes inside a query phase.
//!
//! A span opens only when the request's `OTel` context carries both a valid,
//! sampled parent span and the [`QueryPhase`] marker, so ingest, compaction
//! and the `DataFusion` scan's own reads stay span-free and no span is ever
//! a root. Attributes are the upstream AWS SDK / S3 client-span conventions;
//! the status follows *Recording errors* (a `>= 400` response or a transport
//! failure is `Error`, a request its caller abandoned is left unset).

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use object_store::client::{HttpRequest, HttpResponse};
use opentelemetry::global::{BoxedSpan, BoxedTracer};
use opentelemetry::trace::{
    Span as _, SpanBuilder, SpanKind, Status, TraceContextExt as _, Tracer as _,
};
use opentelemetry::{Context, KeyValue};

use super::metrics::Failure;

/// The `OTel` [`Context`] value that marks a query phase (`load template_map`
/// and the spans under it): object-store requests made under it open `CLIENT`
/// spans. The querier attaches it with the phase span's context.
#[derive(Clone, Copy, Debug)]
pub struct QueryPhase;

/// Where `CLIENT` spans are recorded: the process-global tracer provider,
/// resolved per span so a handle opened before telemetry starts still
/// traces once it has, or a tracer of its own (tests).
#[derive(Clone, Default)]
pub(crate) enum Tracing {
    #[default]
    Global,
    #[cfg_attr(not(test), allow(dead_code))] // only tests own a tracer today
    Owned(Arc<BoxedTracer>),
}

impl fmt::Debug for Tracing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Global => "Tracing::Global",
            Self::Owned(_) => "Tracing::Owned",
        })
    }
}

impl Tracing {
    fn start(&self, builder: SpanBuilder, parent: &Context) -> BoxedSpan {
        match self {
            Self::Global => {
                ourios_core::telemetry::tracer("ourios.storage").build_with_context(builder, parent)
            }
            Self::Owned(tracer) => tracer.build_with_context(builder, parent),
        }
    }
}

/// The configured bucket the S3 client addresses, and where its spans go.
#[derive(Clone, Debug)]
pub(super) struct S3Target {
    pub(super) bucket: String,
    pub(super) region: Option<String>,
    /// The configured endpoint's host, `None` for the AWS default, where
    /// the S3 hosts are under `amazonaws.com`. A request to any other host
    /// (instance metadata, a credential endpoint) is not an S3 request.
    pub(super) endpoint_host: Option<String>,
    /// The configured endpoint's path (`/prefix`), empty for none: an
    /// endpoint override may sit below a path, and path-style requests then
    /// start with it.
    pub(super) path_prefix: String,
    pub(super) tracing: Tracing,
}

/// One request's `CLIENT` span, ended by [`Self::end`] or on drop.
pub(super) struct ClientSpan(BoxedSpan);

impl ClientSpan {
    /// Open the span for `req`, or `None` outside a sampled query phase or
    /// for a request not addressed to the bucket (a credential endpoint).
    /// `request_attributes` are the HTTP client attributes known up front.
    pub(super) fn start(
        req: &HttpRequest,
        target: &S3Target,
        request_attributes: &[KeyValue],
    ) -> Option<Self> {
        let cx = Context::current();
        if !in_traced_phase(&cx) {
            return None;
        }
        let key = object_key(req, target)?;
        let operation = S3Operation::of(req);
        let builder = SpanBuilder::from_name(operation.span_name())
            .with_kind(SpanKind::Client)
            .with_attributes(attributes(operation, key, target, request_attributes));
        Some(Self(target.tracing.start(builder, &cx)))
    }

    /// Record what the response says: its status code and request ids.
    pub(super) fn response(&mut self, response: &HttpResponse) {
        self.0.set_attribute(KeyValue::new(
            "http.response.status_code",
            i64::from(response.status().as_u16()),
        ));
        let headers = response.headers();
        for (header, attribute) in [
            ("x-amz-request-id", "aws.request_id"),
            ("x-amz-id-2", "aws.extended_request_id"),
        ] {
            if let Some(value) = headers.get(header).and_then(|v| v.to_str().ok()) {
                self.0
                    .set_attribute(KeyValue::new(attribute, value.to_owned()));
            }
        }
    }

    /// End the span, `failure` saying how the request failed.
    pub(super) fn end(mut self, failure: Option<Failure>) {
        match failure {
            // Semconv: a request its caller abandoned is not an error.
            None | Some(Failure::Cancelled) => {}
            Some(failure) => {
                self.0.set_attribute(KeyValue::new(
                    ourios_core::telemetry::ERROR_TYPE,
                    failure.error_type(),
                ));
                self.0.set_status(Status::error(""));
            }
        }
        self.0.end();
    }
}

/// Whether `cx` is a query phase under a valid, sampled parent span. A
/// context can carry the sampled flag on an invalid span context; a span
/// started under that would be a root, which §3.7.3 rules out.
fn in_traced_phase(cx: &Context) -> bool {
    let span = cx.span();
    let parent = span.span_context();
    cx.get::<QueryPhase>().is_some() && parent.is_valid() && parent.is_sampled()
}

/// The span's attributes: the HTTP client ones known up front, then the
/// AWS SDK / S3 ones.
fn attributes(
    operation: S3Operation,
    key: String,
    target: &S3Target,
    request_attributes: &[KeyValue],
) -> Vec<KeyValue> {
    let mut attributes = Vec::with_capacity(request_attributes.len() + 6);
    attributes.extend_from_slice(request_attributes);
    attributes.push(KeyValue::new("rpc.system", "aws-api"));
    attributes.push(KeyValue::new("rpc.service", "S3"));
    attributes.push(KeyValue::new("aws.s3.bucket", target.bucket.clone()));
    if let Some(method) = operation.method() {
        attributes.push(KeyValue::new("rpc.method", method));
    }
    if operation.on_object() && !key.is_empty() {
        attributes.push(KeyValue::new("aws.s3.key", key));
    }
    if let Some(region) = &target.region {
        attributes.push(KeyValue::new("cloud.region", region.clone()));
    }
    attributes
}

/// The object key `req` addresses in the target's bucket, decoded from
/// the request path: virtual-hosted (`bucket.host[/prefix]/key`) or
/// path-style (`host[/prefix]/bucket/key`). Empty for a bucket-level
/// request, `None` for a request to anything but the bucket on the
/// configured S3 host.
fn object_key(req: &HttpRequest, target: &S3Target) -> Option<String> {
    let host = req.uri().host()?;
    let path = below_prefix(req.uri().path(), &target.path_prefix)?;
    let bucket = target.bucket.as_str();
    let encoded = match host.strip_prefix(bucket).and_then(|h| h.strip_prefix('.')) {
        Some(base) if is_s3_host(base, target) => path.trim_start_matches('/'),
        _ if is_s3_host(host, target) => {
            let rest = below_prefix(path, &format!("/{bucket}"))?;
            rest.strip_prefix('/').unwrap_or(rest)
        }
        _ => return None,
    };
    Some(
        percent_encoding::percent_decode_str(encoded)
            .decode_utf8_lossy()
            .into_owned(),
    )
}

/// Whether `host` is the configured S3 endpoint, or an AWS S3 host when no
/// endpoint is configured.
fn is_s3_host(host: &str, target: &S3Target) -> bool {
    match &target.endpoint_host {
        Some(endpoint) => host == endpoint,
        None => host.ends_with(".amazonaws.com"),
    }
}

/// `path` with `prefix` removed, where `prefix` ends at a path-segment
/// boundary; `None` when `path` does not start with it.
fn below_prefix<'p>(path: &'p str, prefix: &str) -> Option<&'p str> {
    let rest = path.strip_prefix(prefix)?;
    match rest {
        _ if prefix.is_empty() => Some(rest),
        "" => Some(rest),
        _ if rest.starts_with('/') => Some(rest),
        _ => None,
    }
}

/// The S3 API operation a request performs (RFC 0038 §3.7.3's table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum S3Operation {
    ListObjectsV2,
    GetObject,
    HeadObject,
    CopyObject,
    UploadPart,
    PutObject,
    CreateMultipartUpload,
    CompleteMultipartUpload,
    DeleteObjects,
    AbortMultipartUpload,
    DeleteObject,
    Unknown,
}

/// A request's query string, read for the parameter names that pick the
/// operation.
#[derive(Clone, Copy)]
struct Query<'q>(&'q str);

impl Query<'_> {
    fn has(self, name: &str) -> bool {
        self.0
            .split('&')
            .any(|pair| pair.split('=').next() == Some(name))
    }
}

impl S3Operation {
    fn of(req: &HttpRequest) -> Self {
        let query = Query(req.uri().query().unwrap_or_default());
        match req.method().as_str() {
            "GET" => Self::get(query),
            "HEAD" => Self::HeadObject,
            "PUT" => Self::put(req.headers().contains_key("x-amz-copy-source"), query),
            "POST" => Self::post(query),
            "DELETE" => Self::delete(query),
            _ => Self::Unknown,
        }
    }

    fn get(query: Query<'_>) -> Self {
        if query.has("list-type") {
            Self::ListObjectsV2
        } else {
            Self::GetObject
        }
    }

    fn put(copy: bool, query: Query<'_>) -> Self {
        match (copy, query.has("partNumber")) {
            (true, _) => Self::CopyObject,
            (false, true) => Self::UploadPart,
            (false, false) => Self::PutObject,
        }
    }

    fn post(query: Query<'_>) -> Self {
        [
            ("uploads", Self::CreateMultipartUpload),
            ("uploadId", Self::CompleteMultipartUpload),
            ("delete", Self::DeleteObjects),
        ]
        .into_iter()
        .find(|(name, _)| query.has(name))
        .map_or(Self::Unknown, |(_, operation)| operation)
    }

    fn delete(query: Query<'_>) -> Self {
        if query.has("uploadId") {
            Self::AbortMultipartUpload
        } else {
            Self::DeleteObject
        }
    }

    /// The `rpc.method`, `None` for a request no operation matches.
    fn method(self) -> Option<&'static str> {
        Some(match self {
            Self::ListObjectsV2 => "ListObjectsV2",
            Self::GetObject => "GetObject",
            Self::HeadObject => "HeadObject",
            Self::CopyObject => "CopyObject",
            Self::UploadPart => "UploadPart",
            Self::PutObject => "PutObject",
            Self::CreateMultipartUpload => "CreateMultipartUpload",
            Self::CompleteMultipartUpload => "CompleteMultipartUpload",
            Self::DeleteObjects => "DeleteObjects",
            Self::AbortMultipartUpload => "AbortMultipartUpload",
            Self::DeleteObject => "DeleteObject",
            Self::Unknown => return None,
        })
    }

    /// `Service.Operation`, or the service alone when no operation matches.
    fn span_name(self) -> Cow<'static, str> {
        self.method().map_or(Cow::Borrowed("S3"), |method| {
            Cow::Owned(format!("S3.{method}"))
        })
    }

    /// Whether the operation names one object, so `aws.s3.key` applies.
    fn on_object(self) -> bool {
        !matches!(
            self,
            Self::ListObjectsV2 | Self::DeleteObjects | Self::Unknown
        )
    }
}

#[cfg(test)]
mod tests;
