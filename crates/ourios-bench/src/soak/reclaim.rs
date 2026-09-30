//! RFC 0052 §3.2's reclamation cadence on a synthetic clock — the
//! harness RFC0052.3 runs on (`docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md`
//! §6).
//!
//! The D1/D2 soak paces load on the wall clock and never runs a barrier
//! or a housekeeping pass. This harness drives the pieces the receiver
//! wires — WAL → group-commit coordinator → miner → encode pool →
//! publisher, the capture-only rotation hook, the [`Barrier`] and the
//! [`Housekeeper`] — on a **stepped** synthetic clock instead: each batch
//! advances it by a fixed step, and every barrier and housekeeping tick
//! due on the way runs to completion before time moves on. A pass due at
//! the same instant as a barrier tick runs first, the slower order for
//! reclamation, so the bound a run is held to is the worst case of the
//! two timers' phases.
//!
//! Time advances only once every due tick has finished, so the load is
//! capacity-balanced by construction: the offered rate is what the step
//! and the batch shape say, and reclamation is never behind the clock.
//!
//! One predicate reads the wall clock: the WAL's segment age, from the
//! open segment's `UUIDv7` mint time. The harness notes the instant each
//! segment appears, and a barrier tick whose synthetic time says the open
//! segment has aged waits until the wall clock agrees, so an idle
//! rotation is never credited to a tick the WAL would refuse. Appends are
//! not held that way: a rotation on append stays size-driven unless the
//! wall clock alone ages a segment, so a run that needs age rotation
//! between appends — a low-rate soak — is outside this harness, and
//! [`ReclaimSoak::segments_rolled`] is how a run shows it stayed on size.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use opentelemetry_proto::tonic::common::v1::{AnyValue, any_value};
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use ourios_config::MinerConfig;
use ourios_core::tenant::TenantId;
use ourios_ingester::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use ourios_ingester::barrier::{Barrier, CutOutcome};
use ourios_ingester::encode_pool::EncodePool;
use ourios_ingester::housekeeping::{Housekeeper, HousekeepingTick};
use ourios_ingester::publish::PublishCoordinator;
use ourios_ingester::receiver::{CommitCoordinator, IngestPipeline, ReceiveError, SharedPipeline};
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::Store;
use ourios_wal::{PassOutcome, ReclaimState, RetainFloor, Wal, WalConfig, WalOffset};
use serde::Serialize;

use super::{BASE_UNIX_NANOS, NANOS_PER_SEC, export_request, tenant_name, to_u64};

/// The load is sequential, so the group-commit window only adds latency.
const BATCH_WINDOW_MS: u64 = 1;
const ENCODE_WORKERS: usize = 2;
/// The receiver's sink ceiling, which the barrier bounds coalescing by.
const SINK_CEILING_BYTES: usize = 1024 * 1024 * 1024;
const AUDIT_CEILING_EVENTS: usize = 100_000;
/// Per-record allowance over the body for its OTLP encoding.
const RECORD_OVERHEAD_BYTES: u64 = 128;
/// Per-frame allowance for the resource envelope, the tenant framing and
/// the WAL frame header. [`ReclaimSoak::largest_frame_bytes`] is what a
/// run checks this ceiling against.
const FRAME_OVERHEAD_BYTES: u64 = 1024;
const HOUR_SECS: u64 = 3_600;
/// The receiver's pass cap, which both the WAL and the housekeeper get.
const MAX_UNLINKS_PER_PASS: u32 = ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS;
/// Past the WAL's strict `>` and the `UUIDv7`'s millisecond truncation.
const AGE_MARGIN: Duration = Duration::from_millis(50);

/// One reclamation soak's knobs: the WAL geometry, the two cadences and
/// the offered load.
#[derive(Debug, Clone, Serialize)]
pub struct ReclaimSoakConfig {
    /// `WalConfig::segment_size_bytes`.
    pub segment_size_bytes: u64,
    /// `WalConfig::segment_age_secs`, the one knob the WAL reads against
    /// the wall clock.
    pub segment_age_secs: u64,
    /// §3.1's `barrier_secs`.
    pub barrier_secs: u64,
    /// `WalConfig::housekeeping_secs`.
    pub housekeeping_secs: u64,
    /// Synthetic seconds between batches; with the batch shape, the
    /// offered rate.
    pub secs_per_batch: u64,
    pub records_per_batch: usize,
    /// Body bytes per record.
    pub record_bytes: usize,
    /// Tenants fed round-robin, one per batch.
    pub tenants: usize,
}

