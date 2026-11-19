//! A group's miner (RFC 0065 §6.5, §6.6): mines a group's committed log
//! from its state machine's [`Applied`] events, so every tail replica
//! that applies the same log mines the same rows, `template_id` included.
//!
//! It holds no task and does no I/O of its own. Its template ids come only
//! from the id blocks the group applied, in log order ([`LogBlocks`]), and
//! its configuration only from the group's miner configuration entries,
//! the cluster default with no per-tenant override. The batches before the
//! first configuration are mined under it once it applies, read back from
//! the group's log ([`GroupLogRead`]), which no checkpoint can purge below
//! a configuration; until then the miner keeps only the index of the first.
//! They are read back one at a time ([`GroupMiner::next_held`]), so no step
//! holds more than one entry's output however many waited.
//! Anything it cannot mine exactly as its peers would, a batch it cannot
//! decode, one its log no longer holds or a snapshot installed over the
//! log, stops it for good with the error naming the entry.
//!
//! Each event returns the audit events the miner emitted with the rows it
//! mined, so the template events those rows need can be written ahead of
//! them, as the local receiver writes them. Every audit event is stamped
//! with the time the leader wrote into the batch being mined, so replicas
//! stamp it alike whatever their own clocks say.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use openraft::EntryPayload;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_core::audit::{AuditEvent, SharedAuditSink};
use ourios_core::clock::Clock;
use ourios_core::otlp::OtlpLogRecord;
use ourios_core::record::MinedRecord;
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::{self, IdReservationError, IdReserver, MinerCluster, TreeShapeChanged};
use ourios_wal::{BatchTime, TenantBatch};
use prost::Message as _;

use super::kept::{Definitions, Kept, TemplateKey};
use super::snapshot_record::{GroupMinerConfig, IdBlock};
use super::state_machine::{Applied, AppliedEvent};
use super::types::{Entry, GroupEntry, LogId, TimedBatch};
use crate::receiver::tenant::assign;

/// Where a mined record was ingested: the index of its entry in the
/// group's log and its place in the entry's frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IngestPosition {
    pub seq: u64,
    pub index: u32,
}

/// One mined record and its ingest position.
#[derive(Debug, Clone, PartialEq)]
pub struct MinedRow {
    pub position: IngestPosition,
    pub record: MinedRecord,
}

/// What one event mined: the audit events the miner emitted, in emission
/// order, and the rows. A row's template events are in its own `Mined` or
/// an earlier one, so writing each `Mined`'s events before its rows
/// writes every template event ahead of the rows that need it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mined {
    pub audit: Vec<AuditEvent>,
    pub rows: Vec<MinedRow>,
}

/// Why a group's miner stopped. It stays stopped, and every later event
/// returns the same error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupMinerError {
    /// The batch's tenant prefix or export does not decode, or it holds
    /// more records than an ingest position can number.
    Undecodable { at: LogId, detail: String },
    /// The miner returned no record for one it ingested.
    Uncaptured { at: LogId, index: u32 },
    /// The configuration changes the tree's shape, which the state
    /// machine refuses first.
    ShapeChanged { at: LogId, source: TreeShapeChanged },
    /// An event whose index is not above the last one followed.
    OutOfOrder { at: LogId, last: LogId },
    /// A snapshot replaced the log through `at`; the miner state at its
    /// cut is in the checkpoint bundle, not in the events.
    Installed { at: LogId },
    /// The first configuration applied at `at`, and the group's log no
    /// longer holds the entry at `index`, applied before it.
    Purged { at: LogId, index: u64 },
    /// A snapshot replaced the log through `at`, and the store holds no
    /// bundle at its cut that restores.
    Unrestorable { at: LogId, detail: String },
}

impl fmt::Display for GroupMinerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Undecodable { at, detail } => {
                write!(f, "batch at {at} does not decode: {detail}")
            }
            Self::Uncaptured { at, index } => {
                write!(f, "the miner returned no record for {at} record {index}")
            }
            Self::ShapeChanged { at, source } => write!(f, "configuration at {at}: {source}"),
            Self::OutOfOrder { at, last } => {
                write!(f, "event at {at} does not follow the last, at {last}")
            }
            Self::Installed { at } => {
                write!(
                    f,
                    "a snapshot through {at} replaced the log the miner follows"
                )
            }
            Self::Unrestorable { at, detail } => {
                write!(
                    f,
                    "the miner does not restore at the snapshot through {at}: {detail}"
                )
            }
            Self::Purged { at, index } => write!(
                f,
                "the log no longer holds entry {index}, to mine under the first \
                 configuration at {at}"
            ),
        }
    }
}

