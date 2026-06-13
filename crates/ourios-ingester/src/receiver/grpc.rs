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
//! `ingest` is async (its fsync is batched by the group-commit
//! coordinator — RFC0008.8 — which offloads the blocking `sync` itself),
//! so the handler simply `.await`s it; the handler never panics.

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
        // Run the ingest on its own task so a panic in the pipeline/miner
        // (e.g. an internal `expect` invariant) is contained as a
        // `JoinError` → `INTERNAL` rather than unwinding into tonic and
        // dropping the connection — preserving the handler's no-panic
        // contract. `ingest` is async now (it awaits the batched fsync),
        // so this is `spawn`, not `spawn_blocking`.
        let pipeline = self.pipeline.clone();
        match tokio::spawn(async move { pipeline.ingest(export).await }).await {
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
            // The ingest task panicked; contain it as INTERNAL.
            Err(join) => Err(Status::internal(format!("ingest task failed: {join}"))),
        }
    }
}
