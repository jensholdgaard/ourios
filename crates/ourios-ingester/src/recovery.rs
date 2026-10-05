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
//! re-feeding would double-apply), and the configured record and audit
//! sinks only what the miner regenerates for frames above the checkpoint
//! `X` ([`Wal::last_checkpoint`]) — those at or below it are published
//! already (RFC 0052 §3.1, §3.7). The two horizons are independent, so a
//! lagging snapshot (`S < X`) still has its retained `(S, X]` frames
//! delivered to the miner, and a snapshot ahead of a failed checkpoint
//! write (`S > X`) folds `(X, S]` without mining it.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_core::audit::{AuditEvent, AuditSink};
use ourios_core::record::{MinedRecord, RecordSink};
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::{MinerCluster, RestoreError};
use ourios_miner::snapshot::{LegacyMark, RecoveryOutcome, SnapshotError, WalHighWater};
use ourios_wal::{
    FrameKind, FrameSink, HousekeepingError, LedgerError, RecoveryError, TenantBatch,
    TenantHorizon, Wal, WalOffset,
};
use prost::Message;

use crate::metrics::ERROR_TYPE;
use crate::receiver::tenant::assign;
use crate::snapshot_store::{self, SnapshotStoreError};
use crate::template_ids::{Seated, SnapshotTrust, TemplateIds, TemplateIdsError};

/// What recovery did, for the caller to log and for the
/// RFC0008.10 / RFC 0001 §3.5.3–.4 assertions.
#[derive(Debug, Default)]
pub struct RecoveryReport {
    /// Frames `Wal::replay` delivered (every surviving frame).
    pub frames_delivered: u64,
    /// Records handed to the miner (frame offset above the tenant's
    /// horizon, or no horizon).
    pub records_fed_to_miner: u64,
    /// Records suppressed for the miner (frame offset at or below
    /// the tenant's restored high-water mark).
    pub records_suppressed_for_miner: u64,
    /// `Wal::last_checkpoint()` at entry — the publication horizon `X`:
    /// what the miner regenerates for a frame at or below it is withheld
    /// from the configured sinks.
    pub parquet_horizon: Option<WalOffset>,
    /// Mined records withheld from the record sink (frame at or below
    /// `X`, above the tenant's `S`).
    pub records_suppressed_for_parquet: u64,
    /// Audit events the miner regenerated for frames at or below `X`,
    /// withheld from the audit sink.
    pub audit_events_suppressed: u64,
    /// Highest offset delivered during replay.
    pub max_delivered: Option<WalOffset>,
    /// Per-tenant snapshot outcome, one entry per artefact found.
    pub tenants: Vec<TenantRecovery>,
    /// The template-id high-water this start seated the miner above
    /// (RFC 0059 §3.4).
    pub template_ids: Seated,
}

impl RecoveryReport {
    /// The horizons of the snapshots recovery actually restored — the
    /// seed for the barrier's snapshot ledger (RFC 0052 §3.2).
    #[must_use]
    pub fn accepted_horizons(&self) -> Vec<(TenantId, WalOffset)> {
        self.tenants
            .iter()
            .filter_map(|tenant| tenant.horizon().map(|h| (tenant.tenant_id.clone(), h)))
            .collect()
    }
}

/// One tenant's snapshot-recovery outcome.
#[derive(Debug)]
pub struct TenantRecovery {
    pub tenant_id: TenantId,
    pub fate: SnapshotFate,
    /// The restored high-water mark `S` lies below the checkpoint
    /// and `S`'s segment did not survive to replay (RFC 0001 §3.5.4
    /// — external mutation; see [`recover`]). The caller warns.
    pub stale_gap: bool,
}

impl TenantRecovery {
    /// The miner's view of the outcome, for snapshot-load telemetry.
    #[must_use]
    pub fn outcome(&self) -> RecoveryOutcome {
        match &self.fate {
            SnapshotFate::Restored(_) => RecoveryOutcome::Restored,
            SnapshotFate::Discarded(_) => RecoveryOutcome::UnknownOrCorruptDiscarded,
        }
    }

    /// The horizon recovery restored this tenant at. An artefact that
    /// decodes but that `restore_tenant` rejects has none: the next
    /// start would discard it too, so it must never govern reclamation.
    #[must_use]
    pub fn horizon(&self) -> Option<WalOffset> {
        match &self.fate {
            SnapshotFate::Restored(horizon) => Some(*horizon),
            SnapshotFate::Discarded(_) => None,
        }
    }
}

/// What recovery did with one tenant's snapshot artefact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotFate {
    /// The artefact decoded, carried a horizon, and the miner accepted
    /// its state; replay suppresses the tenant's frames at or below it.
    Restored(WalOffset),
    /// The artefact was discarded and the tenant full-replays.
    Discarded(DiscardReason),
}

