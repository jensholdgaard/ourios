//! An export's ingest on the replicated backend (RFC 0065 §6.3): the
//! local path's binding check, frame and metrics around a group commit
//! instead of a local WAL fsync. The leader never mines (B23); its
//! acknowledged entries reach a miner through the group's log.

use std::time::Instant;

use ourios_core::telemetry::record_span_error;
use ourios_core::tenant::TenantId;
use tracing::Instrument as _;

use super::raft::types::TenantBatchBytes;
use super::write::{GroupPosition, GroupWriter, WriteError};
use crate::receiver::admission::{Admission, Export, Framed};
use crate::receiver::pipeline::{ReceiveError, log_record_count, span_error};

/// What an acknowledged export did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ingested {
    /// No records: success with nothing written (RFC0003.12).
    Empty,
    /// `records` committed at `position`.
    Committed {
        records: usize,
        position: GroupPosition,
    },
}

pub struct ReplicatedIngest {
    admission: Admission,
    writer: GroupWriter,
}

impl ReplicatedIngest {
    #[must_use]
    pub fn new(writer: GroupWriter) -> Self {
        Self {
            admission: Admission::new(),
            writer,
        }
    }

    /// Install the RFC 0026 §3.4 denial audit sink.
    #[must_use]
    pub fn with_denial_audit_sink(
        self,
        sink: Box<dyn ourios_core::audit::AuditSink + Send>,
    ) -> Self {
        self.admission.set_denial_audit_sink(sink);
        self
    }

    /// Ingest `export`: check its binding, frame it, and commit it to its
    /// tenant's group. `Ok` only once the group has committed it (§3.4).
    ///
    /// # Errors
    ///
    /// [`ReceiveError::TenantDenied`] before any write;
    /// [`ReceiveError::Replication`] when the group did not commit it,
    /// counted on `ourios.ingest.batches` under its `error.type`; a size
    /// rejection is counted by the transport, from its response.
    // The local path's `ingest logs` span and its `commit wal` child
    // (RFC 0038 §3.5); here the commit is the group's.
    #[tracing::instrument(
        skip_all,
        name = "ingest logs",
        fields(
            otel.kind = "server",
            ourios.tenant = export.tenant.as_str(),
            ourios.ingest.batch.record_count = log_record_count(&export.request),
            ourios.ingest.batch.size = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        )
    )]
    pub async fn ingest(&self, export: Export<'_>) -> Result<Ingested, ReceiveError> {
        let outcome = self.ingest_batch(export).await;
        if let Err(error) = &outcome
            && let Some(error_type) = span_error(error)
        {
            record_span_error(&tracing::Span::current(), &error_type);
        }
        outcome
    }

    /// [`Self::ingest`]'s work, inside its span.
    async fn ingest_batch(&self, export: Export<'_>) -> Result<Ingested, ReceiveError> {
        let Export {
            request,
            tenant,
            binding,
            lenient_json,
        } = export;
        self.admission.enforce_binding(&tenant, binding)?;
        let mut framed = Framed::of(request, &tenant)?;
        if framed.records.is_empty() {
            return Ok(Ingested::Empty);
        }
        let batch = TenantBatchBytes::new(std::mem::take(&mut framed.payload))
            .map_err(ReceiveError::TenantFrame)?;
        let started = Instant::now();
        match self.commit_traced(&tenant, batch).await {
            Ok(position) => {
                let accepted = framed.accepted(&tenant, lenient_json, started.elapsed());
                self.admission.metrics().record_batch(&accepted);
                Ok(Ingested::Committed {
                    records: framed.records.len(),
                    position,
                })
            }
            // The transport counts a size rejection from its response, as
            // it does the local WAL's.
            Err(e @ WriteError::TooLarge { .. }) => Err(ReceiveError::Replication(e)),
            Err(e) => {
                self.admission
                    .metrics()
                    .record_rejected_batch(e.error_type());
                Err(ReceiveError::Replication(e))
            }
        }
    }

    /// The group commit as the export's `commit wal` child span. An
    /// internal span, so any failure marks it, with the rejection's
    /// `error.type`.
    async fn commit_traced(
        &self,
        tenant: &TenantId,
        batch: TenantBatchBytes,
    ) -> Result<GroupPosition, WriteError> {
        let span = tracing::info_span!(
            "commit wal",
            otel.kind = "internal",
            otel.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        );
        let outcome = self
            .writer
            .commit(tenant.as_str(), batch)
            .instrument(span.clone())
            .await;
        if let Err(e) = &outcome {
            record_span_error(&span, e.error_type());
        }
        outcome
    }
}
