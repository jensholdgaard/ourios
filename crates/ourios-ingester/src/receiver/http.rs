//! OTLP/HTTP listener (RFC 0003 §6.2).
//!
//! An `axum` `POST` handler at a configurable path (default `/v1/logs`)
//! that decodes the body per `Content-Type` (`application/x-protobuf` or
//! `application/json`) and `Content-Encoding` (`identity` or `gzip`),
//! hands the decoded `ExportLogsServiceRequest` to the `IngestPipeline`
//! (WAL-before-ack), and returns an `ExportLogsServiceResponse`.
//!
//! Transport errors are controlled (RFC0003.11): unsupported media type
//! / encoding → 415, malformed body → 400, oversize → 413, an
//! unconfigured path → 404, tenant-resolution failure → 400. No panics.
//!
//! The pipeline is shared behind a plain `Arc`: its group-commit
//! coordinator serializes the single-writer WAL internally (RFC 0008
//! §3.1) while letting concurrent requests batch their fsyncs
//! (RFC0008.8). `ingest` is async, so the handler simply `.await`s it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceResponse;
use prost::Message;

use opentelemetry::context::FutureExt as _;

use crate::receiver::decode::{decode_json, decode_protobuf};
use crate::receiver::pipeline::{IngestFailure, ReceiveError, SharedPipeline};
use crate::receiver::selector;
use ourios_serving::auth::{AuthBinding, AuthError, AuthResolver};
use ourios_serving::propagation::extract_context;

/// OTLP/HTTP listener configuration.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// The accepted `POST` path (default `/v1/logs`; configurable per
    /// §6.2 / RFC0003.14).
    pub path: String,
    /// Maximum request body size in bytes; a larger body is rejected with
    /// 413 (RFC0003.11).
    pub max_body_bytes: usize,
    /// The RFC 0026 / RFC 0029 credential resolver; the default
    /// (`AuthResolver::static_only(None)`) is open mode (§3.1). Otherwise
    /// every request must carry a resolvable `Authorization: Bearer`
    /// credential (→ 401) and its batch is bound to the resolved tenant
    /// set (→ 403).
    pub auth: AuthResolver,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            path: "/v1/logs".to_owned(),
            max_body_bytes: 4 * 1024 * 1024,
            auth: AuthResolver::static_only(None),
        }
    }
}

/// Handler state: the shared pipeline plus the decompressed-size cap
/// (`DefaultBodyLimit` only bounds the *compressed* body, so gzip is
/// bounded separately to defuse a decompression bomb). Authentication
/// runs in [`AuthLayer`], before the handler.
#[derive(Clone)]
struct AppState {
    pipeline: SharedPipeline,
    max_decompressed_bytes: usize,
}

/// Build the OTLP/HTTP router over `pipeline`.
pub fn router(pipeline: SharedPipeline, config: &HttpConfig) -> Router {
    let state = AppState {
        pipeline,
        max_decompressed_bytes: config.max_body_bytes,
    };
    Router::new()
        .route(&config.path, post(handle_logs))
        .layer(DefaultBodyLimit::max(config.max_body_bytes))
        // Outermost: RFC 0026 §3.2 authentication precedes everything —
        // an unauthenticated request is rejected before the body is
        // even collected.
        .layer(AuthLayer::new(config.auth.clone()))
        .with_state(state)
}

/// Per-request bearer authentication for the OTLP/HTTP listener — the
/// HTTP twin of the gRPC transport's `AuthLayer` (`grpc.rs`): resolve
/// the `Authorization` header through the shared [`AuthResolver`],
/// reject with `401`/`503` before any body handling, and attach the
/// resolved [`AuthBinding`] to the request's extensions for the
/// handler's per-batch tenant check. With nothing configured every
/// request passes through unbound (open mode, §3.1).
#[derive(Clone)]
struct AuthLayer {
    resolver: AuthResolver,
    /// Rejection telemetry (RFC 0026 §3.4). The instruments resolve by
    /// name through the global meter, so this instance aggregates with
    /// the pipeline's.
    metrics: Arc<crate::metrics::IngestMetrics>,
}

