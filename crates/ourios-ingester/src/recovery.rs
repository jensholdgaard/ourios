//! Startup recovery driver (RFC 0008 §6.6 / RFC0008.10; RFC 0001
//! §6.9 v2).
//!
//! Runs before the network listeners open: load the per-tenant
//! snapshot artefacts, restore each into the miner, then replay the
//! WAL through the live decode → fan-out → miner pipeline with
//! **per-consumer suppression horizons** — `Wal::replay` delivers
//! every surviving frame, and this driver routes: the miner consumes
//! only frames above its restored snapshot's high-water mark `S` per
//! tenant (frames ≤ `S` are already folded into the snapshot;
//! re-feeding would double-apply), and the Parquet path — once it
//! exists — only frames above the checkpoint `X`
//! ([`Wal::last_checkpoint`]). The two horizons are independent, so
//! a lagging snapshot (`S < X`) still has its retained `(S, X]`
//! frames delivered to the miner.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::MinerCluster;
use ourios_miner::snapshot::{RecoveryOutcome, WalHighWater};
use ourios_wal::{FrameKind, FrameSink, RecoveryError, Wal, WalOffset};
use prost::Message;

use crate::receiver::tenant::{TenantRule, fan_out};
use crate::snapshot_store::{self, SnapshotStoreError};

/// What recovery did, for the caller to log and for the
/// RFC0008.10 / RFC 0001 §3.5.3–.4 assertions.
#[derive(Debug)]
pub struct RecoveryReport {
    /// Frames `Wal::replay` delivered (every surviving frame).
    pub frames_delivered: u64,
    /// Records handed to the miner (frame offset above the tenant's
    /// horizon, or no horizon).
    pub records_fed_to_miner: u64,
    /// Records suppressed for the miner (frame offset at or below
    /// the tenant's restored high-water mark).
    pub records_suppressed_for_miner: u64,
    /// `Wal::last_checkpoint()` at entry — the Parquet-side
    /// suppression horizon. Recorded now, consumed once the Parquet
    /// write path joins the pipeline (no data-side consumer exists
    /// yet to suppress for).
    pub parquet_horizon: Option<WalOffset>,
    /// Highest offset delivered during replay — the high-water mark
    /// a post-recovery snapshot records.
    pub max_delivered: Option<WalOffset>,
    /// Per-tenant snapshot outcome, one entry per artefact found.
    pub tenants: Vec<TenantRecovery>,
}

/// One tenant's snapshot-recovery outcome.
#[derive(Debug)]
pub struct TenantRecovery {
    pub tenant_id: TenantId,
    pub outcome: RecoveryOutcome,
    /// The restored high-water mark `S` lies below the checkpoint
    /// and `S`'s segment did not survive to replay (RFC 0001 §3.5.4
    /// — external mutation; see [`recover`]). The caller warns.
    pub stale_gap: bool,
}

/// Failure during startup recovery. Recovery aborts loudly — a frame
/// that fsync'd as a valid protobuf cannot legitimately fail decode,
/// so a sink rejection here is corruption-adjacent, not skippable.
#[derive(Debug)]
#[non_exhaustive]
pub enum RecoveryDriverError {
    /// Listing or reading snapshot artefacts failed.
    Store(SnapshotStoreError),
    /// `Wal::replay` failed (I/O, frame corruption, or this driver's
    /// sink rejecting a frame that would not decode).
    Replay(RecoveryError),
}

impl std::fmt::Display for RecoveryDriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "recovery snapshot store: {e}"),
            Self::Replay(e) => write!(f, "recovery WAL replay: {e:?}"),
        }
    }
}

impl std::error::Error for RecoveryDriverError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(e) => Some(e),
            Self::Replay(_) => None,
        }
    }
}

