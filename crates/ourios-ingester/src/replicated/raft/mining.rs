//! A group's miner (RFC 0065 §6.5, §6.6): mines a group's committed log
//! from its state machine's [`Applied`] events, so every tail replica
//! that applies the same log mines the same rows, `template_id` included.
//!
//! It holds no task and does no I/O. Its template ids come only from the
//! id blocks the group applied, in log order ([`LogBlocks`]), and its
//! configuration only from the group's miner configuration entries, the
//! cluster default with no per-tenant override. The entries before the
//! first configuration are mined under it once it applies, so the miner
//! mines nothing until then. Anything it cannot mine exactly as its peers
//! would, a batch it cannot decode or a snapshot installed over the log,
//! stops it for good with the error naming the entry.
//!
//! Each event returns the audit events the miner emitted with the rows it
//! mined, so the template events those rows need can be written ahead of
//! them, as the local receiver writes them.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_core::audit::{AuditEvent, SharedAuditSink};
use ourios_core::clock::{Clock, SystemClock};
use ourios_core::otlp::OtlpLogRecord;
use ourios_core::record::MinedRecord;
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::{self, IdReservationError, IdReserver, MinerCluster, TreeShapeChanged};
use ourios_wal::TenantBatch;
use prost::Message as _;

use super::snapshot_record::{GroupMinerConfig, IdBlock};
use super::state_machine::{Applied, AppliedEvent};
use super::types::{LogId, TenantBatchBytes};
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
pub struct LogBlocks(Arc<Mutex<VecDeque<IdBlock>>>);

impl LogBlocks {
    fn queue(&self) -> MutexGuard<'_, VecDeque<IdBlock>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn push(&self, block: IdBlock) {
        self.queue().push_back(block);
    }

    /// The blocks not yet drawn.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.queue().len()
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
        let block = self
            .queue()
            .pop_front()
            .ok_or_else(|| IdReservationError::new(NoBlockLeft))?;
        cluster::IdBlock::new(block.after(), block.through()).ok_or_else(|| {
            IdReservationError::new(format!(
                "empty id block ({}, {}]",
                block.after(),
                block.through()
            ))
        })
    }
}

/// A decoded batch: its entry and its records, under the frame's tenant.
struct Frame {
    at: LogId,
    records: Vec<OtlpLogRecord>,
}

impl Frame {
    fn decode(at: LogId, batch: &TenantBatchBytes) -> Result<Self, GroupMinerError> {
        let undecodable = |detail: String| GroupMinerError::Undecodable { at, detail };
        let frame =
            TenantBatch::decode(batch.as_bytes()).map_err(|e| undecodable(e.to_string()))?;
        let request = ExportLogsServiceRequest::decode(frame.protobuf)
            .map_err(|e| undecodable(e.to_string()))?;
        let records = assign(request, &TenantId::new(frame.tenant));
        match u32::try_from(records.len()) {
            Ok(_) => Ok(Self { at, records }),
            Err(_) => Err(undecodable(format!(
                "{} records, more than an ingest index numbers",
                records.len()
            ))),
        }
    }

    fn mine(&self, miner: &mut MinerCluster) -> Result<Vec<MinedRow>, GroupMinerError> {
        let at = self.at;
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

enum Stage {
    /// No configuration has applied: the batches wait for the first, and
    /// the clock for the miner it builds.
    Unconfigured {
        pending: Vec<Frame>,
        clock: Box<dyn Clock>,
    },
    Mining(Box<MinerCluster>),
    Stopped(GroupMinerError),
}

/// A group's miner, fed its state machine's events in log order.
pub struct GroupMiner {
    stage: Stage,
    blocks: LogBlocks,
    audit: SharedAuditSink,
    last: Option<LogId>,
}

impl Default for GroupMiner {
    fn default() -> Self {
        Self::new()
    }
}

impl GroupMiner {
    /// A miner for a group whose log it follows from the start, stamping
    /// audit events from the system clock.
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(Box::new(SystemClock::new()))
    }

    /// [`Self::new`], stamping audit events from `clock`.
    #[must_use]
    pub fn with_clock(clock: Box<dyn Clock>) -> Self {
        Self {
            stage: Stage::Unconfigured {
                pending: Vec::new(),
                clock,
            },
            blocks: LogBlocks::default(),
            audit: SharedAuditSink::new(),
            last: None,
        }
    }

    /// The id blocks applied and not yet drawn.
    #[must_use]
    pub fn blocks(&self) -> &LogBlocks {
        &self.blocks
    }

    /// Follow one event; returns the audit events and rows it mined, in
    /// log order.
    ///
    /// # Errors
    ///
    /// The [`GroupMinerError`] that stopped the miner, at this event or
    /// an earlier one.
    pub fn apply(&mut self, applied: Applied) -> Result<Mined, GroupMinerError> {
        let followed = match &self.stage {
            Stage::Stopped(error) => Err(error.clone()),
            Stage::Unconfigured { .. } | Stage::Mining(_) => self.follow(applied),
        };
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
        match event {
            AppliedEvent::Batch(batch) => self.batch(Frame::decode(at, &batch)?),
            AppliedEvent::IdBlock(block) => {
                self.blocks.push(block);
                Ok(Vec::new())
            }
            AppliedEvent::MinerConfig(config) => self.configure(at, config),
            AppliedEvent::Flusher(_) | AppliedEvent::Checkpoint(_) => Ok(Vec::new()),
            AppliedEvent::Installed(_) => Err(GroupMinerError::Installed { at }),
        }
    }

    fn batch(&mut self, frame: Frame) -> Result<Vec<MinedRow>, GroupMinerError> {
        match &mut self.stage {
            Stage::Unconfigured { pending, .. } => {
                pending.push(frame);
                Ok(Vec::new())
            }
            Stage::Mining(miner) => frame.mine(miner),
            Stage::Stopped(error) => Err(error.clone()),
        }
    }

    fn configure(
        &mut self,
        at: LogId,
        config: GroupMinerConfig,
    ) -> Result<Vec<MinedRow>, GroupMinerError> {
        match &mut self.stage {
            Stage::Mining(miner) => {
                let refused = |source| GroupMinerError::ShapeChanged { at, source };
                miner.reconfigure(config.config()).map_err(refused)?;
                Ok(Vec::new())
            }
            Stage::Unconfigured { pending, clock } => {
                let pending = std::mem::take(pending);
                let clock = std::mem::replace(clock, Box::new(SystemClock::new()));
                let audit = Box::new(self.audit.clone());
                let reserver = Box::new(self.blocks.clone());
                let mut miner = Box::new(
                    MinerCluster::with_audit_sink(config.config(), audit)
                        .with_clock(clock)
                        .with_id_reserver(reserver),
                );
                let mut rows = Vec::new();
                for frame in &pending {
                    rows.extend(frame.mine(&mut miner)?);
                }
                self.stage = Stage::Mining(miner);
                Ok(rows)
            }
            Stage::Stopped(error) => Err(error.clone()),
        }
    }
}

#[cfg(test)]
mod tests;