impl AuthLayer {
    /// A layer over `resolver` (see [`AuthResolver`] for open mode).
    fn new(resolver: AuthResolver) -> Self {
        Self {
            resolver,
            metrics: Arc::new(crate::metrics::IngestMetrics::new()),
        }
    }
}

impl<S> tower::Layer<S> for AuthLayer {
    type Service = AuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthService {
            inner,
            resolver: self.resolver.clone(),
            metrics: Arc::clone(&self.metrics),
        }
    }
}

/// The [`AuthLayer`] service: authenticate, then delegate.
#[derive(Clone)]
struct AuthService<S> {
    inner: S,
    resolver: AuthResolver,
    metrics: Arc<crate::metrics::IngestMetrics>,
}

impl<S, ReqBody, ResBody> tower::Service<axum::http::Request<ReqBody>> for AuthService<S>
where
    S: tower::Service<axum::http::Request<ReqBody>, Response = axum::http::Response<ResBody>>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
    ReqBody: Send + 'static,
    ResBody: Default,
{
    type Response = axum::http::Response<ResBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: axum::http::Request<ReqBody>) -> Self::Future {
        // The tower readiness dance: `poll_ready` reserved capacity on
        // `self.inner`, so that instance (not a fresh clone) must serve
        // this call; the clone waits for its own `poll_ready` next time.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let resolver = self.resolver.clone();
        let metrics = Arc::clone(&self.metrics);
        Box::pin(async move {
            let authorization = request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            match resolver.authenticate(authorization.as_deref()).await {
                Ok(None) => {}
                Ok(Some(binding)) => {
                    // Arc, not the binding itself: axum's `Extension`
                    // extractor clones out of the extensions map, and
                    // `AuthBinding` deep-clones its tenant sets — the
                    // gRPC side borrows from extensions and pays no
                    // per-request clone, so this keeps parity.
                    request.extensions_mut().insert(Arc::new(binding));
                }
                // One undifferentiated rejection: missing vs malformed vs
                // unknown would be a probing oracle (RFC 0026 §3.2). §3.4:
                // the rejection counts on `ourios.ingest.batches`
                // (`error.type = unauthenticated`).
                Err(AuthError::Unauthenticated) => {
                    metrics.record_rejected_batch(crate::metrics::ERROR_TYPE_UNAUTHENTICATED);
                    return Ok(reject(StatusCode::UNAUTHORIZED));
                }
                // RFC 0047 §3.1: the resolver could not answer — fail
                // closed, 503.
                Err(AuthError::Unavailable) => {
                    metrics.record_rejected_batch(crate::metrics::ERROR_TYPE_UPSTREAM_UNAVAILABLE);
                    return Ok(reject(StatusCode::SERVICE_UNAVAILABLE));
                }
            }
            inner.call(request).await
        })
    }
}

/// An empty-bodied rejection response — the same surface the handler
/// produced before the layer existed.
fn reject<ResBody: Default>(status: StatusCode) -> axum::http::Response<ResBody> {
    let mut response = axum::http::Response::new(ResBody::default());
    *response.status_mut() = status;
    response
}

/// The OTLP wire format selected by `Content-Type`.
#[derive(Clone, Copy)]
enum WireFormat {
    Protobuf,
    Json,
}

/// The supported request `Content-Encoding`s.
enum Encoding {
    Identity,
    Gzip,
}

