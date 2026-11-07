//! OTLP/gRPC listener (RFC 0003 §6.2).
//!
//! Implements `opentelemetry-proto`'s `LogsService` over the shared
//! `IngestPipeline`: `export` hands the (already tonic-decoded)
//! `ExportLogsServiceRequest` to the WAL-before-ack pipeline and maps the
//! result to a tonic `Status`:
//! - tenant-resolution failure → `INVALID_ARGUMENT` (naming the failing
//!   `ResourceLogs` index + attribute, RFC0003.4/.11);
//! - an oversize payload (`AppendError::TooLarge`, > 16 MiB) →
//!   `RESOURCE_EXHAUSTED` without `RetryInfo` — a permanent client sizing
//!   error, non-retryable (RFC 0061 §3.2);
//! - any other WAL append/sync failure → `UNAVAILABLE` — a transient
//!   failure (the batch was not acked, §3.4), so retryable per the OTLP
//!   failures table (RFC 0018 §3.2);
//! - a panicked ingest task → `INTERNAL` (a genuine, non-retryable bug);
//! - success → an empty `ExportLogsServiceResponse`.
//!
//! `ingest` is async (its fsync is batched by the group-commit
//! coordinator — RFC0008.8 — which offloads the blocking `sync` itself),
//! so the handler simply `.await`s it; the handler never panics.
//!
//! [`logs_service`] builds the served service: gzip accepted, the decoder
//! bounded by `receiver.max_request_bytes`, and [`RequestSizeService`]
//! around it so every over-limit rejection is `RESOURCE_EXHAUSTED`
//! (RFC 0061 §3.2).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::{
    LogsService, LogsServiceServer,
};
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use tonic::server::NamedService;
use tonic::{Code, Request, Response, Status};

use opentelemetry::context::FutureExt as _;

use crate::receiver::pipeline::{Export, IngestFailure, ReceiveError, SharedPipeline};
use crate::receiver::selector;
use ourios_serving::auth::{AuthBinding, AuthError, AuthResolver};
use ourios_serving::propagation::extract_context_from_metadata;

/// The authentication gate for the gRPC listener (RFC 0026 §3.2 /
/// RFC 0029 §3.3), applied as a tower layer on the tonic server. A tower
/// service (unlike a sync tonic interceptor) can await the resolver — an
/// OIDC unseen-`kid` miss refetches the JWKS — while still running
/// **before the message decode** (it sees only the HTTP envelope). A
/// rejection is a trailers-only `UNAUTHENTICATED` (grpc-status 16)
/// response; on success the resolved [`AuthBinding`] rides the request
/// extensions into the handler's tenant-binding check. With nothing
/// configured every request passes through unbound (open mode, §3.1).
#[derive(Clone)]
pub struct AuthLayer {
    resolver: AuthResolver,
    /// Rejection telemetry (RFC 0026 §3.4). The instruments resolve by
    /// name through the global meter, so this instance aggregates with
    /// the pipeline's.
    metrics: Arc<crate::metrics::IngestMetrics>,
}