impl Mined {
    fn append(&mut self, Mined { audit, rows }: Mined) {
        self.audit.extend(audit);
        self.rows.extend(rows);
    }
}

impl GroupMinerError {
    /// Its `error.type`, as the registry lists the miner's.
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::Undecodable { .. } => "undecodable",
            Self::Uncaptured { .. } => "uncaptured",
            Self::ShapeChanged { .. } => "shape_changed",
            Self::OutOfOrder { .. } => "out_of_order",
            Self::Installed { .. } => "installed",
            Self::Purged { .. } => "purged",
            Self::Unrestorable { .. } => "unrestorable",
        }
    }
}

impl std::error::Error for GroupMinerError {}

/// The id blocks a group applied that its miner has not drawn yet, in
/// log order. Clones share one queue: the group's miner pushes each block
/// as it applies, and its [`MinerCluster`] draws them as its
/// [`IdReserver`]. Nothing refills it, so every replica runs out at the
/// same position.
#[derive(Debug, Clone, Default)]
pub struct LogBlocks(Arc<Mutex<Blocks>>);

#[derive(Debug, Default)]
struct Blocks {
    queue: VecDeque<IdBlock>,
    /// The block drawn last, which the allocator's ids come from; its
    /// range alone cannot show which block that was (RFC 0065 §6.5).
    current: Option<IdBlock>,
}

impl LogBlocks {
    fn blocks(&self) -> MutexGuard<'_, Blocks> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn push(&self, block: IdBlock) {
        self.blocks().queue.push_back(block);
    }

    /// The blocks not yet drawn.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.blocks().queue.len()
    }

    /// The block drawn last, which the allocator's ids come from.
    #[must_use]
    pub fn current(&self) -> Option<IdBlock> {
        self.blocks().current
    }

    /// Hold `current` as the block drawn last and `pending` as those not
    /// yet drawn, as a bundle records them.
    fn seat(&self, current: Option<IdBlock>, pending: &[IdBlock]) {
        let mut blocks = self.blocks();
        blocks.current = current;
        blocks.queue = pending.iter().copied().collect();
    }

    /// The block drawn last and those not yet drawn, in log order.
    fn state(&self) -> (Option<IdBlock>, Vec<IdBlock>) {
        let blocks = self.blocks();
        (blocks.current, blocks.queue.iter().copied().collect())
    }
}

/// No id block the group applied is left to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoBlockLeft;

impl fmt::Display for NoBlockLeft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("no id block the group's log applied is left")
    }
}

impl std::error::Error for NoBlockLeft {}

impl IdReserver for LogBlocks {
    /// The next block in log order; the allocator refuses one below
    /// `floor`, which the state machine's ordering of blocks rules out.
    fn reserve(&mut self, _floor: u64) -> Result<cluster::IdBlock, IdReservationError> {
        let mut blocks = self.blocks();
        let block = blocks
            .queue
            .pop_front()
            .ok_or_else(|| IdReservationError::new(NoBlockLeft))?;
        blocks.current = Some(block);
        drop(blocks);
        cluster::IdBlock::new(block.after(), block.through()).ok_or_else(|| {
            IdReservationError::new(format!(
                "empty id block ({}, {}]",
                block.after(),
                block.through()
            ))
        })
    }
}

/// The group's log, which the miner reads the batches applied before
/// the first configuration back from.
pub trait GroupLogRead: Send {
    /// The entry at `index`, if the log holds it.
    fn entry(&self, index: u64) -> Option<Entry>;
}

/// A group's log shared by the readers that need it.
pub type SharedLog = Arc<dyn GroupLogRead + Sync>;

impl<T: GroupLogRead + Sync + ?Sized> GroupLogRead for Arc<T> {
    fn entry(&self, index: u64) -> Option<Entry> {
        (**self).entry(index)
    }
}