impl Default for ReclaimSoakConfig {
    /// The receiver's cadences — the barrier on the sink's 300 s age
    /// trigger, housekeeping on the WAL's 60 s default — over the
    /// smallest segment the WAL accepts, at a rate that rolls a segment
    /// a little faster than a barrier interval plus a pass.
    fn default() -> Self {
        Self {
            segment_size_bytes: ourios_wal::MIN_SEGMENT_SIZE_BYTES,
            segment_age_secs: 600,
            barrier_secs: 300,
            housekeeping_secs: 60,
            secs_per_batch: 20,
            records_per_batch: 4,
            record_bytes: 256 * 1024,
            tenants: 3,
        }
    }
}

/// The most the WAL may hold at any instant of a capacity-balanced run
/// under a complete floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct GrowthBound {
    /// Closed segments still inside their reclamation window, plus the
    /// open one.
    pub segments: u32,
    /// `segments` full segments.
    pub segment_bytes: u64,
}

impl ReclaimSoakConfig {
    /// The largest frame one batch can append.
    #[must_use]
    pub fn frame_ceiling_bytes(&self) -> u64 {
        to_u64(self.records_per_batch)
            .saturating_mul(to_u64(self.record_bytes).saturating_add(RECORD_OVERHEAD_BYTES))
            .saturating_add(FRAME_OVERHEAD_BYTES)
    }

    /// The fewest frames a closed segment holds: a segment rotates on
    /// size only when the next frame would not fit. Zero when a frame
    /// at the ceiling would not fit at all, which [`Self::validate`]
    /// refuses.
    #[must_use]
    pub fn fewest_frames_per_segment(&self) -> u64 {
        self.segment_size_bytes
            .saturating_sub(segment_header_bytes())
            / self.frame_ceiling_bytes()
    }

    /// The shortest synthetic time between two size rotations; non-zero
    /// on a config [`Self::validate`] accepts.
    fn segment_period_secs(&self) -> u64 {
        self.fewest_frames_per_segment()
            .saturating_mul(self.secs_per_batch)
    }

    /// The bound RFC0052.3 holds a size-rotated run to.
    ///
    /// A closed segment is covered by the first barrier tick after it
    /// closes — within `barrier_secs` — and unlinked by the first pass
    /// after that, within `housekeeping_secs` more. Size rotations are at
    /// least `fewest_frames_per_segment × secs_per_batch` apart, so at most
    /// `⌈(barrier_secs + housekeeping_secs) / period⌉ + 1` closed segments
    /// are inside that window at once, beside the open one.
    ///
    /// That holds only while one pass can unlink every segment the
    /// window closes — the rate is within the pass cap — so the bound
    /// exists only for a config [`Self::validate`] accepts.
    ///
    /// # Errors
    ///
    /// As [`Self::validate`].
    pub fn growth_bound(&self) -> Result<GrowthBound, ReclaimSoakError> {
        self.validate()?;
        let segments = self.closed_per_window().saturating_add(1);
        Ok(GrowthBound {
            segments: u32::try_from(segments).unwrap_or(u32::MAX),
            segment_bytes: segments.saturating_mul(self.segment_size_bytes),
        })
    }

    /// The most data objects one cut may write (§3.1): one per partition
    /// an interval touches — each tenant's hour partitions — for the
    /// barrier's own drain, and again for each rotation-hook cut the
    /// pending slot coalesced into it.
    ///
    /// # Errors
    ///
    /// As [`Self::validate`].
    pub fn objects_per_cut_bound(&self) -> Result<u64, ReclaimSoakError> {
        self.validate()?;
        let hours = self.barrier_secs.div_ceil(HOUR_SECS).saturating_add(1);
        let drains = self
            .barrier_secs
            .div_ceil(self.segment_period_secs())
            .saturating_add(1);
        Ok(to_u64(self.tenants)
            .saturating_mul(hours)
            .saturating_mul(drains))
    }