impl AuthLayer {
    /// A layer over `resolver` (see [`AuthResolver`] for open mode).
    #[must_use]
    pub fn new(resolver: AuthResolver) -> Self {
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
pub struct AuthService<S> {
    inner: S,
    resolver: AuthResolver,
    metrics: Arc<crate::metrics::IngestMetrics>,
}

impl<S, ReqBody, ResBody> tower::Service<http::Request<ReqBody>> for AuthService<S>
where
    S: tower::Service<http::Request<ReqBody>, Response = http::Response<ResBody>>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
    ReqBody: Send + 'static,
    ResBody: Default,
{
    type Response = http::Response<ResBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: http::Request<ReqBody>) -> Self::Future {
        // The tower readiness dance: `poll_ready` reserved capacity on
        // `self.inner`, so that instance (not a fresh clone) must serve
        // this call; the clone waits for its own `poll_ready` next time.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let resolver = self.resolver.clone();
        let metrics = Arc::clone(&self.metrics);
        Box::pin(async move {
            // Clone the HeaderValue (a refcount bump on its Bytes), not
            // an owned String — the resolver only needs an `&str`.
            let authorization = request.headers().get(http::header::AUTHORIZATION).cloned();
            let authorization = authorization.as_ref().and_then(|value| value.to_str().ok());
            match resolver.authenticate(authorization).await {
                Ok(None) => {}
                Ok(Some(binding)) => {
                    // Arc, not the binding itself: the handler clones it
                    // out of extensions to move it across `tokio::spawn`,
                    // and `AuthBinding` deep-clones its tenant sets — the
                    // Arc makes that a refcount bump.
                    request.extensions_mut().insert(Arc::new(binding));
                }
                // One undifferentiated message: missing vs malformed vs
                // unknown would be a probing oracle (RFC 0026 §3.2). §3.4:
                // the rejection counts on `ourios.ingest.batches`
                // (`error.type = unauthenticated`).
                Err(AuthError::Unauthenticated) => {
                    metrics.record_rejected_batch(crate::metrics::ERROR_TYPE_UNAUTHENTICATED);
                    return Ok(
                        Status::unauthenticated("a valid bearer token is required").into_http()
                    );
                }
                // RFC 0047 §3.1: the resolver could not answer — fail
                // closed, `UNAVAILABLE`.
                Err(AuthError::Unavailable) => {
                    metrics.record_rejected_batch(crate::metrics::ERROR_TYPE_UPSTREAM_UNAVAILABLE);
                    return Ok(Status::unavailable(
                        "authorization resolver unavailable; retry later",
                    )
                    .into_http());
                }
            }
            inner.call(request).await
        })
    }
}

/// The served OTLP/gRPC logs service (RFC 0061 §3.2): the
/// [`LogsReceiver`] behind tonic's decoder, which accepts gzip (the
/// Collector's OTLP exporter compresses by default) and bounds each
/// request at `max_request_bytes` on the wire and after decompression,
/// wrapped in [`RequestSizeService`].
#[must_use]
pub fn logs_service(
    pipeline: SharedPipeline,
    max_request_bytes: usize,
) -> RequestSizeService<LogsServiceServer<LogsReceiver>> {
    RequestSizeService::new(
        LogsServiceServer::new(LogsReceiver::new(pipeline))
            .accept_compressed(tonic::codec::CompressionEncoding::Gzip)
            .max_decoding_message_size(max_request_bytes),
    )
}

const GRPC_STATUS: http::HeaderName = http::HeaderName::from_static("grpc-status");

/// RFC 0061 §3.2 — the size edge directly around `LogsServiceServer`.
///
/// tonic rejects a frame whose declared length is over the limit with
/// `OUT_OF_RANGE`, which OTLP lists as retryable, so a client would resend
/// the same oversize batch until its export timeout. This rewrites that
/// status to `RESOURCE_EXHAUSTED`, keeping tonic's `grpc-message`. Sound
/// because [`LogsReceiver`] never returns `OUT_OF_RANGE`: on this service
/// it can only come from the decoder's size check. tonic sends a decode
/// failure trailers-only, so the status is in the response headers.
///
/// It also counts every too-big rejection on `ourios.ingest.batches`
/// (`error.type = request_too_large`, §3.5): the rewritten one, tonic's own
/// `RESOURCE_EXHAUSTED` for a body that inflates past the limit, and the
/// handler's `AppendError::TooLarge`. The service returns
/// `RESOURCE_EXHAUSTED` for nothing else — backpressure is `UNAVAILABLE`
/// (RFC 0053 §3.1) — so the status alone identifies the class.
#[derive(Clone)]
pub struct RequestSizeService<S> {
    inner: S,
    metrics: Arc<crate::metrics::IngestMetrics>,
}

impl<S> RequestSizeService<S> {
    /// Wrap `inner`.
    #[must_use]
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            metrics: Arc::new(crate::metrics::IngestMetrics::new()),
        }
    }
}

impl<S: NamedService> NamedService for RequestSizeService<S> {
    const NAME: &'static str = S::NAME;
}

impl<S, ReqBody, ResBody> tower::Service<http::Request<ReqBody>> for RequestSizeService<S>
where
    S: tower::Service<http::Request<ReqBody>, Response = http::Response<ResBody>>,
    S::Future: Send + 'static,
    ResBody: Send + 'static,
{
    type Response = http::Response<ResBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<ReqBody>) -> Self::Future {
        let response = self.inner.call(request);
        let metrics = Arc::clone(&self.metrics);
        Box::pin(async move {
            let mut response = response.await?;
            mark_too_large(&mut response, &metrics);
            Ok(response)
        })
    }
}

/// Rewrite a size-check `OUT_OF_RANGE` to `RESOURCE_EXHAUSTED` and count
/// every too-big rejection (see [`RequestSizeService`]).
fn mark_too_large<B>(response: &mut http::Response<B>, metrics: &crate::metrics::IngestMetrics) {
    let code = response
        .headers()
        .get(&GRPC_STATUS)
        .map(|value| Code::from_bytes(value.as_bytes()));
    match code {
        Some(Code::OutOfRange) => {
            let exhausted = http::HeaderValue::from(Code::ResourceExhausted as i32);
            response.headers_mut().insert(GRPC_STATUS, exhausted);
            metrics.record_rejected_batch(crate::metrics::ERROR_TYPE_REQUEST_TOO_LARGE);
        }
        Some(Code::ResourceExhausted) => {
            metrics.record_rejected_batch(crate::metrics::ERROR_TYPE_REQUEST_TOO_LARGE);
        }
        _ => {}
    }
}