/// Run startup recovery (RFC 0008 §6.6): restore per-tenant
/// snapshots from `snapshots_root`, then replay the WAL into `miner`
/// under per-tenant suppression horizons. The caller invokes this
/// before opening any listener (RFC0008.10 — no live append
/// interleaves with replay) and logs each `stale_gap` tenant.
///
/// A snapshot whose recorded high-water segment fails UUID parsing,
/// or whose payload `MinerCluster::restore_tenant` rejects, is
/// treated exactly like a corrupt artefact: discarded,
/// [`RecoveryOutcome::UnknownOrCorruptDiscarded`], full replay for
/// that tenant (RFC 0001 §6.9 — inconsistent means corrupt).
///
/// # Errors
///
/// [`RecoveryDriverError`] on snapshot-store I/O or replay failure
/// (including an `OtlpBatch` frame that fails protobuf decode or
/// tenant fan-out — corruption-adjacent, surfaced loudly).
pub fn recover(
    wal: &mut Wal,
    snapshots_root: &Path,
    miner: &mut MinerCluster,
    rule: &TenantRule,
) -> Result<RecoveryReport, RecoveryDriverError> {
    let parquet_horizon = wal.last_checkpoint();
    let artefacts = snapshot_store::load_all(snapshots_root).map_err(RecoveryDriverError::Store)?;

    let mut tenants = Vec::with_capacity(artefacts.len());
    let mut horizons: HashMap<TenantId, WalOffset> = HashMap::new();
    for (tenant_id, bytes) in artefacts {
        let outcome = match ourios_miner::snapshot::recover(Some(&bytes)) {
            (Some(state), RecoveryOutcome::Restored) => {
                match parse_high_water(state.wal_high_water.as_ref()) {
                    Ok(horizon) => match miner.restore_tenant(&tenant_id, &state) {
                        Ok(()) => {
                            if let Some(horizon) = horizon {
                                horizons.insert(tenant_id.clone(), horizon);
                            }
                            RecoveryOutcome::Restored
                        }
                        Err(_) => RecoveryOutcome::UnknownOrCorruptDiscarded,
                    },
                    Err(()) => RecoveryOutcome::UnknownOrCorruptDiscarded,
                }
            }
            (_, outcome) => outcome,
        };
        tenants.push(TenantRecovery {
            tenant_id,
            outcome,
            stale_gap: false,
        });
    }

    let mut sink = DriverSink {
        miner,
        rule,
        horizons: &horizons,
        frames_delivered: 0,
        records_fed: 0,
        records_suppressed: 0,
        segments_seen: HashSet::new(),
        max_delivered: None,
    };
    wal.replay(&mut sink).map_err(RecoveryDriverError::Replay)?;

    // Stale-gap detection (RFC 0001 §3.5.4): a restored horizon `S`
    // below the checkpoint whose segment never surfaced during
    // replay means frames in `(S, oldest surviving)` are gone.
    // Internally unreachable: the §6.7 retain floor (min over tenant
    // horizons) keeps any segment holding frames above the floor,
    // and a lagging tenant's own `S.segment` is protected by its own
    // membership in the min — so a hit means external mutation of
    // `wal_root`, and the warning names the gap rather than staying
    // silent (hazard #5; the re-minting drift is observable via the
    // RFC 0010 drift query). The rule has no steady-state false
    // positive: in normal operation `S`'s segment either survives
    // (seen during replay) or was reclaimed only after the
    // checkpoint passed it, in which case `S ≥` every reclaimed
    // frame and `S < checkpoint` fails.
    for tenant in &mut tenants {
        if let Some(horizon) = horizons.get(&tenant.tenant_id) {
            tenant.stale_gap = match parquet_horizon {
                Some(checkpoint) => {
                    *horizon < checkpoint && !sink.segments_seen.contains(&horizon.segment)
                }
                None => false,
            };
        }
    }

    Ok(RecoveryReport {
        frames_delivered: sink.frames_delivered,
        records_fed_to_miner: sink.records_fed,
        records_suppressed_for_miner: sink.records_suppressed,
        parquet_horizon,
        max_delivered: sink.max_delivered,
        tenants,
    })
}

