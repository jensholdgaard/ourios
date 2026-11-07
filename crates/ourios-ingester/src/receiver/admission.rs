//! What every ingest path does around its commit, local or replicated:
//! the RFC 0026 §3.2 tenant binding with its §3.4 denial surface, the
//! `TenantOtlpBatch` frame and the records it carries (RFC 0046 §3.2,
//! §3.3), and the accepted-batch metrics.

use std::sync::Mutex;
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_core::audit::{AuditEvent, AuditPayload, AuditSink};
use ourios_core::otlp::OtlpLogRecord;
use ourios_core::sync::lock_recover;
use ourios_core::tenant::TenantId;
use ourios_serving::AuthBinding;
use ourios_wal::TenantBatch;
use prost::Message as _;

use super::error::{ReceiveError, check_binding};
use super::materialize::severity_is_out_of_range;
use super::tenant::assign;
use crate::metrics::{AcceptedBatch, ERROR_TYPE_PERMISSION_DENIED, IngestMetrics};

/// One decoded export, as a transport hands it to an ingest path.
pub struct Export<'a> {
    pub request: ExportLogsServiceRequest,
    /// The out-of-band tenant (RFC 0046 §3.1).
    pub tenant: TenantId,
    /// The caller's RFC 0026 §3.2 binding; `None` is open mode.
    pub binding: Option<&'a AuthBinding>,
    /// Whether the OTLP/JSON payload only parsed via the lenient retry.
    pub lenient_json: bool,
}

pub(crate) struct Admission {
    /// Ingest throughput + WAL-before-ack latency instruments (RFC 0014
    /// §6.3), recorded on durably-acked batches — plus the rejection
    /// counts (`error.type` on the batches counter).
    metrics: IngestMetrics,
    /// RFC 0026 §3.4: the sink for `ingest_denied` audit events. Behind a
    /// mutex — denials are the cold path.
    ///
    /// **Best-effort durability, deliberately.** The server wires the
    /// buffering audit sink, so a crash before its next cadence flush can
    /// drop denial events — and unlike template events they have no WAL
    /// replay to recover from (the denied batch never reached the WAL,
    /// which is the §3.2 point). The durable alerting signal is the
    /// `error.type = permission_denied` counter; the event is forensic
    /// detail. Making denials synchronously durable would put an fsync on
    /// the rejection path — a write-amplification lever for any
    /// authenticated-but-misconfigured (or hostile) sender.
    denial_audit: Mutex<Option<Box<dyn AuditSink + Send>>>,
}

impl Admission {
    pub(crate) fn new() -> Self {
        Self {
            metrics: IngestMetrics::new(),
            denial_audit: Mutex::new(None),
        }
    }

    pub(crate) fn metrics(&self) -> &IngestMetrics {
        &self.metrics
    }

    pub(crate) fn set_denial_audit_sink(&self, sink: Box<dyn AuditSink + Send>) {
        *lock_recover(&self.denial_audit) = Some(sink);
    }

    /// The RFC 0026 §3.2 per-batch tenant-binding enforcement, with the
    /// §3.4 denial surface: the rejection counts on
    /// `ourios.ingest.batches` (`error.type = permission_denied`) and
    /// emits the `IngestDenied` audit event. `None` binding is open
    /// mode — every tenant passes.
    pub(crate) fn enforce_binding(
        &self,
        tenant: &TenantId,
        binding: Option<&AuthBinding>,
    ) -> Result<(), ReceiveError> {
        let Some(binding) = binding else {
            return Ok(());
        };
        check_binding(tenant, binding).inspect_err(|e| self.deny(e))
    }

    fn deny(&self, error: &ReceiveError) {
        let ReceiveError::TenantDenied { token_name, tenant } = error else {
            return;
        };
        self.metrics
            .record_rejected_batch(ERROR_TYPE_PERMISSION_DENIED);
        if let Some(sink) = lock_recover(&self.denial_audit).as_mut() {
            sink.emit(AuditEvent {
                tenant_id: tenant.clone(),
                timestamp: std::time::SystemTime::now(),
                payload: AuditPayload::IngestDenied {
                    token_name: token_name.clone(),
                },
            });
        }
    }
}

/// An export framed for its log: the `TenantOtlpBatch` payload and every
/// record in it under the selected tenant.
pub(crate) struct Framed {
    pub(crate) payload: Vec<u8>,
    pub(crate) records: Vec<OtlpLogRecord>,
}

impl Framed {
    /// Encode before materialisation consumes the request: the frame is
    /// the tenant prefix + the protobuf `ExportLogsServiceRequest`
    /// (RFC 0046 §3.3 / §6.5 step 3). Byte-equality to the wire isn't
    /// required — recoverability is. The batch size lands on the current
    /// `ingest logs` span.
    ///
    /// # Errors
    ///
    /// [`ReceiveError::TenantFrame`]: the selector was validated by the
    /// transport, so the codec's own bound cannot fail here.
    pub(crate) fn of(
        request: ExportLogsServiceRequest,
        tenant: &TenantId,
    ) -> Result<Self, ReceiveError> {
        let payload = TenantBatch::encode(tenant.as_str(), &request.encode_to_vec())
            .map_err(ReceiveError::TenantFrame)?;
        tracing::Span::current().record(
            ourios_semconv::OURIOS_INGEST_BATCH_SIZE,
            super::pipeline::span_count(payload.len()),
        );
        Ok(Self {
            payload,
            records: assign(request, tenant),
        })
    }

    /// The batch as [`IngestMetrics::record_batch`] records it once acked.
    /// RFC 0018 §3.5: out-of-range severities are counted on the
    /// preserved `u8`; the rare non-`u8` extremes narrowed to 0 aren't
    /// separately attributed — accepted limitation.
    pub(crate) fn accepted<'a>(
        &self,
        tenant: &'a TenantId,
        lenient_json: bool,
        elapsed: Duration,
    ) -> AcceptedBatch<'a> {
        AcceptedBatch {
            tenant,
            record_count: self.records.len(),
            severity_out_of_range: self
                .records
                .iter()
                .filter(|r| severity_is_out_of_range(r.severity_number))
                .count(),
            lenient_json,
            elapsed,
        }
    }
}