    /// Refuse a config no bound can be derived for: a zero step or
    /// cadence — with no step, time never reaches a tick — a frame that
    /// does not fit a segment, or a rate reclamation cannot keep up with.
    ///
    /// # Errors
    ///
    /// The [`ReclaimSoakError`] naming the first knob at fault.
    pub fn validate(&self) -> Result<(), ReclaimSoakError> {
        match self {
            config if config.secs_per_batch == 0 => Err(ReclaimSoakError::ZeroStep),
            config if config.barrier_secs == 0 => Err(ReclaimSoakError::ZeroBarrierSecs),
            config if config.housekeeping_secs == 0 => Err(ReclaimSoakError::ZeroHousekeepingSecs),
            config if config.fewest_frames_per_segment() == 0 => {
                Err(ReclaimSoakError::FrameOverSegment {
                    frame_ceiling_bytes: config.frame_ceiling_bytes(),
                    segment_size_bytes: config.segment_size_bytes,
                })
            }
            config => config.within_capacity(),
        }
    }

    /// The closed segments one barrier-plus-pass window can hold must fit
    /// in one pass's unlink cap, or the backlog outgrows the bound by
    /// construction — RFC0052.3 claims nothing above capacity.
    fn within_capacity(&self) -> Result<(), ReclaimSoakError> {
        let closed = self.closed_per_window();
        let cap = u64::from(MAX_UNLINKS_PER_PASS);
        match closed {
            closed if closed <= cap => Ok(()),
            closed => Err(ReclaimSoakError::OverCapacity {
                closed_per_window: closed,
                max_unlinks_per_pass: cap,
            }),
        }
    }

    /// The most closed segments inside one barrier-plus-pass window.
    fn closed_per_window(&self) -> u64 {
        self.barrier_secs
            .saturating_add(self.housekeeping_secs)
            .div_ceil(self.segment_period_secs())
            .saturating_add(1)
    }

    fn wal_config(&self, root: PathBuf) -> WalConfig {
        WalConfig {
            root,
            batch_window_ms: BATCH_WINDOW_MS,
            segment_size_bytes: self.segment_size_bytes,
            segment_age_secs: self.segment_age_secs,
            housekeeping_secs: self.housekeeping_secs,
            max_unlinks_per_pass: MAX_UNLINKS_PER_PASS,
            rotation_retry_attempts: ourios_wal::DEFAULT_ROTATION_RETRY_ATTEMPTS,
            macos_full_fsync: false,
        }
    }
}

/// What a sample was taken after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    Append,
    Barrier,
    Housekeeping,
}

/// §3.2's two timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Timer {
    Barrier,
    Housekeeping,
}

impl From<Timer> for Tick {
    fn from(timer: Timer) -> Self {
        match timer {
            Timer::Barrier => Self::Barrier,
            Timer::Housekeeping => Self::Housekeeping,
        }
    }
}

/// The WAL's state as one step of the run left it.
#[derive(Debug, Clone, Copy)]
pub struct ReclaimSample {
    pub synthetic_secs: u64,
    pub after: Tick,
    pub segment_count: u32,
    pub disk_bytes: u64,
    /// The `*.wal` files alone, so the bound is not charged for the
    /// sidecars.
    pub segment_bytes: u64,
    pub unreclaimed_bytes: u64,
    pub checkpoint: Option<WalOffset>,
    pub floor: RetainFloor,
}

/// One barrier tick's decision and the data objects its cut wrote.
#[derive(Debug, Clone, Copy)]
pub struct CutRecord {
    pub synthetic_secs: u64,
    pub outcome: CutOutcome,
    pub objects_written: u64,
}

/// What one housekeeping tick came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassResult {
    Completed {
        removed_segments: usize,
        outcome: PassOutcome,
    },
    /// The pass returned an error; its `error.type`.
    Failed(&'static str),
    Panicked,
}

/// One housekeeping tick.
#[derive(Debug, Clone, Copy)]
pub struct PassRecord {
    pub synthetic_secs: u64,
    pub result: PassResult,
}

/// The open segment, and when the harness first saw it — on both clocks.
#[derive(Debug, Clone)]
struct OpenSegment {
    name: String,
    synthetic_secs: u64,
    wall: Instant,
}