/// Why recovery discarded a tenant's snapshot artefact: the
/// `error.type` of `ourios.receiver.snapshot.discarded`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscardReason {
    /// Another format version, including every version-1 artefact;
    /// carries the artefact's version byte.
    UnknownVersion(u8),
    /// The payload does not decode.
    Corrupt,
    /// A zero-length artefact.
    Empty,
    /// The artefact records no usable WAL high-water, so it cannot
    /// suppress replay.
    NoHorizon,
    /// The miner rejected the decoded state, for the reason it carries.
    RestoreFailed(RestoreError),
    /// The root never seated against the template-id high-water, so the
    /// artefact holds ids from a pre-RFC 0059 counter (§3.5).
    PredatesHighWater,
    /// A decode failure outside the classes above.
    Other,
}

impl DiscardReason {
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::UnknownVersion(_) => "unknown_version",
            Self::Corrupt => "corrupt",
            Self::Empty => "empty",
            Self::NoHorizon => "no_horizon",
            Self::RestoreFailed(_) => "restore_failed",
            Self::PredatesHighWater => crate::template_ids::names::PREDATES_HIGH_WATER,
            Self::Other => "_OTHER",
        }
    }

    fn of(error: &SnapshotError) -> Self {
        match error {
            SnapshotError::UnknownVersion(version) => Self::UnknownVersion(*version),
            SnapshotError::Corrupt(_) => Self::Corrupt,
            SnapshotError::Empty => Self::Empty,
            _ => Self::Other,
        }
    }
}

impl std::fmt::Display for DiscardReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownVersion(version) => write!(f, "snapshot format version {version}"),
            Self::RestoreFailed(error) => write!(f, "{}: {error}", self.error_type()),
            other => f.write_str(other.error_type()),
        }
    }
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
    /// The post-replay ledger rebuild failed (RFC 0052 §3.7) — I/O, or
    /// a frame carrying a tenant above RFC 0048 §3.1's bound, which
    /// fails startup closed rather than truncating the key.
    Ledger(LedgerError),
    /// RFC 0052 §3.2's legacy stale-gap belt refused a pre-RFC root: a
    /// tenant's oldest surviving frame is above the mark its version-1
    /// snapshot recorded, or the tenant has no decodable mark at all.
    LegacyStaleGap(HousekeepingError),
    /// The same belt, for a tenant whose version-1 snapshot does not
    /// decode even for its mark. It fails closed whether or not the
    /// tenant still has frames in the WAL: with none left, booting
    /// would silently discard the only record of its templates.
    LegacyMarkUnreadable(TenantId),
    /// The template-id high-water could not be read, bootstrapped or
    /// reserved from (RFC 0059 §3.4): a guessed floor could re-issue an id
    /// published rows carry.
    TemplateIds(TemplateIdsError),
}

impl std::fmt::Display for RecoveryDriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "recovery snapshot store: {e}"),
            Self::Replay(e) => write!(f, "recovery WAL replay: {e:?}"),
            Self::Ledger(e) => write!(f, "recovery WAL ledger rebuild: {e}"),
            Self::LegacyStaleGap(e) => write!(f, "recovery legacy stale-gap check: {e}"),
            Self::LegacyMarkUnreadable(tenant) => write!(
                f,
                "recovery legacy stale-gap check: tenant {} has a version-1 snapshot whose \
                 high-water mark cannot be read (RFC 0052 §3.2)",
                tenant.as_str()
            ),
            Self::TemplateIds(e) => write!(f, "recovery template-id high-water: {e}"),
        }
    }
}

impl std::error::Error for RecoveryDriverError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(e) => Some(e),
            Self::Ledger(e) => Some(e),
            Self::TemplateIds(e) => Some(e),
            Self::LegacyStaleGap(_) | Self::LegacyMarkUnreadable(_) | Self::Replay(_) => None,
        }
    }
}