/// The time of the batch being mined: the miner's only clock, so every
/// audit event it emits carries the batch's time.
#[derive(Debug, Clone, Default)]
struct BatchClock(Arc<AtomicU64>);

impl BatchClock {
    fn set(&self, time: BatchTime) {
        self.0.store(time.nanos(), Ordering::Relaxed);
    }
}

impl Clock for BatchClock {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_nanos(self.0.load(Ordering::Relaxed))
    }
}

/// A decoded batch: its entry, its time and its records, under the
/// frame's tenant.
struct Frame {
    at: LogId,
    time: BatchTime,
    records: Vec<OtlpLogRecord>,
}

impl Frame {
    fn decode(at: LogId, timed: &TimedBatch) -> Result<Self, GroupMinerError> {
        let undecodable = |detail: String| GroupMinerError::Undecodable { at, detail };
        let frame =
            TenantBatch::decode(timed.batch.as_bytes()).map_err(|e| undecodable(e.to_string()))?;
        let request = ExportLogsServiceRequest::decode(frame.protobuf)
            .map_err(|e| undecodable(e.to_string()))?;
        let records = assign(request, &TenantId::new(frame.tenant));
        match u32::try_from(records.len()) {
            Ok(_) => Ok(Self {
                at,
                time: timed.time,
                records,
            }),
            Err(_) => Err(undecodable(format!(
                "{} records, more than an ingest index numbers",
                records.len()
            ))),
        }
    }

    fn mine(
        &self,
        miner: &mut MinerCluster,
        clock: &BatchClock,
    ) -> Result<Vec<MinedRow>, GroupMinerError> {
        let at = self.at;
        clock.set(self.time);
        // `decode` bounds the count to u32, so the zip numbers every record.
        self.records
            .iter()
            .zip(0..)
            .map(|(record, index)| match miner.ingest_mined(record) {
                (_, Some(record)) => Ok(MinedRow {
                    position: IngestPosition {
                        seq: at.index,
                        index,
                    },
                    record,
                }),
                (_, None) => Err(GroupMinerError::Uncaptured { at, index }),
            })
            .collect()
    }
}

/// The batches the first configuration, at `at`, has yet to mine: those
/// in the group's log from `next` up to it.
struct Held {
    next: u64,
    at: LogId,
}

enum Stage {
    /// No configuration has applied: the batches from the index of the
    /// `first` one applied wait for it in the group's log.
    Unconfigured {
        first: Option<u64>,
    },
    Mining {
        miner: Box<MinerCluster>,
        held: Option<Held>,
        /// The configuration in force.
        config: GroupMinerConfig,
    },
    Stopped(GroupMinerError),
}

/// A group's miner, fed its state machine's events in log order.
pub struct GroupMiner {
    stage: Stage,
    log: Box<dyn GroupLogRead>,
    blocks: LogBlocks,
    audit: SharedAuditSink,
    clock: BatchClock,
    last: Option<LogId>,
    /// The index of the last entry that changed what a bundle holds.
    changed: Option<u64>,
}

impl GroupMiner {
    /// A miner for a group whose log it follows from the start, stamping
    /// each audit event with the time of the batch it mined.
    #[must_use]
    pub fn new(log: impl GroupLogRead + 'static) -> Self {
        Self {
            stage: Stage::Unconfigured { first: None },
            log: Box::new(log),
            blocks: LogBlocks::default(),
            audit: SharedAuditSink::new(),
            clock: BatchClock::default(),
            last: None,
            changed: None,
        }
    }

    /// A cluster under `config`, emitting its audit events to this miner
    /// at the batch's time and drawing ids from the group's blocks.
    fn cluster_under(&self, config: GroupMinerConfig) -> MinerCluster {
        let audit = Box::new(self.audit.clone());
        MinerCluster::with_audit_sink(config.config(), audit)
            .with_clock(Box::new(self.clock.clone()))
            .with_id_reserver(Box::new(self.blocks.clone()))
    }