/// A failure that stops the run.
#[derive(Debug)]
pub enum ReclaimSoakError {
    Setup(String),
    Ingest(ReceiveError),
    /// A batch acked without the pipeline recording a mark.
    NoMark,
    /// A tick's blocking task was cancelled or aborted.
    TickAborted(&'static str),
    /// `secs_per_batch` is zero: the clock would never reach a tick.
    ZeroStep,
    ZeroBarrierSecs,
    ZeroHousekeepingSecs,
    /// A frame at the config's ceiling does not fit one segment.
    FrameOverSegment {
        frame_ceiling_bytes: u64,
        segment_size_bytes: u64,
    },
    /// The config offers segments faster than one pass can unlink them.
    OverCapacity {
        closed_per_window: u64,
        max_unlinks_per_pass: u64,
    },
}

impl std::fmt::Display for ReclaimSoakError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Setup(detail) => write!(f, "reclaim soak setup: {detail}"),
            Self::Ingest(e) => write!(f, "reclaim soak ingest: {e}"),
            Self::NoMark => write!(f, "reclaim soak: an acked batch left no mark"),
            Self::TickAborted(which) => write!(f, "reclaim soak: the {which} tick aborted"),
            Self::ZeroStep => write!(f, "reclaim soak config: secs_per_batch must be > 0"),
            Self::ZeroBarrierSecs => write!(f, "reclaim soak config: barrier_secs must be > 0"),
            Self::ZeroHousekeepingSecs => {
                write!(f, "reclaim soak config: housekeeping_secs must be > 0")
            }
            Self::FrameOverSegment {
                frame_ceiling_bytes,
                segment_size_bytes,
            } => write!(
                f,
                "reclaim soak config: a {frame_ceiling_bytes} B frame does not fit a \
                 {segment_size_bytes} B segment"
            ),
            Self::OverCapacity {
                closed_per_window,
                max_unlinks_per_pass,
            } => write!(
                f,
                "reclaim soak config: {closed_per_window} segments can close in one barrier-plus-\
                 pass window, more than the {max_unlinks_per_pass} one pass unlinks"
            ),
        }
    }
}

impl std::error::Error for ReclaimSoakError {}

/// The receiver's reclamation pieces, wired as `serve` wires them.
struct Node {
    commits: Arc<CommitCoordinator>,
    pipeline: SharedPipeline,
    barrier: Arc<Barrier>,
    housekeeper: Arc<Housekeeper>,
}

/// §3.2's two timers on the synthetic clock. The barrier skips its
/// immediate first tick and housekeeping does not, as the receiver's
/// tasks do.
#[derive(Debug)]
struct Schedule {
    barrier_secs: u64,
    housekeeping_secs: u64,
    next_barrier: u64,
    next_housekeeping: u64,
}

impl Schedule {
    fn new(config: &ReclaimSoakConfig) -> Self {
        Self {
            barrier_secs: config.barrier_secs,
            housekeeping_secs: config.housekeeping_secs,
            next_barrier: config.barrier_secs,
            next_housekeeping: 0,
        }
    }

    /// The earliest tick due at or before `until`, housekeeping first on
    /// a tie.
    fn pop_due(&mut self, until: u64) -> Option<(u64, Timer)> {
        match (self.next_housekeeping, self.next_barrier) {
            (pass, cut) if pass <= cut && pass <= until => {
                self.next_housekeeping = pass.saturating_add(self.housekeeping_secs);
                Some((pass, Timer::Housekeeping))
            }
            (pass, cut) if cut < pass && cut <= until => {
                self.next_barrier = cut.saturating_add(self.barrier_secs);
                Some((cut, Timer::Barrier))
            }
            _ => None,
        }
    }
}

/// Segment rolls and frame sizes, read off the acknowledged offsets.
#[derive(Debug, Default)]
struct FrameLog {
    last: Option<WalOffset>,
    segments: u64,
    largest_frame: u64,
}

impl FrameLog {
    fn observe(&mut self, offset: WalOffset) {
        match self.last {
            Some(last) if last.segment == offset.segment => {
                self.largest_frame = self
                    .largest_frame
                    .max(offset.byte.saturating_sub(last.byte));
            }
            _ => {
                self.segments += 1;
                self.largest_frame = self
                    .largest_frame
                    .max(offset.byte.saturating_sub(segment_header_bytes()));
            }
        }
        self.last = Some(offset);
    }
}

