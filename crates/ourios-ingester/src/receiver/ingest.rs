//! What the OTLP transports hand an export to: the local WAL's
//! [`IngestPipeline`], or a replicated group (RFC 0065 §6.8), which may
//! forward the export to the group's leader with the client's credential.

use std::sync::Arc;

use super::pipeline::{Export, IngestPipeline, ReceiveError, SpanError};

/// Ingests one export for a transport, acknowledging only what is durable.
#[async_trait::async_trait]
pub trait Ingest: Send + Sync + 'static {
    /// Ingest `export`, whose client presented `credential` (its
    /// `Authorization` value, if any), marking the `ingest logs` span
    /// failed as `span_error` says for this transport. `Ok` with the
    /// records written once they are durable.
    ///
    /// # Errors
    ///
    /// The [`ReceiveError`] the transport maps to its response.
    async fn ingest_reported(
        &self,
        export: Export<'_>,
        credential: Option<&str>,
        span_error: SpanError,
    ) -> Result<usize, ReceiveError>;
}

/// An [`Ingest`] the transports share.
pub type SharedIngest = Arc<dyn Ingest>;

#[async_trait::async_trait]
impl Ingest for IngestPipeline {
    /// The local WAL forwards nothing, so the credential is not needed.
    async fn ingest_reported(
        &self,
        export: Export<'_>,
        _credential: Option<&str>,
        span_error: SpanError,
    ) -> Result<usize, ReceiveError> {
        IngestPipeline::ingest_reported(self, export, span_error).await
    }
}

#[cfg(test)]
mod tests;