/// Run startup recovery (RFC 0008 §6.6): restore per-tenant
/// snapshots from `snapshots_root`, then replay the WAL into `miner`
/// under per-tenant suppression horizons. The caller invokes this
/// before opening any listener (RFC0008.10 — no live append
/// interleaves with replay) and logs each `stale_gap` tenant.
///
/// A snapshot whose recorded high-water mark is absent or fails UUID
/// parsing, or whose payload `MinerCluster::restore_tenant` rejects,
/// is treated exactly like a corrupt artefact: discarded,
/// [`RecoveryOutcome::UnknownOrCorruptDiscarded`], full replay for
/// that tenant (RFC 0001 §6.9 — inconsistent means corrupt). Each
/// discard emits one `ourios.receiver.snapshot.discarded` event naming
/// the tenant and the [`DiscardReason`].
///
/// Before replay mints anything, `ids` seats the miner above the store's
/// template-id high-water (RFC 0059 §3.4), so nothing replay or later
/// ingest allocates can equal an id issued before this start, whatever
/// was restored, discarded or never found.
///
/// # Errors
///
/// [`RecoveryDriverError`] on snapshot-store I/O or replay failure
/// (including an `OtlpBatch` frame that fails protobuf decode or
/// tenant fan-out — corruption-adjacent, surfaced loudly), or when the
/// template-id high-water cannot be seated.
pub fn recover(
    wal: &mut Wal,
    snapshots_root: &Path,
    miner: &mut MinerCluster,
    ids: &TemplateIds,
) -> Result<RecoveryReport, RecoveryDriverError> {
    let parquet_horizon = wal.last_checkpoint();
    let artefacts =
        snapshot_store::load_all_durable(snapshots_root).map_err(RecoveryDriverError::Store)?;

    let trust =
        SnapshotTrust::of(snapshots_root, ids.store()).map_err(RecoveryDriverError::TemplateIds)?;
    let Restored {
        mut tenants,
        horizons,
        legacy,
    } = restore_artefacts(miner, artefacts, trust);
    let template_ids = seat_root(snapshots_root, miner, ids, trust)?;
    let replay = replay_gated(wal, miner, &horizons, parquet_horizon)?;
    ids.finish_replay();
    // RFC 0052 §3.7: recovery ends by rebuilding the ledger, and it
    // ends there rather than at `Wal::open` because open runs before
    // replay has healed a torn tail — a figure taken there would count
    // torn bytes. Without this call the WAL would export zero
    // unreclaimed bytes, and the housekeeping sweep would have no
    // restart debris to pop, which is #793's shape all over again: a
    // method whose only callers are tests.
    wal.rebuild_ledger().map_err(RecoveryDriverError::Ledger)?;
    // Before the caller's post-recovery write replaces the version-1
    // artefacts whose marks this check reads.
    refuse_legacy_stale_gaps(wal, &horizons, legacy)?;

    let reclaimed = wal.reclaimed_through();
    for tenant in &mut tenants {
        if let Some(horizon) = horizons.get(&tenant.tenant_id) {
            tenant.stale_gap = stale_gap(*horizon, parquet_horizon, &replay.segments_seen)
                && !reclaim_explains(*horizon, reclaimed.get(&tenant.tenant_id));
        }
    }

    Ok(RecoveryReport {
        frames_delivered: replay.frames_delivered,
        records_fed_to_miner: replay.records_fed,
        records_suppressed_for_miner: replay.records_suppressed,
        parquet_horizon,
        records_suppressed_for_parquet: replay.withheld.records,
        audit_events_suppressed: replay.withheld.events,
        max_delivered: replay.max_delivered,
        tenants,
        template_ids,
    })
}

/// What one replay pass saw and withheld.
struct Replayed {
    frames_delivered: u64,
    records_fed: u64,
    records_suppressed: u64,
    segments_seen: HashSet<uuid::Uuid>,
    max_delivered: Option<WalOffset>,
    withheld: Withheld,
}

/// Replay the WAL into `miner` with its configured sinks held aside, so
/// that nothing it regenerates for a frame at or below `checkpoint`
/// reaches them (RFC 0052 §3.7). The sinks are handed back whether or
/// not the replay succeeds.
fn replay_gated(
    wal: &mut Wal,
    miner: &mut MinerCluster,
    horizons: &HashMap<TenantId, WalOffset>,
    checkpoint: Option<WalOffset>,
) -> Result<Replayed, RecoveryDriverError> {
    let capture = ReplayCapture::install(miner);
    let mut sink = DriverSink {
        miner,
        horizons,
        checkpoint,
        capture,
        frames_delivered: 0,
        records_fed: 0,
        records_suppressed: 0,
        segments_seen: HashSet::new(),
        max_delivered: None,
    };
    let replayed = wal.replay(&mut sink);
    let DriverSink {
        miner,
        capture,
        frames_delivered,
        records_fed,
        records_suppressed,
        segments_seen,
        max_delivered,
        ..
    } = sink;
    let withheld = capture.restore(miner);
    replayed.map_err(RecoveryDriverError::Replay)?;
    Ok(Replayed {
        frames_delivered,
        records_fed,
        records_suppressed,
        segments_seen,
        max_delivered,
        withheld,
    })
}

/// Where one replayed frame goes. A frame the tenant's snapshot folds is
/// not mined at all; one at or below the checkpoint is mined for the
/// miner's state alone, its rows and events being published already; any
/// other is mined and published. `max(X, S)` is therefore the record
/// gate, and `X` the audit gate, since a folded frame regenerates nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Folded,
    MinerOnly,
    Published,
}

fn route(offset: WalOffset, snapshot: Option<&WalOffset>, checkpoint: Option<WalOffset>) -> Route {
    match (snapshot, checkpoint) {
        (Some(horizon), _) if offset <= *horizon => Route::Folded,
        (_, Some(mark)) if offset <= mark => Route::MinerOnly,
        _ => Route::Published,
    }
}

/// A buffer the miner emits into during replay, drained once per frame.
struct Capture<T>(Arc<Mutex<Vec<T>>>);

impl<T> Clone for Capture<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> Capture<T> {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    fn push(&self, item: T) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(item);
    }

    fn take(&self) -> Vec<T> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl RecordSink for Capture<MinedRecord> {
    fn emit(&mut self, record: MinedRecord) {
        self.push(record);
    }
}

impl AuditSink for Capture<AuditEvent> {
    fn emit(&mut self, event: AuditEvent) {
        self.push(event);
    }
}

/// What the gate kept from the configured sinks.
#[derive(Debug, Default, Clone, Copy)]
struct Withheld {
    records: u64,
    events: u64,
}