/// A running reclamation soak over a directory the caller owns.
pub struct ReclaimSoak {
    config: ReclaimSoakConfig,
    node: Node,
    wal_root: PathBuf,
    data_root: PathBuf,
    now_secs: u64,
    schedule: Schedule,
    batches: u64,
    frames: FrameLog,
    open_segment: Option<OpenSegment>,
    samples: Vec<ReclaimSample>,
    cuts: Vec<CutRecord>,
    passes: Vec<PassRecord>,
}

impl ReclaimSoak {
    /// Open a fresh WAL and stores under `root` and wire the node.
    ///
    /// # Errors
    ///
    /// [`ReclaimSoakError::Setup`] when a directory, the WAL or a store
    /// cannot be opened.
    pub fn open(root: &Path, config: ReclaimSoakConfig) -> Result<Self, ReclaimSoakError> {
        let wal_root = root.join("wal");
        let data_root = root.join("data");
        let audit_root = root.join("audit");
        for dir in [&wal_root, &data_root, &audit_root] {
            std::fs::create_dir_all(dir)
                .map_err(|e| ReclaimSoakError::Setup(format!("create {}: {e}", dir.display())))?;
        }
        config.validate()?;
        let wal = Wal::open(config.wal_config(wal_root.clone()))
            .map_err(|e| ReclaimSoakError::Setup(format!("open WAL: {e:?}")))?;
        let sinks = sinks(&data_root, &audit_root)?;
        let node = wire(wal, sinks, &wal_root, &config);
        Ok(Self {
            schedule: Schedule::new(&config),
            config,
            node,
            wal_root,
            data_root,
            now_secs: 0,
            batches: 0,
            frames: FrameLog::default(),
            open_segment: None,
            samples: Vec::new(),
            cuts: Vec::new(),
            passes: Vec::new(),
        })
    }

    /// `batches` appends, one step apart, with every tick due between
    /// them.
    ///
    /// # Errors
    ///
    /// As [`Self::ingest`] and [`Self::advance`].
    pub async fn run_load(&mut self, batches: u64) -> Result<(), ReclaimSoakError> {
        for _ in 0..batches {
            self.ingest().await?;
            self.advance(self.config.secs_per_batch).await?;
        }
        Ok(())
    }

    /// Run the ticks due now, then append one batch at the current
    /// synthetic instant and wait for its ack.
    ///
    /// # Errors
    ///
    /// [`ReclaimSoakError::Ingest`] when the batch is refused.
    pub async fn ingest(&mut self) -> Result<WalOffset, ReclaimSoakError> {
        self.run_due(self.now_secs).await?;
        let tenant = tenant_name(self.batches, self.config.tenants).into_owned();
        let records = bulk_records(&self.config, self.batches, self.now_unix_nanos());
        self.node
            .pipeline
            .ingest(
                export_request(tenant.clone(), records),
                TenantId::new(tenant),
            )
            .await
            .map_err(ReclaimSoakError::Ingest)?;
        let offset = self
            .node
            .pipeline
            .acknowledged_durable()
            .ok_or(ReclaimSoakError::NoMark)?;
        self.frames.observe(offset);
        self.batches += 1;
        self.sample(Tick::Append);
        Ok(offset)
    }

    /// Move the clock `secs` forward with no traffic, running every tick
    /// due on the way.
    ///
    /// # Errors
    ///
    /// [`ReclaimSoakError::TickAborted`] when a tick's task does not
    /// return.
    pub async fn advance(&mut self, secs: u64) -> Result<(), ReclaimSoakError> {
        let target = self.now_secs.saturating_add(secs);
        self.run_due(target).await?;
        self.now_secs = target;
        Ok(())
    }

    #[must_use]
    pub fn config(&self) -> &ReclaimSoakConfig {
        &self.config
    }

    #[must_use]
    pub fn now_secs(&self) -> u64 {
        self.now_secs
    }

    #[must_use]
    pub fn state(&self) -> ReclaimState {
        self.node.commits.reclaim_state()
    }

    #[must_use]
    pub fn samples(&self) -> &[ReclaimSample] {
        &self.samples
    }

