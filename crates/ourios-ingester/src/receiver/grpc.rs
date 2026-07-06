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

use std::sync::Arc;

use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsService;
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use ourios_core::auth::TokenStore;
use tonic::{Request, Response, Status};

use crate::receiver::auth::{AuthBinding, authenticate_bearer};
use crate::receiver::pipeline::{ReceiveError, SharedPipeline};

/// The RFC 0026 §3.2 authentication gate for the gRPC listener, installed
/// via `LogsServiceServer::with_interceptor`. It runs **before the message
/// decode** (an interceptor sees only metadata), rejecting a missing or
/// unknown bearer with `UNAUTHENTICATED`; on success it attaches the
/// resolved [`AuthBinding`] as a request extension for the handler's
/// tenant-binding check. With no store configured it passes every request
/// through unbound (open mode, §3.1) — one service type either way.
#[derive(Clone)]
pub struct AuthInterceptor {
    store: Option<Arc<TokenStore>>,
}

impl AuthInterceptor {
    /// An interceptor over `store` (`None` = open mode pass-through).
    #[must_use]
    pub fn new(store: Option<Arc<TokenStore>>) -> Self {
        Self { store }
    }
}

impl tonic::service::Interceptor for AuthInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let authorization = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok());
        match authenticate_bearer(self.store.as_deref(), authorization) {
            Ok(None) => Ok(request),
            Ok(Some(binding)) => {
                request.extensions_mut().insert(binding);
                Ok(request)
            }
            // One undifferentiated message: missing vs malformed vs unknown
            // would be a probing oracle (RFC 0026 §3.2).
            Err(_) => Err(Status::unauthenticated("missing or unknown bearer token")),
        }
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
        match tokio::spawn(async move { pipeline.ingest_bound(export, binding.as_ref()).await })
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
