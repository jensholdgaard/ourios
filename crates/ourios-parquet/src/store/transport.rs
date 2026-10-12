//! The S3 client's HTTP transport wrapper: one timed attempt per request
//! the client makes, retries included, and inside a query phase one
//! `CLIENT` span (see [`super::client_span`]).

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use object_store::ClientOptions;
use object_store::client::{
    HttpClient, HttpConnector, HttpError, HttpRequest, HttpResponse, HttpService,
};
use opentelemetry::KeyValue;

use super::client_span::{ClientSpan, S3Target};
use super::metrics::{
    Attempts, ERROR_TYPE, Failure, Metrics, OTHER, current_attempts, http_error_kind,
};

/// The S3 client's connector, timing every HTTP attempt and, inside a query
/// phase, tracing it as a `CLIENT` span.
#[derive(Debug)]
pub(super) struct MeteredConnector<C> {
    pub(super) inner: C,
    pub(super) metrics: Metrics,
    pub(super) target: S3Target,
}

impl<C: HttpConnector> HttpConnector for MeteredConnector<C> {
    fn connect(&self, options: &ClientOptions) -> object_store::Result<HttpClient> {
        Ok(HttpClient::new(MeteredHttp {
            inner: self.inner.connect(options)?,
            metrics: self.metrics.clone(),
            target: self.target.clone(),
        }))
    }
}

#[derive(Debug)]
struct MeteredHttp {
    inner: HttpClient,
    metrics: Metrics,
    target: S3Target,
}

#[async_trait]
impl HttpService for MeteredHttp {
    async fn call(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        let scope = current_attempts().map(|attempts| {
            let identity = request_identity(&req);
            attempts.attempt(identity);
            (attempts, identity)
        });
        let attributes = request_attributes(&req);
        let span = ClientSpan::start(&req, &self.target, &attributes);
        let mut attempt = Attempt {
            metrics: self.metrics.clone(),
            attributes,
            started: Instant::now(),
            scope,
            span,
            done: false,
        };
        let response = self.inner.execute(req).await;
        match &response {
            Ok(response) => {
                if let Some(span) = &mut attempt.span {
                    span.response(response);
                }
                let status = response.status().as_u16();
                attempt.finish(
                    Some(status),
                    (status >= 400).then_some(Failure::Status(status)),
                );
            }
            Err(e) => attempt.finish(None, Some(Failure::Transport(http_error_kind(e.kind())))),
        }
        response
    }
}

/// One HTTP attempt, recorded when it ends. Dropped before a response, as
/// when its caller gives up on the request, it records as cancelled.
struct Attempt {
    metrics: Metrics,
    attributes: Vec<KeyValue>,
    started: Instant,
    /// The logical operation it belongs to, and its request identity.
    scope: Option<(Arc<Attempts>, u64)>,
    span: Option<ClientSpan>,
    done: bool,
}

impl Attempt {
    fn finish(&mut self, status: Option<u16>, failure: Option<Failure>) {
        if std::mem::replace(&mut self.done, true) {
            return;
        }
        let took = self.started.elapsed();
        let mut attributes = std::mem::take(&mut self.attributes);
        if let Some(status) = status {
            attributes.push(KeyValue::new(
                "http.response.status_code",
                i64::from(status),
            ));
        }
        if let Some(failure) = failure {
            attributes.push(KeyValue::new(ERROR_TYPE, failure.error_type()));
        }
        self.metrics.get().record_http(took, &attributes);
        if let Some((attempts, identity)) = &self.scope {
            attempts.ended(*identity, failure);
        }
        if let Some(span) = self.span.take() {
            span.end(failure);
        }
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        self.finish(None, Some(Failure::Cancelled));
    }
}

/// What makes two requests the same request: method, URI and `Range`, so
/// the parallel ranged GETs of one `get_ranges` stay distinct. Other headers
/// are left out, since a retry re-signs. A body retry resumes with a
/// narrower `Range`, so it is not counted.
fn request_identity(req: &HttpRequest) -> u64 {
    let mut hasher = DefaultHasher::new();
    req.method().hash(&mut hasher);
    req.uri().hash(&mut hasher);
    req.headers()
        .get("range")
        .map(object_store::HeaderValue::as_bytes)
        .hash(&mut hasher);
    hasher.finish()
}

/// The HTTP client attributes known before the response.
fn request_attributes(req: &HttpRequest) -> Vec<KeyValue> {
    // Spelled out because `as_str` borrows the request and the attribute
    // needs a `'static` value.
    let method = match req.method().as_str() {
        "GET" => "GET",
        "HEAD" => "HEAD",
        "PUT" => "PUT",
        "POST" => "POST",
        "DELETE" => "DELETE",
        "PATCH" => "PATCH",
        "OPTIONS" => "OPTIONS",
        _ => OTHER,
    };
    let uri = req.uri();
    let mut attributes = Vec::with_capacity(5);
    attributes.push(KeyValue::new("http.request.method", method));
    if let Some(host) = uri.host() {
        attributes.push(KeyValue::new("server.address", host.to_owned()));
    }
    let port = uri.port_u16().or(match uri.scheme_str() {
        Some("https") => Some(443),
        Some("http") => Some(80),
        _ => None,
    });
    if let Some(port) = port {
        attributes.push(KeyValue::new("server.port", i64::from(port)));
    }
    attributes
}