    #[must_use]
    pub fn cuts(&self) -> &[CutRecord] {
        &self.cuts
    }

    #[must_use]
    pub fn passes(&self) -> &[PassRecord] {
        &self.passes
    }

    /// Distinct segments the acknowledged appends landed in.
    #[must_use]
    pub fn segments_rolled(&self) -> u64 {
        self.frames.segments
    }

    /// The largest frame measured between two appends in one segment.
    #[must_use]
    pub fn largest_frame_bytes(&self) -> u64 {
        self.frames.largest_frame
    }

    /// Whether the segment `offset` lies in is still on disk.
    #[must_use]
    pub fn segment_on_disk(&self, offset: WalOffset) -> bool {
        self.wal_root
            .join(format!("{}.wal", offset.segment))
            .exists()
    }

    async fn run_due(&mut self, until: u64) -> Result<(), ReclaimSoakError> {
        while let Some((at, timer)) = self.schedule.pop_due(until) {
            self.now_secs = at;
            match timer {
                Timer::Barrier => self.barrier_tick().await?,
                Timer::Housekeeping => self.housekeeping_tick().await?,
            }
            self.sample(timer.into());
        }
        Ok(())
    }

    async fn barrier_tick(&mut self) -> Result<(), ReclaimSoakError> {
        self.await_segment_age().await;
        let before = count_objects(&self.data_root);
        let (barrier, pipeline) = (
            Arc::clone(&self.node.barrier),
            Arc::clone(&self.node.pipeline),
        );
        let outcome = tokio::task::spawn_blocking(move || barrier.tick(&pipeline, true))
            .await
            .map_err(|_| ReclaimSoakError::TickAborted("barrier"))?;
        self.cuts.push(CutRecord {
            synthetic_secs: self.now_secs,
            outcome,
            objects_written: to_u64(count_objects(&self.data_root).saturating_sub(before)),
        });
        Ok(())
    }

    async fn housekeeping_tick(&mut self) -> Result<(), ReclaimSoakError> {
        let housekeeper = Arc::clone(&self.node.housekeeper);
        let tick = tokio::task::spawn_blocking(move || housekeeper.tick())
            .await
            .map_err(|_| ReclaimSoakError::TickAborted("housekeeping"))?;
        let result = match tick {
            HousekeepingTick::Completed(progress) => PassResult::Completed {
                removed_segments: progress.removed_segments,
                outcome: progress.outcome,
            },
            HousekeepingTick::Failed(e) => PassResult::Failed(e.error_type()),
            HousekeepingTick::Panicked => PassResult::Panicked,
        };
        self.passes.push(PassRecord {
            synthetic_secs: self.now_secs,
            result,
        });
        Ok(())
    }

    /// Hold a barrier tick the synthetic clock places past the open
    /// segment's age until the wall clock is past it too. The segment was
    /// minted no later than the harness first saw it, so waiting from
    /// that sighting never undershoots the WAL's own reading.
    async fn await_segment_age(&self) {
        let Some(open) = &self.open_segment else {
            return;
        };
        let aged_at = open
            .synthetic_secs
            .saturating_add(self.config.segment_age_secs);
        if self.now_secs < aged_at {
            return;
        }
        let deadline = open.wall + Duration::from_secs(self.config.segment_age_secs) + AGE_MARGIN;
        tokio::time::sleep_until(deadline.into()).await;
    }

    /// Segments are created only inside an append or a barrier tick, and
    /// this runs after each, so a new segment is dated to the step that
    /// made it. `UUIDv7` names sort by mint time, so the newest is open.
    fn note_open_segment(&mut self) {
        let Some(name) = newest_segment(&self.wal_root) else {
            return;
        };
        if self
            .open_segment
            .as_ref()
            .is_some_and(|open| open.name == name)
        {
            return;
        }
        self.open_segment = Some(OpenSegment {
            name,
            synthetic_secs: self.now_secs,
            wall: Instant::now(),
        });
    }

    fn sample(&mut self, after: Tick) {
        self.note_open_segment();
        let state = self.node.commits.reclaim_state();
        self.samples.push(ReclaimSample {
            synthetic_secs: self.now_secs,
            after,
            segment_count: state.segment_count,
            disk_bytes: state.disk_bytes,
            segment_bytes: segment_bytes(&self.wal_root),
            unreclaimed_bytes: state.unreclaimed_bytes,
            checkpoint: state.checkpoint,
            floor: state.floor,
        });
    }