/// Write one snapshot artefact per live tenant in `miner`, recording
/// `high_water` as each artefact's WAL high-water mark (RFC 0001
/// §6.9 cadence points: post-recovery and graceful shutdown today;
/// per-segment-rotation once rotation lands). A `None` high water is
/// honest degradation: the next start full-replays for that tenant.
///
/// # Errors
///
/// [`SnapshotStoreError`] on encode or filesystem failure. The
/// snapshot is a rebuildable cache, so callers on the shutdown path
/// downgrade this to a warning.
pub fn write_snapshots(
    root: &Path,
    miner: &MinerCluster,
    high_water: Option<WalOffset>,
) -> Result<(), SnapshotStoreError> {
    for tenant_id in miner.tenant_ids() {
        let mut state = miner.snapshot_state(&tenant_id);
        state.wal_high_water = high_water.map(|offset| WalHighWater {
            segment: offset.segment.to_string(),
            byte: offset.byte,
        });
        snapshot_store::write(root, &tenant_id, &state)?;
    }
    Ok(())
}

/// Parse a snapshot's recorded high-water mark into a [`WalOffset`].
/// `Err(())` (an unparseable segment UUID) is the caller's
/// discard-as-corrupt signal.
fn parse_high_water(high_water: Option<&WalHighWater>) -> Result<Option<WalOffset>, ()> {
    match high_water {
        None => Ok(None),
        Some(hw) => match uuid::Uuid::parse_str(&hw.segment) {
            Ok(segment) => Ok(Some(WalOffset {
                segment,
                byte: hw.byte,
            })),
            Err(_) => Err(()),
        },
    }
}

/// The §6.6 [`FrameSink`]: per `OtlpBatch` frame, decode →
/// [`fan_out`] → feed each record to the miner iff the frame offset
/// is above that record's tenant horizon.
struct DriverSink<'a> {
    miner: &'a mut MinerCluster,
    rule: &'a TenantRule,
    horizons: &'a HashMap<TenantId, WalOffset>,
    frames_delivered: u64,
    records_fed: u64,
    records_suppressed: u64,
    segments_seen: HashSet<uuid::Uuid>,
    max_delivered: Option<WalOffset>,
}

impl FrameSink for DriverSink<'_> {
    fn consume(
        &mut self,
        offset: WalOffset,
        kind: FrameKind,
        payload: &[u8],
    ) -> Result<(), RecoveryError> {
        self.frames_delivered += 1;
        self.segments_seen.insert(offset.segment);
        self.max_delivered = Some(match self.max_delivered {
            Some(max) => max.max(offset),
            None => offset,
        });
        match kind {
            FrameKind::OtlpBatch => {
                let request =
                    ExportLogsServiceRequest::decode(payload).map_err(|e| reject(offset, &e))?;
                let records = fan_out(request, self.rule).map_err(|e| reject(offset, &e))?;
                for record in &records {
                    let feed = match self.horizons.get(&record.tenant_id) {
                        Some(horizon) => offset > *horizon,
                        None => true,
                    };
                    if feed {
                        self.miner.ingest(record);
                        self.records_fed += 1;
                    } else {
                        self.records_suppressed += 1;
                    }
                }
            }
            // Nothing writes AuditEvent frames yet (`encode_audit_event`
            // is the RFC 0008 §9 stub); when the encoder lands these
            // reinject into the audit Parquet queue, gated on the
            // Parquet horizon. Counted in frames_delivered, never a
            // panic — the frame kind is valid on the wire today.
            FrameKind::AuditEvent => {}
        }
        Ok(())
    }
}

/// A WAL-fsync'd frame failing decode or fan-out is
/// corruption-adjacent (it was valid when acked), so replay stops
/// loudly rather than skipping it.
fn reject(offset: WalOffset, error: &dyn std::fmt::Display) -> RecoveryError {
    RecoveryError::SinkRejected {
        detail: format!(
            "OtlpBatch frame at {}+{}: {error}",
            offset.segment, offset.byte
        ),
    }
}