/// The gRPC `LogsService` over a shared `IngestPipeline`.
pub struct LogsReceiver {
    pipeline: SharedPipeline,
}

impl LogsReceiver {
    /// Build a gRPC receiver over the shared pipeline.
    #[must_use]
    pub fn new(pipeline: SharedPipeline) -> Self {
        Self { pipeline }
    }
}

#[tonic::async_trait]
impl LogsService for LogsReceiver {
    async fn export(
        &self,
        request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        // RFC 0039 §3.3: the `ingest logs` span is born inside `ingest_bound`,
        // past the `tokio::spawn` below — which ambient context does not cross.
        // So the caller's context is extracted here, while the request (and its
        // metadata) is still in hand, and re-attached inside the task where the
        // span is actually created.
        let parent = extract_context_from_metadata(request.metadata());
        // The [`AuthInterceptor`]'s binding, when auth is enabled — the
        // pipeline enforces the §3.2 tenant binding against it.
        let binding = request.extensions().get::<Arc<AuthBinding>>().cloned();
        // RFC 0046 §3.1: the tenant selector — required, exactly once,
        // visible-ASCII metadata — decided before the set check and before
        // any WAL work; a missing/malformed one is INVALID_ARGUMENT.
        let tenant = selector::from_metadata(request.metadata())
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let export = request.into_inner();
        // Run the ingest on its own task so a panic in the pipeline/miner
        // (e.g. an internal `expect` invariant) is contained as a
        // `JoinError` → `INTERNAL` rather than unwinding into tonic and
        // dropping the connection — preserving the handler's no-panic
        // contract. `ingest` is async now (it awaits the batched fsync),
        // so this is `spawn`, not `spawn_blocking`.
        let pipeline = self.pipeline.clone();
        match tokio::spawn(
            async move {
                pipeline
                    .ingest_reported(
                        Export {
                            request: export,
                            tenant,
                            binding: binding.as_deref(),
                            lenient_json: false,
                        },
                        span_error,
                    )
                    .await
            }
            .with_context(parent),
        )
        .await
        {
            Ok(Ok(_)) => Ok(Response::new(ExportLogsServiceResponse::default())),
            Ok(Err(e)) => Err(ingest_error_status(&e)),
            // The ingest task panicked — a genuine, non-retryable internal
            // bug; contain it as INTERNAL.
            Err(join) => Err(Status::internal(format!("ingest task failed: {join}"))),
        }
    }
}

/// Map a settled ingest failure to a tonic `Status` (RFC 0018 §3.2).
///
/// Permanent client errors are non-retryable: a tenant outside the token's
/// set → `PERMISSION_DENIED`; an oversize payload (`AppendError::TooLarge`,
/// over the 16 MiB WAL frame ceiling) → `RESOURCE_EXHAUSTED` without
/// `RetryInfo`, the code the listener's own size limit uses (RFC 0061
/// §3.2) — retrying an oversize batch byte-identical can never succeed.
///
/// Every other WAL append/sync failure is `UNAVAILABLE`, which OTLP defines
/// as retryable — the batch was not acked (§3.4), so compliant clients
/// re-send rather than drop data (a non-retryable `INTERNAL` would tell them
/// to drop it). That includes RFC 0052 §3.3's **terminal** rotation state,
/// which RFC 0018 §3.2's third class reports server-terminal but still
/// client-retryable for exactly that reason; the message is what separates
/// it from a transient blip, and no retry hint rides on either, since the
/// server schedules no retry of its own.
///
/// Adapt the shared [`IngestFailure`] classification to gRPC status
/// vocabulary (the classification itself lives beside `ReceiveError`
/// and is exhaustive there). The message rides along on every arm.
fn ingest_error_status(error: &ReceiveError) -> Status {
    let msg = error.to_string();
    let status = match IngestFailure::classify(error) {
        IngestFailure::Denied => Status::permission_denied(msg),
        IngestFailure::Unauthenticated => Status::unauthenticated(msg),
        // OTLP names RESOURCE_EXHAUSTED for an over-size request; with no
        // RetryInfo it is non-retryable (RFC 0061 §3.2).
        IngestFailure::TooLarge => Status::resource_exhausted(msg),
        // Both durability classes are `UNAVAILABLE`: a non-retryable code
        // would tell the client to drop a batch that was never acked
        // (RFC 0018 §3.2, as RFC 0052 §3.3 amends it). The message, not
        // the code, carries the distinction.
        IngestFailure::Unavailable | IngestFailure::ServerTerminal => Status::unavailable(msg),
        IngestFailure::Internal => Status::internal(msg),
    };
    // A leader's rejection goes back as it gave it, details included
    // (RFC 0065 §6.8).
    match error {
        ReceiveError::Forwarded(rejected) if !rejected.details.is_empty() => {
            let details = rejected.details.clone().into();
            Status::with_details(status.code(), status.message(), details)
        }
        _ => status,
    }
}

