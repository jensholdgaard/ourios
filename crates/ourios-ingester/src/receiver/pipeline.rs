//! The ingest pipeline — the §6.5 WAL-before-ack business-logic layer.
//!
//! Composes the pieces the earlier slices built (decode → fan-out) with
//! durability and templating: a decoded `ExportLogsServiceRequest` is
//! fanned out per tenant, the whole export is appended as one
//! `FrameKind::OtlpBatch` frame and **fsync'd before any ack**
//! (`CLAUDE.md` §3.4 / RFC0003.1), and only then are the records handed
//! to the miner (§6.5 step ordering). An empty batch takes the
//! fast path: success with no WAL write (RFC0003.12).
//!
//! The live gRPC/HTTP transports wrap this layer; they hand it a decoded
//! request and map its `Result` to the transport-level response.

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_miner::cluster::MinerCluster;
use ourios_wal::{FrameKind, Wal};
use prost::Message;

use crate::receiver::tenant::{TenantResolutionError, TenantRule, fan_out};

/// The ingester's WAL-before-ack ingest path. Owns the single-writer
/// `Wal`, the per-process `MinerCluster`, and the tenant-derivation
/// `TenantRule`.
///
/// No `Debug`: `MinerCluster` holds the per-tenant Drain trees and does
/// not implement it.
pub struct IngestPipeline {
    wal: Wal,
    miner: MinerCluster,
    rule: TenantRule,
}

impl IngestPipeline {
    /// Build a pipeline over an opened `Wal`, a `MinerCluster`, and a
    /// tenant-derivation rule.
    #[must_use]
    pub fn new(wal: Wal, miner: MinerCluster, rule: TenantRule) -> Self {
        Self { wal, miner, rule }
    }

    /// Ingest one decoded export per the §6.5 sequence: fan out, append
    /// the export as a single `OtlpBatch` frame, **fsync**, then hand the
    /// records to the miner, then ack. Returns the number of records
    /// ingested (`0` for the empty fast path).
    ///
    /// The fsync (step 4) completes before this returns `Ok`, so the
    /// caller never acks a batch that isn't durable (`[§3.4]`).
    ///
    /// # Errors
    ///
    /// - [`ReceiveError::TenantResolution`] if any `ResourceLogs` fails
    ///   tenant resolution — the whole batch is rejected before any WAL
    ///   write (RFC0003.4).
    /// - [`ReceiveError::WalAppend`] / [`ReceiveError::WalSync`] if
    ///   persistence fails; the batch is **not** acked.
    pub fn ingest(&mut self, request: ExportLogsServiceRequest) -> Result<usize, ReceiveError> {
        // Encode before fan-out consumes the request: the WAL frame is a
        // protobuf `ExportLogsServiceRequest` (§6.5 step 3). Byte-equality
        // to the wire isn't required — recoverability is.
        let payload = request.encode_to_vec();

        // Steps 1–2: fan out per tenant. An unresolvable Resource rejects
        // the entire batch here, before any WAL write (RFC0003.4).
        let records = fan_out(request, &self.rule)?;

        // Empty fast path (RFC0003.12): no records → success, no WAL
        // frame, miner untouched.
        if records.is_empty() {
            return Ok(0);
        }

        // Step 3: append the export as one OtlpBatch frame. Step 4: fsync
        // — the batch is durable before the ack below.
        self.wal
            .append(FrameKind::OtlpBatch, &payload)
            .map_err(ReceiveError::WalAppend)?;
        self.wal.sync().map_err(ReceiveError::WalSync)?;

        // Step 5: hand records to the miner (only after durability, so a
        // crash between fsync and here replays from the WAL).
        for record in &records {
            self.miner.ingest(record);
        }

        // Step 6: ack.
        Ok(records.len())
    }

    /// The pipeline's miner, for inspection (tests; future metrics).
    #[must_use]
    pub fn miner(&self) -> &MinerCluster {
        &self.miner
    }
}

/// Failure ingesting an export. Tenant-resolution failures reject the
/// whole batch (RFC0003.4); WAL failures mean the batch is not acked
/// (`[§3.4]`).
#[derive(Debug)]
#[non_exhaustive]
pub enum ReceiveError {
    /// A `ResourceLogs` group's Resource did not resolve to a tenant.
    TenantResolution(TenantResolutionError),
    /// Appending the `OtlpBatch` frame to the WAL failed.
    WalAppend(ourios_wal::AppendError),
    /// Fsyncing the WAL failed — the batch must not be acked.
    WalSync(ourios_wal::SyncError),
}

impl From<TenantResolutionError> for ReceiveError {
    fn from(e: TenantResolutionError) -> Self {
        Self::TenantResolution(e)
    }
}

impl std::fmt::Display for ReceiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // `TenantResolutionError`'s own Display already leads with
            // "tenant resolution failed: …"; delegate, don't re-prefix.
            Self::TenantResolution(e) => write!(f, "{e}"),
            Self::WalAppend(e) => write!(f, "WAL append failed: {e:?}"),
            Self::WalSync(e) => write!(f, "WAL sync failed: {e:?}"),
        }
    }
}

impl std::error::Error for ReceiveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TenantResolution(e) => Some(e),
            // `ourios_wal::{AppendError, SyncError}` don't implement
            // `std::error::Error` (RFC 0008 hand-rolled enums), so they
            // can't be returned as a `source`; their detail is in the
            // `Display` above.
            Self::WalAppend(_) | Self::WalSync(_) => None,
        }
    }
}