async fn handle_logs(
    State(state): State<AppState>,
    binding: Option<axum::Extension<Arc<AuthBinding>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // RFC 0026 §3.2: authentication already ran in `AuthLayer`, before
    // the body was collected; a present extension is the bound
    // credential, absence is open mode. (Arc: cloning the extension is
    // a refcount bump, not a tenant-set deep-clone.)
    let binding = binding.map(|axum::Extension(b)| b);
    // RFC 0046 §3.1: the tenant selector is required, exactly once, and
    // decided before authorization against the set, before decode, before
    // any WAL work — a missing/malformed selector is a 400 with the reason.
    let tenant = match selector::from_headers(&headers) {
        Ok(tenant) => tenant,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };

    let Some(format) = content_type(&headers) else {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    };
    let raw = match content_encoding(&headers) {
        Some(Encoding::Identity) => body.to_vec(),
        Some(Encoding::Gzip) => match gunzip(&body, state.max_decompressed_bytes) {
            Ok(bytes) => bytes,
            // Corrupt gzip is a malformed request (400); a body that
            // decompresses past the limit is too large (413) — a
            // decompression bomb, since DefaultBodyLimit only bounds the
            // compressed bytes.
            Err(GunzipError::Corrupt) => return StatusCode::BAD_REQUEST.into_response(),
            Err(GunzipError::TooLarge) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        },
        None => return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response(),
    };
    let decoded = match format {
        WireFormat::Protobuf => decode_protobuf(&raw).map(|request| (request, false)),
        WireFormat::Json => decode_json(&raw),
    };
    let Ok((request, lenient_json)) = decoded else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if lenient_json {
        // Rare by construction (spec-valid payloads upstream with-serde
        // rejects — ourios#549); debug so a client emitting them at
        // volume can't turn the log stream into the bottleneck. The
        // countable signal is `ourios.ingest.json.lenient` on the
        // batches counter.
        tracing::debug!("OTLP/JSON payload parsed via the lenient unset-AnyValue retry");
    }

    // WAL-before-ack ingest. The fsync is batched by the group-commit
    // coordinator (RFC0008.8), which offloads its blocking `sync`, so the
    // handler just awaits. Run it on its own task so a panic in the
    // pipeline/miner is contained as a 500 (the handler promises not to
    // panic) rather than aborting the connection.
    // RFC 0039 §3.3: as on the gRPC arm, the `ingest logs` span is created
    // inside `ingest_bound` past this spawn, so the caller's context is
    // extracted from the request headers here and re-attached inside the task.
    let parent = extract_context(&headers);
    let pipeline = state.pipeline.clone();
    match tokio::spawn(
        async move {
            pipeline
                .ingest_bound(request, tenant, binding.as_deref(), lenient_json)
                .await
        }
        .with_context(parent),
    )
    .await
    {
        Ok(Ok(_)) => success_response(format),
        Ok(Err(e)) => ingest_error_status(&e).into_response(),
        // The ingest task panicked — a genuine, non-retryable internal bug.
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Map a settled ingest failure to its HTTP status (RFC 0018 §3.2).
///
/// Permanent client errors are non-retryable but split by class:
/// a tenant outside the token's set → `403`; an oversize payload
/// (`AppendError::TooLarge`, over the 16 MiB WAL frame ceiling) → `413`. Any
/// other WAL append/sync failure is *transient* (the batch was not acked,
/// §3.4) → retryable `503`, so compliant clients re-send rather than drop
/// data (a non-retryable `500` would tell them to drop it).
///
/// Adapt the shared [`IngestFailure`] classification to HTTP status
/// vocabulary — the exhaustive `ReceiveError` match (and its
/// build-breaking-on-new-variant property) lives beside the error
/// type in `pipeline.rs`.
fn ingest_error_status(error: &ReceiveError) -> StatusCode {
    match IngestFailure::classify(error) {
        IngestFailure::Denied => StatusCode::FORBIDDEN,
        IngestFailure::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        IngestFailure::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        IngestFailure::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Map `Content-Type` to a wire format, ignoring any `; charset=…`
/// parameters. `None` = missing or unsupported (→ 415).
fn content_type(headers: &HeaderMap) -> Option<WireFormat> {
    let value = headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
    // Media types are case-insensitive; ignore any `; charset=…` params.
    let media_type = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match media_type.as_str() {
        "application/x-protobuf" => Some(WireFormat::Protobuf),
        "application/json" => Some(WireFormat::Json),
        _ => None,
    }
}

/// Map `Content-Encoding` to a supported encoding. Absent or `identity`
/// is identity; `gzip` is supported; anything else (`zstd`, `br`, …) is
/// `None` (→ 415; zstd is deferred per §9).
fn content_encoding(headers: &HeaderMap) -> Option<Encoding> {
    match headers.get(header::CONTENT_ENCODING) {
        None => Some(Encoding::Identity),
        // Content-Encoding tokens are case-insensitive.
        Some(value) => match value.to_str().ok()?.trim().to_ascii_lowercase().as_str() {
            "" | "identity" => Some(Encoding::Identity),
            "gzip" => Some(Encoding::Gzip),
            _ => None,
        },
    }
}

/// Why a gzip body was rejected.
enum GunzipError {
    /// Not a valid gzip stream.
    Corrupt,
    /// The decompressed size exceeded `max` — a decompression bomb.
    TooLarge,
}

/// Decompress a gzip body, refusing to inflate past `max` bytes
/// (`DefaultBodyLimit` bounds only the compressed body, so an attacker
/// could otherwise expand a tiny upload into an unbounded allocation).
fn gunzip(bytes: &[u8], max: usize) -> Result<Vec<u8>, GunzipError> {
    use std::io::Read;
    // Read one byte past the cap so we can distinguish "exactly max" from
    // "over the cap".
    let cap = max.saturating_add(1) as u64;
    let mut decoder = flate2::read::GzDecoder::new(bytes).take(cap);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|_| GunzipError::Corrupt)?;
    if out.len() > max {
        return Err(GunzipError::TooLarge);
    }
    Ok(out)
}

/// A 200 carrying an empty `ExportLogsServiceResponse` (`partial_success`
/// unset), encoded in the request's wire format.
fn success_response(format: WireFormat) -> Response {
    let response = ExportLogsServiceResponse::default();
    match format {
        WireFormat::Protobuf => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/x-protobuf")],
            response.encode_to_vec(),
        )
            .into_response(),
        WireFormat::Json => match serde_json::to_vec(&response) {
            Ok(body) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response(),
            // Encoding the (trivial) success response shouldn't fail; if
            // it ever did, a 500 is honest — never a 200 with an empty
            // body.
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{ReceiveError, StatusCode, ingest_error_status};
    use ourios_wal::{AppendError, SyncError};

    #[test]
    fn tenant_denied_is_403() {
        let e = ReceiveError::TenantDenied {
            token_name: "edge".to_string(),
            tenant: ourios_core::tenant::TenantId::new("intruder"),
        };
        assert_eq!(ingest_error_status(&e), StatusCode::FORBIDDEN);
    }

    #[test]
    fn oversize_payload_is_413() {
        let e = ReceiveError::WalAppend(AppendError::TooLarge {
            len: 32 * 1024 * 1024,
            limit: 16 * 1024 * 1024,
        });
        assert_eq!(ingest_error_status(&e), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn transient_wal_append_is_503() {
        let e = ReceiveError::WalAppend(AppendError::Io {
            op: "write",
            source: std::io::Error::other("io"),
        });
        assert_eq!(ingest_error_status(&e), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn quiesced_wal_append_is_503() {
        let e = ReceiveError::WalAppend(AppendError::QuiescedAfterRotationFailure);
        assert_eq!(ingest_error_status(&e), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn transient_wal_sync_is_503() {
        let e = ReceiveError::WalSync(SyncError::Io {
            op: "fdatasync",
            source: std::io::Error::other("io"),
        });
        assert_eq!(ingest_error_status(&e), StatusCode::SERVICE_UNAVAILABLE);
    }
}
