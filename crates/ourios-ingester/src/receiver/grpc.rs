//! OTLP/gRPC listener (RFC 0003 §6.2).
//!
//! Implements `opentelemetry-proto`'s `LogsService` over the shared
//! `IngestPipeline`: `export` hands the (already tonic-decoded)
//! `ExportLogsServiceRequest` to the WAL-before-ack pipeline and maps the
//! result to a tonic `Status`:
//! - tenant-resolution failure → `INVALID_ARGUMENT` (naming the failing
//!   `ResourceLogs` index + attribute, RFC0003.4/.11);
//! - WAL failure → `INTERNAL` (the batch was not acked, §3.4);
//! - success → an empty `ExportLogsServiceResponse`.
//!
//! Like the HTTP handler, the blocking `ingest` (WAL append + fsync) runs
//! via `spawn_blocking` so it doesn't stall the async runtime, and a
//! poisoned lock is recovered (the handler never panics).

use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsService;
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use tonic::{Request, Response, Status};

use crate::receiver::pipeline::{ReceiveError, SharedPipeline};

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
        let export = request.into_inner();
        let pipeline = self.pipeline.clone();
        // Offload the blocking WAL append + fsync (mirrors the HTTP path).
        let outcome = tokio::task::spawn_blocking(move || {
            pipeline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ingest(export)
        })
        .await;

        match outcome {
            Ok(Ok(_)) => Ok(Response::new(ExportLogsServiceResponse::default())),
            // A Resource that doesn't resolve to a tenant is a client
            // error; the whole batch is rejected (RFC0003.4). The error's
            // Display names the failing ResourceLogs index + attribute.
            Ok(Err(ReceiveError::TenantResolution(e))) => {
                Err(Status::invalid_argument(e.to_string()))
            }
            // A WAL append/sync failure — server-side; the batch was not
            // acked (§3.4). Surface the (Display-able) detail.
            Ok(Err(e)) => Err(Status::internal(e.to_string())),
            // The blocking ingest task panicked (a `spawn_blocking` task
            // can't be cancelled); `JoinError`'s Display is short + safe.
            Err(join) => Err(Status::internal(format!("ingest task failed: {join}"))),
        }
    }
}
