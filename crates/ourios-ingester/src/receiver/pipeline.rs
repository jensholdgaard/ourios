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

use std::sync::{Arc, Mutex};

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_miner::cluster::MinerCluster;
use ourios_wal::{FrameKind, Wal, WalOffset};
use prost::Message;

use crate::receiver::tenant::{TenantResolutionError, TenantRule, fan_out};

/// The ingest pipeline shared across a listener's requests. The
/// single-writer WAL forces serialization; concurrent requests queue on
/// the mutex (the lock never spans an `.await`, so `std::sync::Mutex`
/// suffices). Used by both the HTTP and gRPC transports.
pub type SharedPipeline = Arc<Mutex<IngestPipeline>>;

/// The durability sink the pipeline appends to and fsyncs through.
///
/// Abstracted (rather than a concrete `Wal`) so the WAL-before-ack
/// ordering can be exercised with a spy that records the `append`/`sync`
/// calls (RFC0003.1/.12 per §8). The only production implementation is
/// [`Wal`].
///
/// `Send` so the pipeline can live behind a shared `Arc<Mutex<_>>` as
/// state in the async HTTP/gRPC listeners.
pub trait Journal: Send {
    /// Append one `OtlpBatch` frame carrying `payload` (not yet durable).
    ///
    /// # Errors
    ///
    /// [`ReceiveError::WalAppend`] on a persistence failure.
    fn append_batch(&mut self, payload: &[u8]) -> Result<(), ReceiveError>;

    /// Fsync — appended frames are durable when this returns `Ok`,
    /// yielding the durable high-water offset when the journal has
    /// one (the WAL does; test spies that persist nothing return
    /// `None`). The snapshot writer records this offset as the
    /// snapshot's WAL high-water mark (RFC 0001 §6.9).
    ///
    /// # Errors
    ///
    /// [`ReceiveError::WalSync`] on an fsync failure.
    fn sync(&mut self) -> Result<Option<WalOffset>, ReceiveError>;
}

impl Journal for Wal {
    fn append_batch(&mut self, payload: &[u8]) -> Result<(), ReceiveError> {
        Wal::append(self, FrameKind::OtlpBatch, payload)
            .map(|_| ())
            .map_err(ReceiveError::WalAppend)
    }

    fn sync(&mut self) -> Result<Option<WalOffset>, ReceiveError> {
        Wal::sync(self).map(Some).map_err(ReceiveError::WalSync)
    }
}

/// The ingester's WAL-before-ack ingest path. Owns the durability
/// [`Journal`], the per-process `MinerCluster`, and the
/// tenant-derivation `TenantRule`.
///
/// No `Debug`: `MinerCluster` holds the per-tenant Drain trees and does
/// not implement it.
pub struct IngestPipeline {
    journal: Box<dyn Journal>,
    miner: MinerCluster,
    rule: TenantRule,
    last_durable: Option<WalOffset>,
}

impl IngestPipeline {
    /// Build a pipeline over a durability [`Journal`] (production: a
    /// `Box<Wal>`), a `MinerCluster`, and a tenant-derivation rule.
    #[must_use]
    pub fn new(journal: Box<dyn Journal>, miner: MinerCluster, rule: TenantRule) -> Self {
        Self {
            journal,
            miner,
            rule,
            last_durable: None,
        }
    }

    /// Seed the durable high-water mark from startup recovery
    /// (`RecoveryReport::max_delivered`). Without the seed, a process
    /// that serves zero requests writes shutdown snapshots with no
    /// high-water mark, forcing the next start to discard them and
    /// full-replay (RFC 0001 §6.9). A later [`Journal::sync`] offset
    /// supersedes the seed.
    #[must_use]
    pub fn with_last_durable(mut self, offset: Option<WalOffset>) -> Self {
        self.last_durable = offset;
        self
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
        self.journal.append_batch(&payload)?;
        if let Some(offset) = self.journal.sync()? {
            self.last_durable = Some(offset);
        }

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

    /// The journal's durable high-water offset after the most recent
    /// acked batch, when known — what a snapshot taken now records as
    /// its WAL high-water mark (RFC 0001 §6.9).
    #[must_use]
    pub fn last_durable(&self) -> Option<WalOffset> {
        self.last_durable
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
            Self::WalAppend(e) => write!(f, "{e}"),
            Self::WalSync(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReceiveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TenantResolution(e) => Some(e),
            Self::WalAppend(e) => Some(e),
            Self::WalSync(e) => Some(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use ourios_core::config::MinerConfig;

    /// Persists nothing; `sync` reports the configured offset.
    struct FixedSyncJournal {
        offset: Option<WalOffset>,
    }

    impl Journal for FixedSyncJournal {
        fn append_batch(&mut self, _payload: &[u8]) -> Result<(), ReceiveError> {
            Ok(())
        }

        fn sync(&mut self) -> Result<Option<WalOffset>, ReceiveError> {
            Ok(self.offset)
        }
    }

    fn pipeline(sync_offset: Option<WalOffset>) -> IngestPipeline {
        IngestPipeline::new(
            Box::new(FixedSyncJournal {
                offset: sync_offset,
            }),
            MinerCluster::new(MinerConfig::default()),
            TenantRule::service_name(),
        )
    }

    fn offset(byte: u64) -> WalOffset {
        WalOffset {
            segment: uuid::Uuid::from_u128(1),
            byte,
        }
    }

    fn string_value(s: &str) -> AnyValue {
        AnyValue {
            value: Some(Value::StringValue(s.to_owned())),
        }
    }

    fn request() -> ExportLogsServiceRequest {
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![KeyValue {
                        key: "service.name".to_owned(),
                        value: Some(string_value("checkout")),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                scope_logs: vec![ScopeLogs {
                    log_records: vec![LogRecord {
                        body: Some(string_value("user 1 logged in")),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    #[test]
    fn seeded_last_durable_holds_until_a_sync_supersedes_it() {
        let seed = offset(10);
        let synced = offset(64);
        let mut pipeline = pipeline(Some(synced)).with_last_durable(Some(seed));

        assert_eq!(pipeline.last_durable(), Some(seed));
        pipeline.ingest(request()).expect("ingest");
        assert_eq!(pipeline.last_durable(), Some(synced));
    }

    #[test]
    fn seeded_last_durable_survives_a_sync_without_an_offset() {
        let seed = offset(10);
        let mut pipeline = pipeline(None).with_last_durable(Some(seed));

        pipeline.ingest(request()).expect("ingest");
        assert_eq!(pipeline.last_durable(), Some(seed));
    }
}