    fn now_unix_nanos(&self) -> u64 {
        BASE_UNIX_NANOS.saturating_add(self.now_secs.saturating_mul(NANOS_PER_SEC))
    }
}

fn sinks(
    data_root: &Path,
    audit_root: &Path,
) -> Result<(SharedParquetSink, SharedParquetAuditSink), ReclaimSoakError> {
    let open = |root: &Path| {
        Store::local(root).map_err(|e| ReclaimSoakError::Setup(format!("open store: {e}")))
    };
    let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(
        open(audit_root)?,
        AUDIT_CEILING_EVENTS,
    ));
    let settled = audit.clone();
    // Nothing flushes on its own, so every object the data store gains is
    // one a cut wrote.
    let flush = FlushConfig {
        target_bytes: usize::MAX,
        max_buffer_age: Duration::from_secs(86_400),
        ceiling_bytes: SINK_CEILING_BYTES,
    };
    let sink = SharedParquetSink::new(
        ParquetRecordSink::new(open(data_root)?, flush)
            .with_audit_barrier(Box::new(move || settled.settled())),
    );
    Ok((sink, audit))
}

fn wire(
    wal: Wal,
    (sink, audit): (SharedParquetSink, SharedParquetAuditSink),
    wal_root: &Path,
    config: &ReclaimSoakConfig,
) -> Node {
    let commits = CommitCoordinator::new(
        Box::new(wal),
        Duration::from_millis(BATCH_WINDOW_MS),
        config.segment_size_bytes,
    );
    let publish = PublishCoordinator::new(sink.clone(), audit.clone());
    let barrier = Arc::new(Barrier::new(
        publish.clone(),
        Arc::clone(&commits),
        wal_root.join("snapshots"),
        SINK_CEILING_BYTES,
    ));
    let miner = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(audit))
        .with_record_sink(Box::new(sink));
    let hook = Arc::clone(&barrier);
    let pipeline = Arc::new(
        IngestPipeline::new(Arc::clone(&commits), miner)
            .with_encode_pool(EncodePool::with_publisher(
                publish.publisher(),
                ENCODE_WORKERS,
            ))
            .with_rotation_hook(Box::new(move |miner, mark| {
                hook.capture_rotation(miner, mark);
            })),
    );
    let max_unlinks = usize::try_from(MAX_UNLINKS_PER_PASS).unwrap_or(1);
    let housekeeper = Arc::new(Housekeeper::new(
        Arc::clone(&commits),
        Arc::clone(&barrier),
        publish,
        max_unlinks,
    ));
    Node {
        commits,
        pipeline,
        barrier,
        housekeeper,
    }
}

/// `records_per_batch` records whose bodies carry `record_bytes` of
/// filler behind a short varying prefix, so the frame is the size the
/// config says while the miner sees one template.
fn bulk_records(config: &ReclaimSoakConfig, batch: u64, ts_unix_nanos: u64) -> Vec<LogRecord> {
    let filler = "x".repeat(config.record_bytes);
    (0..config.records_per_batch)
        .map(|i| LogRecord {
            time_unix_nano: ts_unix_nanos.saturating_add(to_u64(i)),
            severity_number: 9,
            severity_text: "INFO".to_string(),
            body: Some(AnyValue {
                value: Some(any_value::Value::StringValue(format!(
                    "bulk batch {batch} record {i} {filler}"
                ))),
            }),
            ..LogRecord::default()
        })
        .collect()
}

/// The data store's Parquet objects.
fn count_objects(root: &Path) -> usize {
    walk_files(root)
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "parquet"))
        .count()
}

fn segment_header_bytes() -> u64 {
    to_u64(ourios_wal::SEGMENT_HEADER_LEN)
}

/// The newest `*.wal` segment's name.
fn newest_segment(wal_root: &Path) -> Option<String> {
    std::fs::read_dir(wal_root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "wal"))
        .filter_map(|path| path.file_stem()?.to_str().map(str::to_owned))
        .max()
}

