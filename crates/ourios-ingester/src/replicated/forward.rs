//! The leader's side of `Forward` (RFC 0065 §6.8). A forwarded export is
//! authenticated and checked against its tenant binding as if its client
//! had sent it here, then committed to its group by this node and never
//! forwarded on: a node that does not lead the group says so, with the
//! leader it knows of.
//!
//! Counting on `ourios.ingest.batches`: a rejection before appending is
//! decided here, so it is counted here and passed back for the sender to
//! return uncounted. The size rejection is the exception: as on the
//! direct path, the transport counts it from its response, on the node
//! the client sent the export to. A commit that failed after the entry may have been
//! appended is not counted here. Its sender cannot tell that answer from a
//! lost one, so the sender counts every forward that ends without a
//! committed position, as `group_unavailable` (§6.3).

use std::sync::Arc;

use ourios_core::tenant::TenantId;
use ourios_serving::{AuthError, AuthResolver};

use super::raft::transport::{ForwardTarget, Forwarded, Refusal, Rejected, Rejection};
use super::write::{GroupWriter, WriteError};
use crate::metrics::{
    ERROR_TYPE_PERMISSION_DENIED, ERROR_TYPE_REQUEST_TOO_LARGE, ERROR_TYPE_UNAUTHENTICATED,
    ERROR_TYPE_UPSTREAM_UNAVAILABLE,
};
use crate::receiver::admission::Admission;

/// Commits the exports other nodes forward to this one.
pub struct LeaderTarget(Arc<Inner>);

struct Inner {
    writer: GroupWriter,
    resolver: AuthResolver,
    admission: Admission,
}

impl LeaderTarget {
    /// Commit through `writer`, authenticating each export's credential
    /// with `resolver`, the receiver's own.
    #[must_use]
    pub fn new(writer: GroupWriter, resolver: AuthResolver) -> Self {
        Self(Arc::new(Inner {
            writer,
            resolver,
            admission: Admission::new(),
        }))
    }

    /// Install the RFC 0026 §3.4 denial audit sink.
    #[must_use]
    pub fn with_denial_audit_sink(
        self,
        sink: Box<dyn ourios_core::audit::AuditSink + Send>,
    ) -> Self {
        self.0.admission.set_denial_audit_sink(sink);
        self
    }
}

impl ForwardTarget for LeaderTarget {
    fn commit(
        &self,
        export: Forwarded,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<u64, Refusal>> + Send>> {
        let inner = Arc::clone(&self.0);
        Box::pin(async move { inner.commit(export).await })
    }
}

impl Inner {
    async fn commit(&self, export: Forwarded) -> Result<u64, Refusal> {
        let Forwarded {
            group,
            batch,
            credential,
        } = export;
        let binding = match self.resolver.authenticate(credential.as_deref()).await {
            Ok(binding) => binding,
            Err(AuthError::Unauthenticated) => {
                return Err(self.reject(
                    Rejection::Unauthenticated,
                    ERROR_TYPE_UNAUTHENTICATED,
                    "a valid bearer token is required".to_owned(),
                ));
            }
            Err(AuthError::Unavailable) => {
                return Err(self.reject(
                    Rejection::Unavailable,
                    ERROR_TYPE_UPSTREAM_UNAVAILABLE,
                    "authorization resolver unavailable; retry later".to_owned(),
                ));
            }
        };
        let tenant = match ourios_wal::TenantBatch::decode(batch.as_bytes()) {
            Ok(frame) => TenantId::new(frame.tenant),
            Err(e) => return Err(Refusal::Failed(format!("a forwarded export: {e}"))),
        };
        if let Err(denied) = self.admission.enforce_binding(&tenant, binding.as_ref()) {
            return Err(rejected(
                Rejection::PermissionDenied,
                ERROR_TYPE_PERMISSION_DENIED,
                denied.to_string(),
            ));
        }
        if self.writer.group_of(tenant.as_str()) != group {
            return Err(Refusal::NotHosted);
        }
        match self.writer.commit(tenant.as_str(), batch).await {
            Ok(position) => Ok(position.seq),
            Err(WriteError::NotLeader { leader, .. }) => Err(Refusal::NotLeader { leader }),
            Err(WriteError::NotHosted { .. }) => Err(Refusal::NotHosted),
            Err(e @ WriteError::TooLarge { .. }) => Err(rejected(
                Rejection::TooLarge,
                ERROR_TYPE_REQUEST_TOO_LARGE,
                e.to_string(),
            )),
            Err(e) => Err(Refusal::Failed(e.to_string())),
        }
    }

    /// `rejection`, counted here under `error_type`.
    fn reject(&self, rejection: Rejection, error_type: &'static str, message: String) -> Refusal {
        self.admission.metrics().record_rejected_batch(error_type);
        rejected(rejection, error_type, message)
    }
}

fn rejected(rejection: Rejection, error_type: &str, message: String) -> Refusal {
    Refusal::Rejected(Rejected {
        rejection,
        error_type: error_type.to_owned(),
        message,
        details: Vec::new(),
    })
}

#[cfg(test)]
mod tests;