/// The miner's configured sinks, held aside for the replay while it emits
/// into capture buffers.
struct ReplayCapture {
    records: Capture<MinedRecord>,
    events: Capture<AuditEvent>,
    record_sink: Box<dyn RecordSink>,
    audit_sink: Box<dyn AuditSink>,
    withheld: Withheld,
}

impl ReplayCapture {
    fn install(miner: &mut MinerCluster) -> Self {
        let (records, events) = (Capture::new(), Capture::new());
        Self {
            record_sink: miner.replace_record_sink(Box::new(records.clone())),
            audit_sink: miner.replace_audit_sink(Box::new(events.clone())),
            records,
            events,
            withheld: Withheld::default(),
        }
    }

    /// Forward what the frame just mined emitted, in emission order, or
    /// count it as withheld.
    fn settle(&mut self, route: Route) {
        let (records, events) = (self.records.take(), self.events.take());
        match route {
            // Events first: a record emit can publish inline, and the sink's
            // audit barrier must already see the template event it flushes
            // ahead of that record.
            Route::Published => {
                for event in events {
                    self.audit_sink.emit(event);
                }
                for record in records {
                    self.record_sink.emit(record);
                }
            }
            Route::MinerOnly | Route::Folded => {
                self.withheld.records += records.len() as u64;
                self.withheld.events += events.len() as u64;
            }
        }
    }

    fn restore(self, miner: &mut MinerCluster) -> Withheld {
        drop(miner.replace_record_sink(self.record_sink));
        drop(miner.replace_audit_sink(self.audit_sink));
        self.withheld
    }
}

/// What restoring the snapshot artefacts left: each tenant's outcome,
/// the horizons of those restored, and what version-1 artefacts offer
/// RFC 0052 §3.2's legacy check.
struct Restored {
    tenants: Vec<TenantRecovery>,
    horizons: HashMap<TenantId, WalOffset>,
    legacy: LegacyMarks,
}

/// Every version-1 artefact's tenant, split by whether its mark reads.
#[derive(Default)]
struct LegacyMarks {
    recorded: HashMap<TenantId, WalOffset>,
    unreadable: Vec<TenantId>,
}

impl LegacyMarks {
    fn note(&mut self, tenant: &TenantId, bytes: &[u8]) {
        match ourios_miner::snapshot::legacy_v1_mark(bytes) {
            LegacyMark::NotLegacy => {}
            LegacyMark::Recorded(mark) => match snapshot_store::offset_of(&mark) {
                Some(offset) => {
                    self.recorded.insert(tenant.clone(), offset);
                }
                None => self.unreadable.push(tenant.clone()),
            },
            LegacyMark::Unreadable => self.unreadable.push(tenant.clone()),
        }
    }
}

/// Restore every artefact `trust` allows, and discard every other one as
/// predating the root's first template-id seat.
fn restore_artefacts(
    miner: &mut MinerCluster,
    artefacts: Vec<(TenantId, Vec<u8>)>,
    trust: SnapshotTrust,
) -> Restored {
    let mut restored = Restored {
        tenants: Vec::with_capacity(artefacts.len()),
        horizons: HashMap::new(),
        legacy: LegacyMarks::default(),
    };
    for (tenant_id, bytes) in artefacts {
        let fate = if trust.restores() {
            restore_artefact(miner, &tenant_id, &bytes)
        } else {
            SnapshotFate::Discarded(DiscardReason::PredatesHighWater)
        };
        match &fate {
            SnapshotFate::Restored(horizon) => {
                restored.horizons.insert(tenant_id.clone(), *horizon);
            }
            SnapshotFate::Discarded(reason) => announce_discard(&tenant_id, reason),
        }
        restored.legacy.note(&tenant_id, &bytes);
        restored.tenants.push(TenantRecovery {
            tenant_id,
            fate,
            stale_gap: false,
        });
    }
    restored
}

/// Seat `miner` above the template-id high-water (RFC 0059 §3.4–§3.5).
/// A root that never seated loses its untrusted artefacts first, and
/// records its marker only once they are gone, so a crash at any earlier
/// step leads the next start to the same decision.
fn seat_root(
    snapshots_root: &Path,
    miner: &mut MinerCluster,
    ids: &TemplateIds,
    trust: SnapshotTrust,
) -> Result<Seated, RecoveryDriverError> {
    if trust == SnapshotTrust::PredatesHighWater {
        snapshot_store::remove_all(snapshots_root).map_err(RecoveryDriverError::Store)?;
    }
    let seated = ids
        .start(miner, trust.may_bootstrap())
        .map_err(RecoveryDriverError::TemplateIds)?;
    ids.record_seat(snapshots_root, trust, seated)
        .map_err(RecoveryDriverError::TemplateIds)?;
    Ok(seated)
}