/// The summed length of the WAL's `*.wal` segment files.
fn segment_bytes(wal_root: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(wal_root) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "wal"))
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| meta.len())
        .fold(0, u64::saturating_add)
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn housekeeping_wins_a_tie_and_the_barrier_skips_its_first_tick() {
        let mut schedule = Schedule::new(&ReclaimSoakConfig {
            barrier_secs: 120,
            housekeeping_secs: 60,
            ..ReclaimSoakConfig::default()
        });
        let mut due = Vec::new();
        while let Some(tick) = schedule.pop_due(240) {
            due.push(tick);
        }
        assert_eq!(
            due,
            [
                (0, Timer::Housekeeping),
                (60, Timer::Housekeeping),
                (120, Timer::Housekeeping),
                (120, Timer::Barrier),
                (180, Timer::Housekeeping),
                (240, Timer::Housekeeping),
                (240, Timer::Barrier),
            ]
        );
    }

    #[test]
    fn zero_steps_and_cadences_are_refused_before_any_bound() {
        let refused = |config: ReclaimSoakConfig| {
            let refusal = config.validate().expect_err("refused");
            assert_eq!(
                format!("{refusal}"),
                format!("{}", config.growth_bound().expect_err("no bound")),
            );
            refusal
        };
        assert!(matches!(
            refused(ReclaimSoakConfig {
                secs_per_batch: 0,
                ..ReclaimSoakConfig::default()
            }),
            ReclaimSoakError::ZeroStep
        ));
        assert!(matches!(
            refused(ReclaimSoakConfig {
                barrier_secs: 0,
                ..ReclaimSoakConfig::default()
            }),
            ReclaimSoakError::ZeroBarrierSecs
        ));
        assert!(matches!(
            refused(ReclaimSoakConfig {
                housekeeping_secs: 0,
                ..ReclaimSoakConfig::default()
            }),
            ReclaimSoakError::ZeroHousekeepingSecs
        ));
        assert!(matches!(
            refused(ReclaimSoakConfig {
                record_bytes: 64 * 1024 * 1024,
                ..ReclaimSoakConfig::default()
            }),
            ReclaimSoakError::FrameOverSegment { .. }
        ));
        assert!(ReclaimSoakConfig::default().validate().is_ok());
    }

    #[test]
    fn a_rate_one_pass_cannot_clear_is_refused() {
        let config = ReclaimSoakConfig {
            barrier_secs: 3_600,
            secs_per_batch: 1,
            ..ReclaimSoakConfig::default()
        };
        // 16 frames a segment, one a second: 3,660 s holds 229 + 1 closes.
        assert!(matches!(
            config.validate(),
            Err(ReclaimSoakError::OverCapacity {
                closed_per_window: 230,
                max_unlinks_per_pass: 128,
            })
        ));
        let dir = tempfile::TempDir::new().expect("temp dir");
        assert!(matches!(
            ReclaimSoak::open(dir.path(), config),
            Err(ReclaimSoakError::OverCapacity { .. })
        ));
        assert!(ReclaimSoakConfig::default().validate().is_ok());
    }

    #[test]
    fn a_segments_first_frame_counts_toward_the_largest() {
        let (first, second) = (uuid::Uuid::from_u128(1), uuid::Uuid::from_u128(2));
        let at = |segment, byte| WalOffset { segment, byte };
        let header = segment_header_bytes();
        let mut log = FrameLog::default();
        log.observe(at(first, header + 100));
        log.observe(at(first, header + 150));
        log.observe(at(second, header + 900));
        log.observe(at(second, header + 1_000));
        assert_eq!(log.largest_frame, 900, "the second segment's first frame");
        assert_eq!(log.segments, 2);
    }

    #[test]
    fn the_bound_counts_the_window_in_segment_periods() {
        // 16 frames of ≤ 1 MiB + 5 KiB per 17 MiB segment, 20 s apart:
        // a segment closes at most every 320 s, so a 360 s window holds
        // two closes, one more for the window's edge, plus the open one.
        let config = ReclaimSoakConfig::default();
        assert_eq!(config.fewest_frames_per_segment(), 16);
        assert_eq!(
            config.growth_bound().expect("the default config is valid"),
            GrowthBound {
                segments: 4,
                segment_bytes: 4 * ourios_wal::MIN_SEGMENT_SIZE_BYTES,
            }
        );
    }
}
