//! An export's ingest on the replicated backend (RFC 0065 §6.3): the
//! local path's binding check, frame and metrics around a group commit
//! instead of a local WAL fsync. The leader never mines (B23); its
//! acknowledged entries reach a miner through the group's log.

use std::time::Instant;

use super::raft::types::TenantBatchBytes;
use super::write::{GroupPosition, GroupWriter, WriteError};
use crate::receiver::admission::{Admission, Export, Framed};
use crate::receiver::pipeline::ReceiveError;

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
    /// [`ReceiveError::Replication`], counted on `ourios.ingest.batches`
    /// under its `error.type`, when the group did not commit it.
    pub async fn ingest(&self, export: Export<'_>) -> Result<Ingested, ReceiveError> {
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
        match self.writer.commit(tenant.as_str(), batch).await {
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
}