/// Restore one artefact into `miner`.
fn restore_artefact(miner: &mut MinerCluster, tenant_id: &TenantId, bytes: &[u8]) -> SnapshotFate {
    let state = match ourios_miner::snapshot::load_snapshot(bytes) {
        Ok(state) => state,
        Err(e) => return SnapshotFate::Discarded(DiscardReason::of(&e)),
    };
    // A restorable snapshot requires a concrete horizon: restoring
    // without one cannot suppress, so replay would re-feed every frame
    // the snapshot already folded — exactly the v1 double-apply hazard.
    // §6.9 maps a missing horizon to the discard class, same as an
    // unparseable one.
    let Some(horizon) = parse_high_water(state.wal_high_water.as_ref()) else {
        return SnapshotFate::Discarded(DiscardReason::NoHorizon);
    };
    match miner.restore_tenant(tenant_id, &state) {
        Ok(()) => SnapshotFate::Restored(horizon),
        Err(error) => SnapshotFate::Discarded(DiscardReason::RestoreFailed(error)),
    }
}

fn announce_discard(tenant_id: &TenantId, reason: &DiscardReason) {
    tracing::warn!(
        name: ourios_semconv::EVENT_OURIOS_RECEIVER_SNAPSHOT_DISCARDED,
        {
            { ourios_semconv::OURIOS_TENANT } = tenant_id.as_str(),
            { ERROR_TYPE } = reason.error_type(),
        },
        "startup recovery discarded tenant {:?}'s miner snapshot ({reason}); its miner state \
         will be rebuilt next by replaying the remaining WAL frames, and if the restart \
         succeeds, templates first seen in reclaimed frames re-mint — drift is observable via \
         the RFC 0010 drift query",
        tenant_id.as_str(),
    );
}

/// RFC 0052 §3.2's legacy stale-gap belt, over every restored horizon
/// and every version-1 artefact. Only a pre-RFC root is checked; the WAL
/// decides which roots those are.
fn refuse_legacy_stale_gaps(
    wal: &Wal,
    horizons: &HashMap<TenantId, WalOffset>,
    legacy: LegacyMarks,
) -> Result<(), RecoveryDriverError> {
    if !wal.on_legacy_branch() {
        return Ok(());
    }
    if let Some(tenant) = legacy.unreadable.into_iter().next() {
        return Err(RecoveryDriverError::LegacyMarkUnreadable(tenant));
    }
    let mut marks: HashMap<TenantId, TenantHorizon> = legacy
        .recorded
        .into_iter()
        .map(|(tenant, mark)| (tenant, TenantHorizon::RecordedOnly(mark)))
        .collect();
    marks.extend(
        horizons
            .iter()
            .map(|(tenant, horizon)| (tenant.clone(), TenantHorizon::Restorable(*horizon))),
    );
    wal.refuse_legacy_stale_gaps_at_open(&marks)
        .map_err(RecoveryDriverError::LegacyStaleGap)
}

/// Write one snapshot artefact per live tenant in `miner`, each stamped
/// at that tenant's own folded horizon (RFC 0052 §3.1) — the
/// post-recovery and shutdown cadence points' stamp. Returns the horizon
/// every written artefact carries, for seeding the snapshot ledger with
/// exactly what was installed. A tenant with no folded horizon is written
/// without one, which the next start discards and full-replays.
///
/// # Errors
///
/// [`SnapshotStoreError`] on encode or filesystem failure; the artefacts
/// written before the failure stay written. The snapshot is a rebuildable
/// cache, so callers on the shutdown path downgrade this to a warning.
pub fn write_folded_snapshots(
    root: &Path,
    miner: &MinerCluster,
) -> Result<Vec<(TenantId, WalOffset)>, SnapshotStoreError> {
    let mut installed = Vec::new();
    for tenant_id in miner.tenant_ids() {
        let mark = miner
            .folded_horizon(&tenant_id)
            .and_then(snapshot_store::offset_of);
        let mut state = miner.snapshot_state(&tenant_id);
        state.wal_high_water = mark.map(snapshot_store::high_water);
        snapshot_store::write(root, &tenant_id, &state)?;
        if let Some(mark) = mark {
            installed.push((tenant_id, mark));
        }
    }
    Ok(installed)
}

/// Stale-gap detection (RFC 0001 §3.5.4): a restored horizon `S`
/// below the checkpoint whose segment never surfaced during replay may
/// mean frames in `(S, oldest surviving)` are gone. Under per-tenant
/// horizons (RFC 0052 §3.1) that shape is also the normal one — an idle
/// tenant's own last segment is reclaimed once its horizon covers it —
/// so a hit is a gap only when [`reclaim_explains`] does not account
/// for it. What remains is external mutation of `wal_root`, and the
/// warning names the gap rather than staying silent (hazard #5; the
/// re-minting drift is observable via the RFC 0010 drift query).
fn stale_gap(
    horizon: WalOffset,
    checkpoint: Option<WalOffset>,
    segments_seen: &HashSet<uuid::Uuid>,
) -> bool {
    match checkpoint {
        Some(checkpoint) => horizon < checkpoint && !segments_seen.contains(&horizon.segment),
        None => false,
    }
}

/// RFC 0052 §3.2's restated witness: an absent horizon segment is
/// explained when the `RECLAIM` record says a pass reclaimed the
/// tenant's frames at or above `S`. Only `S` above the tenant's
/// reclaimed-through, or no entry at all, leaves the absence unexplained.
fn reclaim_explains(horizon: WalOffset, reclaimed_through: Option<&WalOffset>) -> bool {
    reclaimed_through.is_some_and(|through| *through >= horizon)
}