    /// The definitions of `wanted` that the miner's templates for
    /// `tenant` hold: each mined leaf at its version, and each adoption at
    /// the version it binds.
    #[must_use]
    pub fn definitions(&self, tenant: &TenantId, wanted: &BTreeSet<TemplateKey>) -> Definitions {
        let Stage::Mining { miner, .. } = &self.stage else {
            return Definitions::new();
        };
        miner
            .template_definitions(tenant, wanted)
            .into_iter()
            .collect()
    }

    /// The definitions of `wanted` for `tenant` a tail replica holds as one
    /// state: this miner's, and the older versions `kept`, the output it
    /// kept, binds in its audit events.
    #[must_use]
    pub fn definitions_with(
        &self,
        kept: &Kept,
        tenant: &TenantId,
        wanted: &BTreeSet<TemplateKey>,
    ) -> Definitions {
        let mut defined = self.definitions(tenant, wanted);
        for (key, template) in kept.definitions(tenant, wanted) {
            defined.entry(key).or_insert(template);
        }
        defined
    }

    /// The id blocks applied and not yet drawn.
    #[must_use]
    pub fn blocks(&self) -> &LogBlocks {
        &self.blocks
    }

    /// Follow one event; returns the audit events and rows it mined, in
    /// log order, the batches held for a first configuration included.
    ///
    /// # Errors
    ///
    /// The [`GroupMinerError`] that stopped the miner, at this event or
    /// an earlier one.
    pub fn apply(&mut self, applied: Applied) -> Result<Mined, GroupMinerError> {
        let mut mined = self.step(applied)?;
        mined.append(self.drain_held()?);
        Ok(mined)
    }

    /// Follow one event, leaving the batches a first configuration holds
    /// to [`Self::next_held`]. Batches still held when it is called are
    /// mined first, ahead of its own output, to keep log order.
    ///
    /// # Errors
    ///
    /// As [`Self::apply`].
    pub fn step(&mut self, applied: Applied) -> Result<Mined, GroupMinerError> {
        let mut mined = self.drain_held()?;
        let followed = match &self.stage {
            Stage::Stopped(error) => Err(error.clone()),
            Stage::Unconfigured { .. } | Stage::Mining { .. } => self.follow(applied),
        };
        mined.append(self.settle(followed)?);
        Ok(mined)
    }

    /// Mine the next batch held for the first configuration, in log
    /// order; `None` once none is left.
    ///
    /// # Errors
    ///
    /// The [`GroupMinerError`] that stopped the miner at that batch.
    pub fn next_held(&mut self) -> Option<Result<Mined, GroupMinerError>> {
        let mut unbounded = usize::MAX;
        self.next_held_within(&mut unbounded)
    }

    /// As [`Self::next_held`], reading at most `reads` more of the log, what
    /// it reads taken from `reads`; `None` too once they are spent, while
    /// it still [holds](Self::holds) batches to drain (#1310).
    pub fn next_held_within(
        &mut self,
        reads: &mut usize,
    ) -> Option<Result<Mined, GroupMinerError>> {
        let read = self.read_held(reads)?;
        Some(self.settle(read))
    }

    /// The index from which a first configuration's held batches are
    /// still to be read, while it holds any.
    #[must_use]
    pub fn held_from(&self) -> Option<u64> {
        match &self.stage {
            Stage::Mining {
                held: Some(Held { next, .. }),
                ..
            } => Some(*next),
            _ => None,
        }
    }

    /// The index through which every entry it followed has been mined: the
    /// last one followed, but not a batch it still holds unmined (before its
    /// first configuration, or while it drains what that configuration
    /// found), nor the entry a stopped miner failed on (RFC 0065 §6.7: a
    /// read point is served only from what is mined).
    #[must_use]
    pub fn mined_through(&self) -> Option<u64> {
        let last = self.last.map(|last| last.index);
        match &self.stage {
            Stage::Unconfigured { first: Some(first) }
            | Stage::Mining {
                held: Some(Held { next: first, .. }),
                ..
            } => last.map(|last| last.min(first.saturating_sub(1))),
            Stage::Stopped(_) => last.map(|last| last.saturating_sub(1)),
            Stage::Unconfigured { first: None } | Stage::Mining { held: None, .. } => last,
        }
    }

    /// Whether a first configuration still holds batches to mine.
    #[must_use]
    pub fn holds(&self) -> bool {
        matches!(self.stage, Stage::Mining { held: Some(_), .. })
    }

