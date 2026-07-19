//! OTLP/gRPC listener (RFC 0003 §6.2).
//!
//! Implements `opentelemetry-proto`'s `LogsService` over the shared
//! `IngestPipeline`: `export` hands the (already tonic-decoded)
//! `ExportLogsServiceRequest` to the WAL-before-ack pipeline and maps the
//! result to a tonic `Status`:
//! - tenant-resolution failure → `INVALID_ARGUMENT` (naming the failing
//!   `ResourceLogs` index + attribute, RFC0003.4/.11);
//! - an oversize payload (`AppendError::TooLarge`, > 16 MiB) →
//!   `INVALID_ARGUMENT` — a permanent client sizing error, non-retryable;
//! - any other WAL append/sync failure → `UNAVAILABLE` — a transient
//!   failure (the batch was not acked, §3.4), so retryable per the OTLP
//!   failures table (RFC 0018 §3.2);
//! - a panicked ingest task → `INTERNAL` (a genuine, non-retryable bug);
//! - success → an empty `ExportLogsServiceResponse`.
//!
//! `ingest` is async (its fsync is batched by the group-commit
//! coordinator — RFC0008.8 — which offloads the blocking `sync` itself),
//! so the handler simply `.await`s it; the handler never panics.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsService;
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use tonic::{Request, Response, Status};

use crate::receiver::auth::{AuthBinding, AuthResolver};
use crate::receiver::pipeline::{ReceiveError, SharedPipeline};

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
            let authorization = request
                .headers()
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            match resolver.authenticate(authorization.as_deref()).await {
                Ok(None) => {}
                Ok(Some(binding)) => {
                    request.extensions_mut().insert(binding);
                }
                // One undifferentiated message: missing vs malformed vs
                // unknown would be a probing oracle (RFC 0026 §3.2). §3.4:
                // the rejection counts on `ourios.ingest.batches`
                // (`error.type = unauthenticated`).
                Err(_) => {
                    metrics.record_rejected_batch(crate::metrics::ERROR_TYPE_UNAUTHENTICATED);
                    return Ok(
                        Status::unauthenticated("a valid bearer token is required").into_http()
                    );
                }
            }
            inner.call(request).await
        })
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
        // The [`AuthInterceptor`]'s binding, when auth is enabled — the
        // pipeline enforces the §3.2 tenant binding against it.
        let binding = request.extensions().get::<AuthBinding>().cloned();
        let export = request.into_inner();
        // Run the ingest on its own task so a panic in the pipeline/miner
        // (e.g. an internal `expect` invariant) is contained as a
        // `JoinError` → `INTERNAL` rather than unwinding into tonic and
        // dropping the connection — preserving the handler's no-panic
        // contract. `ingest` is async now (it awaits the batched fsync),
        // so this is `spawn`, not `spawn_blocking`.
        let pipeline = self.pipeline.clone();
        match tokio::spawn(
            async move { pipeline.ingest_bound(export, binding.as_ref(), false).await },
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
/// Permanent client errors are non-retryable: tenant-resolution failure and
/// an oversize payload (`AppendError::TooLarge`, over the 16 MiB WAL frame
/// ceiling) both → `INVALID_ARGUMENT` — retrying an oversize batch
/// byte-identical can never succeed. Any other WAL append/sync failure is
/// *transient* (the batch was not acked, §3.4) → retryable `UNAVAILABLE`, so
/// compliant clients re-send rather than drop data (a non-retryable
/// `INTERNAL` would tell them to drop it).
///
/// Matched per-variant (exhaustive over `ReceiveError` in-crate): a future
/// `#[non_exhaustive]` variant breaks the build here, forcing a
/// retryable-vs-not decision rather than defaulting either way.
fn ingest_error_status(error: &ReceiveError) -> Status {
    match error {
        // `TenantResolutionError`'s Display names the failing ResourceLogs
        // index + attribute.
        ReceiveError::TenantResolution(e) => Status::invalid_argument(e.to_string()),
        // An authenticated caller writing outside its tenant set — a
        // permanent authz rejection, whole batch, pre-WAL (RFC 0026 §3.2).
        e @ ReceiveError::TenantDenied { .. } => Status::permission_denied(e.to_string()),
        e @ ReceiveError::WalAppend(ourios_wal::AppendError::TooLarge { .. }) => {
            Status::invalid_argument(e.to_string())
        }
        e @ (ReceiveError::WalAppend(_) | ReceiveError::WalSync(_)) => {
            Status::unavailable(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ReceiveError, ingest_error_status};
    use crate::receiver::tenant::TenantResolutionError;
    use ourios_wal::{AppendError, SyncError};
    use tonic::Code;

    #[test]
    fn tenant_resolution_is_invalid_argument() {
        let e = ReceiveError::TenantResolution(TenantResolutionError::for_test("service.name"));
        assert_eq!(ingest_error_status(&e).code(), Code::InvalidArgument);
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
        assert_eq!(ingest_error_status(&e).code(), Code::InvalidArgument);
    }

    #[test]
    fn transient_wal_append_is_unavailable() {
        let e = ReceiveError::WalAppend(AppendError::Io {
            op: "write",
            source: std::io::Error::other("io"),
        });
        assert_eq!(ingest_error_status(&e).code(), Code::Unavailable);
    }

    #[test]
    fn quiesced_wal_append_is_unavailable() {
        let e = ReceiveError::WalAppend(AppendError::QuiescedAfterRotationFailure);
        assert_eq!(ingest_error_status(&e).code(), Code::Unavailable);
    }

    #[test]
    fn transient_wal_sync_is_unavailable() {
        let e = ReceiveError::WalSync(SyncError::Io {
            op: "fdatasync",
            source: std::io::Error::other("io"),
        });
        assert_eq!(ingest_error_status(&e).code(), Code::Unavailable);
    }
}