/// Parse a snapshot's recorded high-water mark into a [`WalOffset`].
/// `None` — the mark is absent or its segment UUID is unparseable —
/// is the caller's discard-as-corrupt signal: a restorable snapshot
/// requires a concrete horizon.
fn parse_high_water(high_water: Option<&WalHighWater>) -> Option<WalOffset> {
    snapshot_store::offset_of(high_water?)
}

/// The §6.6 [`FrameSink`]: per `TenantOtlpBatch` frame, decode the tenant
/// prefix then the export (RFC 0046 §3.3) → [`assign`] every record to that
/// tenant → feed each to the miner iff the frame offset is above the
/// tenant's horizon.
struct DriverSink<'a> {
    miner: &'a mut MinerCluster,
    horizons: &'a HashMap<TenantId, WalOffset>,
    checkpoint: Option<WalOffset>,
    capture: ReplayCapture,
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
            // A pre-RFC 0046 frame carries no tenant; replay does not guess
            // (RFC 0046 §3.3). Rejected as unsupported by the driver — the
            // frame's own CRC passed, so this is not RFC0008.5 corruption.
            FrameKind::OtlpBatch => {
                return Err(reject(
                    kind,
                    offset,
                    &"legacy OtlpBatch frame (kind 0x01, pre-RFC 0046) carries no tenant and \
                      cannot be replayed by this version — drain the WAL under the previous \
                      version, or delete it",
                ));
            }
            FrameKind::TenantOtlpBatch => self.consume_batch(offset, payload)?,
            // The miner's regeneration is the only source of replayed
            // events (RFC 0052 §3.7): feeding a stored event as well
            // would forward it twice. Nothing writes the kind yet
            // (`encode_audit_event` is RFC 0008 §9's stub), and an
            // encoder that lands must keep this arm or amend §3.7.
            FrameKind::AuditEvent => {}
        }
        Ok(())
    }
}

impl DriverSink<'_> {
    /// Feed one tenant's frame to the miner unless its restored horizon
    /// already folds it, advance that tenant's folded horizon to the
    /// frame (RFC 0052 §3.1) — replay folds in WAL order, as ingest does —
    /// and publish what it regenerated only above the checkpoint.
    fn consume_batch(&mut self, offset: WalOffset, payload: &[u8]) -> Result<(), RecoveryError> {
        let kind = FrameKind::TenantOtlpBatch;
        let batch = TenantBatch::decode(payload).map_err(|e| reject(kind, offset, &e))?;
        let tenant = TenantId::new(batch.tenant);
        let request = ExportLogsServiceRequest::decode(batch.protobuf)
            .map_err(|e| reject(kind, offset, &e))?;
        let records = assign(request, &tenant);
        let count = records.len() as u64;
        match route(offset, self.horizons.get(&tenant), self.checkpoint) {
            Route::Folded => self.records_suppressed += count,
            mined => {
                for record in &records {
                    self.miner.ingest(record);
                }
                self.records_fed += count;
                self.miner
                    .fold_through(&tenant, snapshot_store::high_water(offset));
                self.capture.settle(mined);
            }
        }
        Ok(())
    }
}

