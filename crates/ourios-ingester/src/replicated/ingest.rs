//! An export's ingest on the replicated backend (RFC 0065 §6.3): the
//! local path's binding check, frame and metrics around a group commit
//! instead of a local WAL fsync. The leader never mines (B23); its
//! acknowledged entries reach a miner through the group's log.
//!
//! A node that does not lead the export's group forwards it to the leader
//! with the client's credential (§6.8). Its outcome is counted here, on
//! the node the client sent it to, except a rejection the leader gave,
//! which the leader counted and this node returns as it was given.

use std::time::Instant;

use ourios_core::telemetry::record_span_error;
use ourios_core::tenant::TenantId;
use tracing::Instrument as _;

use super::raft::transport::{Forwarded, Rejected};
use super::raft::types::TenantBatchBytes;
use super::route::{Routed, Router};
use super::write::{GroupPosition, GroupWriter, Leading, WriteError};
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
    /// Where an export for a group this node does not lead goes; without
    /// one it gets the retryable rejection.
    router: Option<Router>,
}

/// Why an export's commit, local or forwarded, did not happen.
enum Uncommitted {
    Write(WriteError),
    /// The leader refused the forwarded export before appending it.
    Rejected(Rejected),
}

impl Uncommitted {
    fn error_type(&self) -> &str {
        match self {
            Self::Write(e) => e.error_type(),
            Self::Rejected(rejected) => &rejected.error_type,
        }
    }
}

impl ReplicatedIngest {
    #[must_use]
    pub fn new(writer: GroupWriter) -> Self {
        Self {
            admission: Admission::new(),
            writer,
            router: None,
        }
    }

    /// Forward exports for groups this node does not lead through `router`.
    #[must_use]
    pub fn with_router(self, router: Router) -> Self {
        Self {
            router: Some(router),
            ..self
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

    /// [`Self::ingest_with`] for a client that presented no credential.
    ///
    /// # Errors
    ///
    /// As [`Self::ingest_with`].
    pub async fn ingest(&self, export: Export<'_>) -> Result<Ingested, ReceiveError> {
        self.ingest_with(export, None).await
    }

    /// Ingest `export`: check its binding, frame it, and commit it to its
    /// tenant's group, here or, forwarded with the client's `credential`,
    /// on the group's leader. `Ok` only once the group has committed it
    /// (§3.4).
    ///
    /// # Errors
    ///
    /// [`ReceiveError::TenantDenied`] before any write;
    /// [`ReceiveError::Replication`] when the group did not commit it,
    /// counted on `ourios.ingest.batches` under its `error.type`; a size
    /// rejection is counted by the transport, from its response;
    /// [`ReceiveError::Forwarded`] when the leader refused it, already
    /// counted there.
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
    pub async fn ingest_with(
        &self,
        export: Export<'_>,
        credential: Option<&str>,
    ) -> Result<Ingested, ReceiveError> {
        let outcome = self.ingest_batch(export, credential).await;
        if let Err(error) = &outcome
            && let Some(error_type) = span_error(error)
        {
            record_span_error(&tracing::Span::current(), &error_type);
        }
        outcome
    }

    /// [`Self::ingest_with`]'s work, inside its span.
    async fn ingest_batch(
        &self,
        export: Export<'_>,
        credential: Option<&str>,
    ) -> Result<Ingested, ReceiveError> {
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
        match self.commit_traced(&tenant, batch, credential).await {
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
            Err(Uncommitted::Write(e @ WriteError::TooLarge { .. })) => {
                Err(ReceiveError::Replication(e))
            }
            Err(Uncommitted::Write(e)) => {
                self.admission
                    .metrics()
                    .record_rejected_batch(e.error_type());
                Err(ReceiveError::Replication(e))
            }
            Err(Uncommitted::Rejected(rejected)) => Err(ReceiveError::Forwarded(rejected)),
        }
    }

    /// The group commit, here or forwarded, as the export's `commit wal`
    /// child span. An internal span, so any failure marks it, with the
    /// rejection's `error.type`.
    async fn commit_traced(
        &self,
        tenant: &TenantId,
        batch: TenantBatchBytes,
        credential: Option<&str>,
    ) -> Result<GroupPosition, Uncommitted> {
        let span = tracing::info_span!(
            "commit wal",
            otel.kind = "internal",
            otel.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        );
        let outcome = self
            .commit(tenant, batch, credential)
            .instrument(span.clone())
            .await;
        if let Err(e) = &outcome {
            record_span_error(&span, e.error_type());
        }
        outcome
    }

    /// Commit here if this node leads the group, else forward to its
    /// leader. The size check comes first, so an export too large for a
    /// WAL frame is the permanent rejection wherever its leader is.
    /// Leadership is read before the commit, which takes the batch: should
    /// it change in between, the commit's not-leader answer is the
    /// retryable rejection.
    async fn commit(
        &self,
        tenant: &TenantId,
        batch: TenantBatchBytes,
        credential: Option<&str>,
    ) -> Result<GroupPosition, Uncommitted> {
        GroupWriter::fits(&batch).map_err(Uncommitted::Write)?;
        let (Leading::Elsewhere { group, leader }, Some(router)) =
            (self.writer.leading(tenant.as_str()), &self.router)
        else {
            let committed = self.writer.commit(tenant.as_str(), batch).await;
            return committed.map_err(Uncommitted::Write);
        };
        let export = Forwarded {
            group,
            batch,
            credential: credential.map(str::to_owned),
        };
        match router.forward(export, leader).await {
            Routed::Committed(seq) => Ok(GroupPosition { group, seq }),
            Routed::Rejected(rejected) => Err(Uncommitted::Rejected(rejected)),
            Routed::Unavailable(reason) => Err(Uncommitted::Write(WriteError::Unforwarded {
                group,
                reason,
            })),
        }
    }
}