/// The `error.type` the `ingest logs` span fails with for `error`: its gRPC
/// code, when that code is one the gRPC server convention counts as an error.
fn span_error(error: &ReceiveError) -> Option<String> {
    server_error_code(ingest_error_status(error).code()).map(str::to_owned)
}

/// `code`'s name if it fails a gRPC **server** span. Semconv
/// (<https://opentelemetry.io/docs/specs/semconv/rpc/grpc/#server>) lists
/// exactly `UNKNOWN`, `DEADLINE_EXCEEDED`, `UNIMPLEMENTED`, `INTERNAL`, `UNAVAILABLE`
/// and `DATA_LOSS`; every other code, `PERMISSION_DENIED` and `INVALID_ARGUMENT`
/// included, leaves the span unset.
fn server_error_code(code: Code) -> Option<&'static str> {
    match code {
        Code::Unknown => Some("UNKNOWN"),
        Code::DeadlineExceeded => Some("DEADLINE_EXCEEDED"),
        Code::Unimplemented => Some("UNIMPLEMENTED"),
        Code::Internal => Some("INTERNAL"),
        Code::Unavailable => Some("UNAVAILABLE"),
        Code::DataLoss => Some("DATA_LOSS"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{IngestFailure, ReceiveError, ingest_error_status, server_error_code};
    use ourios_wal::{AppendError, RotationFault, RotationSite, SyncError};
    use tonic::Code;

    /// A forwarded rejection reaches the gRPC client as its leader gave it:
    /// code, message and status details.
    #[test]
    fn a_forwarded_rejection_keeps_its_code_message_and_details() {
        use crate::replicated::raft::transport::{Rejected, Rejection};
        let details = vec![0x08, 0x07, 0x12, 0x02, b'n', b'o'];
        let error = ReceiveError::Forwarded(Rejected {
            rejection: Rejection::PermissionDenied,
            error_type: "permission_denied".to_owned(),
            message: "the leader's words".to_owned(),
            details: details.clone(),
        });
        let status = ingest_error_status(&error);
        assert_eq!(status.code(), Code::PermissionDenied);
        assert_eq!(status.message(), "the leader's words");
        assert_eq!(status.details(), details.as_slice());
    }

    fn fault(attempts: u32) -> RotationFault {
        RotationFault::new(
            RotationSite::ParentFsync,
            &std::io::Error::other("io"),
            attempts,
            3,
        )
    }

    #[test]
    fn tenant_denied_is_permission_denied() {
        let e = ReceiveError::TenantDenied {
            token_name: "edge".to_string(),
            tenant: ourios_core::tenant::TenantId::new("intruder"),
        };
        assert_eq!(ingest_error_status(&e).code(), Code::PermissionDenied);
    }

    #[test]
    fn oversize_payload_is_invalid_argument() {
        let e = ReceiveError::WalAppend(AppendError::TooLarge {
            len: 32 * 1024 * 1024,
            limit: 16 * 1024 * 1024,
        });
        assert_eq!(ingest_error_status(&e).code(), Code::ResourceExhausted);
    }

    /// Scenario RFC0061.3 — a batch over the WAL frame maps the same way
    /// as an over-limit request: `RESOURCE_EXHAUSTED` without `RetryInfo`.
    /// The HTTP arm (`413`) is `receiver/http.rs`'s
    /// `oversize_payload_is_413`; the `error.type` arm is in
    /// `tests/rfc0061_telemetry.rs`.
    /// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
    #[test]
    fn rfc0061_3_too_large_is_resource_exhausted_without_retry_info() {
        use tonic_types::StatusExt as _;

        let e = ReceiveError::WalAppend(AppendError::TooLarge {
            len: 32 * 1024 * 1024,
            limit: 16 * 1024 * 1024,
        });
        let status = ingest_error_status(&e);
        assert_eq!(status.code(), Code::ResourceExhausted);
        assert!(status.get_details_retry_info().is_none());
    }

    #[test]
    fn transient_wal_append_is_unavailable() {
        let e = ReceiveError::WalAppend(AppendError::Io {
            op: "write",
            source: std::io::Error::other("io"),
        });
        assert_eq!(ingest_error_status(&e).code(), Code::Unavailable);
    }

    /// RFC 0052 §3.3 replaced the permanent quiesce with a bounded
    /// retry, so this arm now covers both of its states. Neither may
    /// become a non-retryable code: the batch was never acked.
    #[test]
    fn both_rotation_states_are_unavailable() {
        for error in [
            AppendError::RotationRetrying(fault(1)),
            AppendError::RotationTerminal(fault(3)),
        ] {
            let e = ReceiveError::WalAppend(error);
            assert_eq!(ingest_error_status(&e).code(), Code::Unavailable);
        }
    }

    #[test]
    fn transient_wal_sync_is_unavailable() {
        let e = ReceiveError::WalSync(SyncError::Io {
            op: "fdatasync",
            source: std::io::Error::other("io"),
        });
        assert_eq!(ingest_error_status(&e).code(), Code::Unavailable);
    }

    /// The `ingest logs` span fails exactly when the gRPC status is one the
    /// semconv gRPC server convention counts as an error (`UNKNOWN`,
    /// `DEADLINE_EXCEEDED`, `UNIMPLEMENTED`, `INTERNAL`, `UNAVAILABLE`, `DATA_LOSS`);
    /// `PERMISSION_DENIED` and `INVALID_ARGUMENT` leave it unset.
    #[test]
    fn the_batch_span_fails_exactly_on_a_grpc_server_error_code() {
        let errors = [
            ReceiveError::TenantDenied {
                token_name: "edge".to_string(),
                tenant: ourios_core::tenant::TenantId::new("intruder"),
            },
            ReceiveError::WalAppend(AppendError::TooLarge { len: 2, limit: 1 }),
            ReceiveError::WalAppend(AppendError::RotationTerminal(fault(3))),
            ReceiveError::WalSync(SyncError::Io {
                op: "fdatasync",
                source: std::io::Error::other("io"),
            }),
        ];
        for e in errors {
            let server_error = matches!(
                ingest_error_status(&e).code(),
                Code::Unknown
                    | Code::DeadlineExceeded
                    | Code::Unimplemented
                    | Code::Internal
                    | Code::Unavailable
                    | Code::DataLoss
            );
            let span_error = IngestFailure::classify(&e).span_error_type().is_some();
            assert_eq!(span_error, server_error, "{e}");
            assert_eq!(super::span_error(&e).is_some(), server_error, "{e}");
        }
    }

    /// An `ingest logs`-shaped server span marked from `code` as the gRPC
    /// arm marks it, exported.
    fn server_span_for(code: Code) -> opentelemetry_sdk::trace::SpanData {
        use tracing_subscriber::prelude::*;

        let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer().with_tracer(ourios_telemetry::test_tracer(&provider)),
        );
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "ingest logs",
                otel.kind = "server",
                otel.status_code = tracing::field::Empty,
                error.type = tracing::field::Empty,
            );
            if let Some(error_type) = server_error_code(code) {
                ourios_core::telemetry::record_span_error(&span, error_type);
            }
        });
        let mut spans = exporter.get_finished_spans().expect("spans");
        spans.pop().expect("one span")
    }

    #[test]
    fn grpc_internal_fails_the_server_span_and_permission_denied_does_not() {
        let internal = server_span_for(Code::Internal);
        assert!(
            matches!(internal.status, opentelemetry::trace::Status::Error { .. }),
            "{:?}",
            internal.status
        );
        let error_type = internal
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "error.type")
            .map(|kv| kv.value.as_str().into_owned());
        assert_eq!(error_type.as_deref(), Some("INTERNAL"));

        let denied = server_span_for(Code::PermissionDenied);
        assert_eq!(denied.status, opentelemetry::trace::Status::Unset);
        assert!(
            denied
                .attributes
                .iter()
                .all(|kv| kv.key.as_str() != "error.type")
        );
    }

    #[test]
    fn only_the_listed_grpc_codes_are_server_errors() {
        let listed = [
            Code::Unknown,
            Code::DeadlineExceeded,
            Code::Unimplemented,
            Code::Internal,
            Code::Unavailable,
            Code::DataLoss,
        ];
        for value in 0..=16 {
            let code = Code::from_i32(value);
            assert_eq!(
                server_error_code(code).is_some(),
                listed.contains(&code),
                "{code:?}"
            );
        }
    }
}