/// A WAL-fsync'd frame failing decode or fan-out is
/// corruption-adjacent (it was valid when acked), so replay stops
/// loudly rather than skipping it.
fn reject(kind: FrameKind, offset: WalOffset, error: &dyn std::fmt::Display) -> RecoveryError {
    RecoveryError::SinkRejected {
        detail: format!(
            "{kind:?} frame at {}+{}: {error}",
            offset.segment, offset.byte
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ourios_config::MinerConfig;

    const SEGMENT: &str = "0190b3c8-1a2b-7c3d-9e4f-50607080a0b0";

    fn offset(segment: &str, byte: u64) -> WalOffset {
        WalOffset {
            segment: uuid::Uuid::parse_str(segment).expect("test uuid"),
            byte,
        }
    }

    #[test]
    fn parse_high_water_accepts_a_concrete_mark() {
        let hw = WalHighWater {
            segment: SEGMENT.to_string(),
            byte: 64,
        };
        assert_eq!(parse_high_water(Some(&hw)), Some(offset(SEGMENT, 64)));
    }

    #[test]
    fn parse_high_water_rejects_an_unparseable_segment() {
        let hw = WalHighWater {
            segment: "not-a-uuid".to_string(),
            byte: 64,
        };
        assert_eq!(parse_high_water(Some(&hw)), None);
    }

    #[test]
    fn parse_high_water_rejects_an_absent_mark() {
        // A restorable snapshot requires a concrete horizon (the
        // double-apply hazard); absent maps to the same discard
        // signal as unparseable.
        assert_eq!(parse_high_water(None), None);
    }

    #[test]
    fn stale_gap_flags_a_horizon_below_the_checkpoint_whose_segment_is_unseen() {
        let s = offset("00000000-0000-7000-8000-000000000001", 10);
        let x = offset("00000000-0000-7000-8000-000000000002", 5);
        let seen = HashSet::from([x.segment]);
        assert!(stale_gap(s, Some(x), &seen));
    }

    #[test]
    fn a_reclaim_entry_at_or_above_the_horizon_explains_its_absent_segment() {
        let s = offset(SEGMENT, 10);
        assert!(reclaim_explains(s, Some(&offset(SEGMENT, 10))), "at S");
        assert!(reclaim_explains(s, Some(&offset(SEGMENT, 11))), "above S");
        assert!(!reclaim_explains(s, Some(&offset(SEGMENT, 9))), "below S");
        assert!(!reclaim_explains(s, None), "nothing reclaimed");
    }

    #[test]
    fn stale_gap_is_false_without_a_checkpoint() {
        let s = offset("00000000-0000-7000-8000-000000000001", 10);
        assert!(!stale_gap(s, None, &HashSet::new()));
    }

    #[test]
    fn stale_gap_is_false_when_the_horizon_segment_survived() {
        let s = offset("00000000-0000-7000-8000-000000000001", 10);
        let x = offset("00000000-0000-7000-8000-000000000002", 5);
        let seen = HashSet::from([s.segment, x.segment]);
        assert!(!stale_gap(s, Some(x), &seen));
    }

    #[test]
    fn route_folds_at_or_below_the_snapshot_whatever_the_checkpoint() {
        let s = offset(SEGMENT, 50);
        for checkpoint in [None, Some(offset(SEGMENT, 20)), Some(offset(SEGMENT, 80))] {
            assert_eq!(route(s, Some(&s), checkpoint), Route::Folded);
        }
    }

    #[test]
    fn route_mines_only_between_the_snapshot_and_the_checkpoint() {
        let (s, x) = (offset(SEGMENT, 20), offset(SEGMENT, 80));
        assert_eq!(
            route(offset(SEGMENT, 50), Some(&s), Some(x)),
            Route::MinerOnly
        );
        assert_eq!(route(x, Some(&s), Some(x)), Route::MinerOnly);
        assert_eq!(route(x, None, Some(x)), Route::MinerOnly);
    }

    #[test]
    fn route_publishes_above_both_marks() {
        let (s, x) = (offset(SEGMENT, 20), offset(SEGMENT, 80));
        assert_eq!(
            route(offset(SEGMENT, 81), Some(&s), Some(x)),
            Route::Published
        );
        assert_eq!(route(offset(SEGMENT, 81), None, None), Route::Published);
        assert_eq!(route(offset(SEGMENT, 60), Some(&x), Some(s)), Route::Folded);
        assert_eq!(
            route(offset(SEGMENT, 90), Some(&x), Some(s)),
            Route::Published
        );
    }

    fn sink<'a>(
        miner: &'a mut MinerCluster,
        horizons: &'a HashMap<TenantId, WalOffset>,
    ) -> DriverSink<'a> {
        let capture = ReplayCapture::install(miner);
        DriverSink {
            miner,
            horizons,
            checkpoint: None,
            capture,
            frames_delivered: 0,
            records_fed: 0,
            records_suppressed: 0,
            segments_seen: HashSet::new(),
            max_delivered: None,
        }
    }

    fn sink_rejected(err: RecoveryError) -> String {
        let RecoveryError::SinkRejected { detail } = err else {
            panic!("expected SinkRejected, got {err:?}");
        };
        detail
    }

    #[test]
    fn sink_rejects_a_malformed_payload_naming_the_offset() {
        let mut miner = MinerCluster::new(MinerConfig::default());
        let horizons = HashMap::new();
        let mut sink = sink(&mut miner, &horizons);

        // A valid tenant prefix followed by a truncated varint key that
        // cannot decode as a protobuf message.
        let payload = TenantBatch::encode("acme", &[0xFF; 4]).expect("encode");
        let detail = sink_rejected(
            sink.consume(offset(SEGMENT, 128), FrameKind::TenantOtlpBatch, &payload)
                .expect_err("malformed payload must be rejected"),
        );
        assert!(
            detail.contains(&format!("{SEGMENT}+128")),
            "detail names the offset, got {detail:?}",
        );
    }

    // RFC0046.11 — the tenant prefix is validated before the protobuf; each
    // malformed shape is a SinkRejected (not corruption) naming the offset.
    #[test]
    fn sink_rejects_malformed_tenant_prefixes() {
        let mut miner = MinerCluster::new(MinerConfig::default());
        let horizons = HashMap::new();
        let mut sink = sink(&mut miner, &horizons);
        for (payload, needle) in [
            (vec![7u8], "length prefix"),
            (vec![0, 0, 1], "zero"),
            (vec![1, 1, b'a'], "exceeds"),
            (vec![5, 0, b'a'], "runs past"),
            (vec![1, 0, 0xFF], "UTF-8"),
        ] {
            let detail = sink_rejected(
                sink.consume(offset(SEGMENT, 64), FrameKind::TenantOtlpBatch, &payload)
                    .expect_err("malformed prefix must be rejected"),
            );
            assert!(detail.contains(needle), "{needle}: {detail}");
            assert!(detail.contains(&format!("{SEGMENT}+64")));
        }
    }

    fn one_line_frame(tenant: &str) -> Vec<u8> {
        use opentelemetry_proto::tonic::common::v1::{AnyValue, any_value::Value};
        use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};

        let request = ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                scope_logs: vec![ScopeLogs {
                    log_records: vec![LogRecord {
                        body: Some(AnyValue {
                            value: Some(Value::StringValue("user 1 logged in".to_owned())),
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        TenantBatch::encode(tenant, &request.encode_to_vec()).expect("frame")
    }

    /// RFC 0052 §3.1: replay folds each tenant to its **own** last
    /// replayed frame. A restored tenant whose frames are all at or
    /// below its horizon keeps that horizon, however far another
    /// tenant's frames reach.
    #[test]
    fn replay_folds_each_tenant_to_its_own_last_frame() {
        let (idle, busy) = (TenantId::new("idle"), TenantId::new("busy"));
        let restored_at = offset(SEGMENT, 50);
        let mut miner = MinerCluster::new(MinerConfig::default());
        let mut state = miner.snapshot_state(&idle);
        state.wal_high_water = Some(snapshot_store::high_water(restored_at));
        miner.restore_tenant(&idle, &state).expect("restore");
        let horizons = HashMap::from([(idle.clone(), restored_at)]);
        let mut sink = sink(&mut miner, &horizons);

        for (tenant, byte) in [("idle", 40), ("busy", 60), ("busy", 80)] {
            sink.consume(
                offset(SEGMENT, byte),
                FrameKind::TenantOtlpBatch,
                &one_line_frame(tenant),
            )
            .expect("consume");
        }

        assert_eq!(sink.records_suppressed, 1, "idle's frame is folded already");
        let folded = |tenant| {
            miner
                .folded_horizon(tenant)
                .and_then(snapshot_store::offset_of)
        };
        assert_eq!(folded(&idle), Some(restored_at), "idle keeps its horizon");
        assert_eq!(folded(&busy), Some(offset(SEGMENT, 80)), "busy's own last");
    }

    // RFC0046.5 — a legacy 0x01 frame is unsupported for replay (a
    // SinkRejected naming the offset and remedy), never corruption.
    #[test]
    fn sink_refuses_legacy_otlp_batch_frames() {
        let mut miner = MinerCluster::new(MinerConfig::default());
        let horizons = HashMap::new();
        let mut sink = sink(&mut miner, &horizons);
        let detail = sink_rejected(
            sink.consume(offset(SEGMENT, 32), FrameKind::OtlpBatch, b"anything")
                .expect_err("legacy frame is refused"),
        );
        assert!(detail.contains("legacy"), "{detail}");
        assert!(detail.contains("drain the WAL"), "{detail}");
        assert!(detail.contains(&format!("{SEGMENT}+32")));
    }

    /// RFC 0052 §3.7: the ledger is rebuilt by *this* driver, after
    /// replay, and not by a test. Without the call the WAL would
    /// export zero unreclaimed bytes and the housekeeping sweep would
    /// never see a previous process's rotation debris — #793's shape,
    /// a method whose only callers are tests.
    #[test]
    fn recover_seeds_the_reclaim_ledger_from_the_surviving_root() {
        let tmp = tempfile::TempDir::new().expect("temp");
        let root = tmp.path();
        let mut wal = Wal::open(wal_config(root)).expect("open");
        let payload = TenantBatch::encode(
            "checkout",
            &ExportLogsServiceRequest::default().encode_to_vec(),
        )
        .expect("frame");
        wal.append(FrameKind::TenantOtlpBatch, &payload)
            .expect("append");
        wal.sync().expect("sync");
        drop(wal);
        let partial = root.join(format!("{}.wal.partial", uuid::Uuid::now_v7()));
        std::fs::write(&partial, b"a previous process's rotation debris").expect("partial");

        let mut wal = Wal::open(wal_config(root)).expect("reopen");
        assert_eq!(
            wal.reclaim_state().unreclaimed_bytes,
            0,
            "open alone seeds nothing: it runs before replay has healed a torn tail",
        );

        let mut miner = MinerCluster::new(MinerConfig::default());
        let ids = TemplateIds::new(ourios_parquet::Store::in_memory());
        recover(&mut wal, &root.join("snapshots"), &mut miner, &ids).expect("recover");

        let state = wal.reclaim_state();
        assert!(
            state.unreclaimed_bytes > 0,
            "recovery seeds the byte figure from the validated frames",
        );
        assert_eq!(
            state.stale_partials, 1,
            "and the partial list the sweep pops from, without a listing on the pass",
        );
        let cap = usize::try_from(ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS).expect("cap fits");
        wal.housekeeping_pass(&ourios_wal::SnapshotHorizons::NoConsumer, cap)
            .expect("housekeeping");
        assert!(
            !partial.exists(),
            "so the very first pass sweeps the debris"
        );
    }

    fn wal_config(root: &Path) -> ourios_wal::WalConfig {
        ourios_wal::WalConfig {
            root: root.to_path_buf(),
            batch_window_ms: 100,
            segment_size_bytes: 128 * 1024 * 1024,
            segment_age_secs: 600,
            housekeeping_secs: 60,
            max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
            rotation_retry_attempts: ourios_wal::DEFAULT_ROTATION_RETRY_ATTEMPTS,
            macos_full_fsync: false,
        }
    }
}