    fn drain_held(&mut self) -> Result<Mined, GroupMinerError> {
        let mut mined = Mined::default();
        while let Some(held) = self.next_held() {
            mined.append(held?);
        }
        Ok(mined)
    }

    /// The audit events emitted since the last call, with `followed`'s
    /// rows; an error stops the miner.
    fn settle(
        &mut self,
        followed: Result<Vec<MinedRow>, GroupMinerError>,
    ) -> Result<Mined, GroupMinerError> {
        let audit = self.audit.drain();
        match followed {
            Ok(rows) => Ok(Mined { audit, rows }),
            Err(error) => {
                self.stage = Stage::Stopped(error.clone());
                Err(error)
            }
        }
    }

    fn follow(&mut self, Applied { at, event }: Applied) -> Result<Vec<MinedRow>, GroupMinerError> {
        match self.last {
            // Rows are positioned by index alone, so a later term never
            // excuses an index that does not advance.
            Some(last) if at.index <= last.index => {
                return Err(GroupMinerError::OutOfOrder { at, last });
            }
            _ => self.last = Some(at),
        }
        if !matches!(
            event,
            AppliedEvent::Flusher(_) | AppliedEvent::Checkpoint(_)
        ) {
            self.changed = Some(at.index);
        }
        match event {
            AppliedEvent::Batch(batch) => self.batch(at, &batch),
            AppliedEvent::IdBlock(block) => {
                self.blocks.push(block);
                Ok(Vec::new())
            }
            AppliedEvent::MinerConfig(config) => self.configure(at, config),
            AppliedEvent::Flusher(_) | AppliedEvent::Checkpoint(_) => Ok(Vec::new()),
            AppliedEvent::Installed(_) => Err(GroupMinerError::Installed { at }),
        }
    }

    fn batch(&mut self, at: LogId, batch: &TimedBatch) -> Result<Vec<MinedRow>, GroupMinerError> {
        match &mut self.stage {
            Stage::Unconfigured { first } => {
                first.get_or_insert(at.index);
                Ok(Vec::new())
            }
            Stage::Mining { miner, .. } => Frame::decode(at, batch)?.mine(miner, &self.clock),
            Stage::Stopped(error) => Err(error.clone()),
        }
    }

    /// The rows of the next held batch in the group's log, or `None`
    /// once the configuration holds none.
    fn read_held(&mut self, reads: &mut usize) -> Option<Result<Vec<MinedRow>, GroupMinerError>> {
        let Stage::Mining { miner, held, .. } = &mut self.stage else {
            return None;
        };
        let Held { next, at } = held.as_mut()?;
        while *next < at.index {
            if *reads == 0 {
                return None;
            }
            *reads -= 1;
            let index = *next;
            *next += 1;
            match self.log.entry(index) {
                None => return Some(Err(GroupMinerError::Purged { at: *at, index })),
                Some(Entry {
                    log_id,
                    payload: EntryPayload::Normal(GroupEntry::Batch(batch)),
                }) => {
                    let frame = Frame::decode(log_id, &batch);
                    return Some(frame.and_then(|frame| frame.mine(miner, &self.clock)));
                }
                Some(_) => {}
            }
        }
        *held = None;
        None
    }

    fn configure(
        &mut self,
        at: LogId,
        config: GroupMinerConfig,
    ) -> Result<Vec<MinedRow>, GroupMinerError> {
        match &mut self.stage {
            Stage::Mining {
                miner,
                config: in_force,
                ..
            } => {
                let refused = |source| GroupMinerError::ShapeChanged { at, source };
                miner.reconfigure(config.config()).map_err(refused)?;
                *in_force = config;
                Ok(Vec::new())
            }
            Stage::Unconfigured { first } => {
                let held = first.map(|next| Held { next, at });
                let miner = Box::new(self.cluster_under(config));
                self.stage = Stage::Mining {
                    miner,
                    held,
                    config,
                };
                Ok(Vec::new())
            }
            Stage::Stopped(error) => Err(error.clone()),
        }
    }
}

mod checkpoint;
mod restore;

pub use checkpoint::{CaptureError, Captured};
pub use restore::RestoreError;

#[cfg(test)]
pub(super) mod tests;
