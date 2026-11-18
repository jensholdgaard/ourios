//! A tail replica's part in its group's publication (RFC 0065 §6.5,
//! §6.7). Every tail replica keeps its mined output for a takeover; the
//! one the latest flusher assignment names also publishes it.
//!
//! Named, it reads the group's log id from `formed.json` (§6.1) and
//! publishes everything it keeps, then each later step's output. Without a
//! readable record it publishes nothing and tries again at the next step.
//! An assignment naming another replica stops it at that entry: what it
//! has not published stays kept, as on every tail replica. A named
//! replica with a gap publishes nothing it keeps until it has rebuilt what
//! the gap dropped, by a replay of the group's log; with a checkpoint applied that needs the
//! checkpoint's bundle, which it holds for. Once no gap remains it
//! publishes everything it keeps again, and a row a former publication or
//! the replay already wrote repeats only its position, which compaction
//! and queries count once (§6.6).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ourios_core::audit::AuditEvent;
use ourios_parquet::{PromotedAttributes, Store};
use ourios_semconv as semconv;

use super::formation::{Formation, formed_key};
use super::kept::Kept;
use super::mining::{Mined, MinedRow, SharedLog};
use super::publication::Publication;
use super::telemetry::Replays;
use super::types::NodeId;
use crate::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use crate::record_sink::{FlushConfig, ParquetRecordSink};
use rebuild::{Rebuild, Slice};
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

/// Where and how a group's flusher publishes.
#[derive(Clone, Debug)]
pub struct Source {
    pub store: Store,
    /// The group's voters in placement order, which `formed.json` names.
    pub voters: Vec<String>,
    pub flush: FlushConfig,
    pub promoted: PromotedAttributes,
    /// The audit sink's soft ceiling, in events.
    pub audit_ceiling: usize,
    /// The RFC 0047 §3.3 graph the group's published rows feed, when the
    /// graph is configured.
    #[cfg(feature = "openfga")]
    pub graph_emitter: Option<std::sync::Arc<crate::graph_emitter::GraphEmitter>>,
}

/// How a tail node publishes the groups it is named flusher of.
#[derive(Clone, Debug)]
pub struct Publishing {
    pub store: Store,
    /// Each group's voters in placement order, which its `formed.json`
    /// names.
    pub voters: BTreeMap<u32, Vec<String>>,
    pub flush: FlushConfig,
    pub promoted: PromotedAttributes,
    /// The audit sink's soft ceiling, in events.
    pub audit_ceiling: usize,
    /// The RFC 0047 §3.3 graph the groups' published rows feed, when the
    /// graph is configured.
    #[cfg(feature = "openfga")]
    pub graph_emitter: Option<std::sync::Arc<crate::graph_emitter::GraphEmitter>>,
}

/// The age sweep's most: a partition flushes within its age plus this
/// (RFC 0063 §3.2, as the local receiver's sweep).
const SWEEP_TICK: Duration = Duration::from_secs(30);

/// The barrier's least, as RFC 0052 §3.1 validates `barrier_secs`.
const BARRIER_FLOOR: Duration = Duration::from_secs(1);

/// The age sweep's least, so an age of zero does not spin.
const SWEEP_FLOOR: Duration = Duration::from_millis(10);

impl Publishing {
    /// `group`'s source, if this node knows its voters.
    #[must_use]
    pub fn source(&self, group: u32) -> Option<Source> {
        Some(Source {
            store: self.store.clone(),
            voters: self.voters.get(&group)?.clone(),
            flush: self.flush.clone(),
            promoted: self.promoted.clone(),
            audit_ceiling: self.audit_ceiling,
            #[cfg(feature = "openfga")]
            graph_emitter: self.graph_emitter.clone(),
        })
    }

    /// How often a group's flusher checkpoints: RFC 0052's barrier runs
    /// at the sink's age trigger, here at least once a second.
    #[must_use]
    pub fn barrier(&self) -> Duration {
        self.flush.max_buffer_age.max(BARRIER_FLOOR)
    }

    /// How often a group's age sweep runs: at most 30 s and
    /// never slower than the age it enforces.
    #[must_use]
    pub fn sweep(&self) -> Duration {
        SWEEP_TICK.min(self.flush.max_buffer_age).max(SWEEP_FLOOR)
    }
}

/// What one step publishes.
pub enum Need {
    /// Nothing: stop, if publishing.
    Hold,
    /// The publication is to be built first: see [`Flushing::prepare`].
    Prepare,
    /// The step's own output.
    Output,
    /// Everything kept, the step's output among it.
    Backlog(Backlog),
    /// The next slice of the replay through the gap.
    Replay { gap: u64 },
}

/// Where a named replica with a gap is in rebuilding it.
enum Rebuilding {
    Replaying(Box<Rebuild>),
    /// Held, as reported, until the gap clears; with the start of a
    /// replay a checkpoint overtook, which ends once the gap clears.
    Held(Option<Instant>),
}

/// What a replica keeps, copied for publishing once it is no longer held.
pub struct Backlog {
    audit: Vec<AuditEvent>,
    rows: Vec<MinedRow>,
}

/// Publish what the age trigger has made due, every `every` or as soon as
/// the live publication's audit buffer reaches its ceiling, until `stop`
/// says so. A flush that fails leaves its rows buffered for the next one,
/// and its failure is counted where it happens.
pub async fn sweep(mut current: Current, every: Duration, mut stop: watch::Receiver<bool>) {
    let mut ticks = tokio::time::interval(every);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        match wake(&mut current, &mut ticks, &mut stop).await {
            Wake::Stop => return,
            // A new publication has a notifier of its own to wait on.
            Wake::Changed => {}
            Wake::Flush => {
                if !flush_aged(&current).await {
                    return;
                }
            }
        }
    }
}

/// Why the sweep woke.
enum Wake {
    Stop,
    Changed,
    Flush,
}

async fn wake(
    current: &mut Current,
    ticks: &mut tokio::time::Interval,
    stop: &mut watch::Receiver<bool>,
) -> Wake {
    let overflow = current.borrow().as_ref().map(|p| p.overflow());
    let overflowed = async {
        match &overflow {
            Some(notify) => notify.notified().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        _ = stop.wait_for(|stopped| *stopped) => Wake::Stop,
        changed = current.changed() => match changed {
            Ok(()) => Wake::Changed,
            Err(_) => Wake::Stop,
        },
        _ = ticks.tick() => Wake::Flush,
        () = overflowed => Wake::Flush,
    }
}

/// Run the age flush of the live publication, if any, off the runtime's
/// workers; `false` if it panicked, which is reported where it happened.
async fn flush_aged(current: &Current) -> bool {
    let Some(publication) = current.borrow().clone() else {
        return true;
    };
    tokio::task::spawn_blocking(move || publication.flush_aged())
        .await
        .is_ok()
}

/// The publication a group's age loop drives, while there is one.
pub type Current = watch::Receiver<Option<Arc<Publication>>>;

/// One group's publication on one tail replica.
pub struct Flushing {
    replica: NodeId,
    group: u32,
    source: Source,
    named: bool,
    publication: Option<Arc<Publication>>,
    /// Everything kept has been handed to `publication`.
    caught_up: bool,
    /// `publication` takes handoffs: it was resumed, for the backlog or a
    /// replay, and not discarded since.
    active: bool,
    /// Why it last held, as reported; `None` once publishing.
    held: Option<&'static str>,
    log: SharedLog,
    rebuilding: Option<Rebuilding>,
    replays: Replays,
    /// How many times the kept backlog was copied for publication.
    #[cfg(test)]
    backlog_copies: usize,
    current: watch::Sender<Option<Arc<Publication>>>,
}

impl Flushing {
    #[must_use]
    pub fn new(replica: NodeId, group: u32, source: Source, log: SharedLog) -> Self {
        Self {
            replica,
            group,
            source,
            named: false,
            publication: None,
            caught_up: false,
            active: false,
            held: None,
            log,
            rebuilding: None,
            replays: Replays::global(),
            #[cfg(test)]
            backlog_copies: 0,
            current: watch::Sender::new(None),
        }
    }

    /// Record its gap replays' durations on `replays`.
    #[must_use]
    pub(crate) fn with_replays(self, replays: Replays) -> Self {
        Self { replays, ..self }
    }

    /// The publication to drive the age trigger on.
    #[must_use]
    pub fn current(&self) -> Current {
        self.current.subscribe()
    }

    /// The publication a checkpoint publishes through, while this replica
    /// is named and has handed it everything `kept` holds, with no gap to
    /// rebuild: only then are all the rows through its miner's last entry
    /// published or buffered there (RFC 0065 §6.5).
    #[must_use]
    pub fn checkpointable(&self, kept: &Kept) -> Option<Arc<Publication>> {
        let publishing_all = self.named
            && self.active
            && self.caught_up
            && self.rebuilding.is_none()
            && kept.gap().is_none();
        publishing_all.then(|| self.publication.clone()).flatten()
    }

    /// Why a named replica is not publishing, as the `error.type` it last
    /// reported; `None` when nothing holds it.
    #[cfg(test)]
    fn held(&self) -> Option<&'static str> {
        self.held
    }

    /// One step: `assigned` is the replica a flusher assignment applied at
    /// this entry named, `out` what the entry mined, already in `kept`.
    pub fn step(&mut self, assigned: Option<NodeId>, out: &Mined, kept: &mut Kept) {
        let mut assigned = assigned;
        loop {
            let need = match self.plan(assigned.take(), kept) {
                Need::Prepare if self.prepare() => self.plan(None, kept),
                Need::Prepare => return,
                need => need,
            };
            let Some(through) = self.publish(need, out) else {
                return;
            };
            kept.replayed(through);
        }
    }

    /// Decide what this step publishes, while `kept`, which already holds
    /// the step's output, is held; it does no I/O, so whoever holds `kept`
    /// is not kept waiting on the store.
    pub fn plan(&mut self, assigned: Option<NodeId>, kept: &Kept) -> Need {
        if let Some(assigned) = assigned {
            self.named = assigned == self.replica;
        }
        match kept.gap() {
            _ if !self.named => {
                self.end_rebuild(false);
                Need::Hold
            }
            Some(gap) => self.plan_rebuild(gap, kept.cut()),
            None => {
                self.end_rebuild(true);
                self.plan_publish(kept)
            }
        }
    }

    fn plan_publish(&mut self, kept: &Kept) -> Need {
        if self.caught_up {
            Need::Output
        } else if self.publication.is_none() {
            Need::Prepare
        } else {
            #[cfg(test)]
            {
                self.backlog_copies += 1;
            }
            Need::Backlog(Backlog {
                audit: kept.audit().cloned().collect(),
                rows: kept.rows().cloned().collect(),
            })
        }
    }

    /// A gap through `gap` above the last cut, `cut`: replay it from the
    /// first entry while no checkpoint has passed the replay, or hold for
    /// the checkpoint's bundle.
    fn plan_rebuild(&mut self, gap: u64, cut: u64) -> Need {
        let passed = match &self.rebuilding {
            Some(Rebuilding::Held(_)) => return Need::Hold,
            Some(Rebuilding::Replaying(rebuild)) => Some(cut == 0 || rebuild.passed(cut)),
            None => None,
        };
        match passed {
            Some(true) => Need::Replay { gap },
            None if cut == 0 && self.publication.is_none() => Need::Prepare,
            None if cut == 0 => Need::Replay { gap },
            Some(false) | None => {
                let overtaken = match self.rebuilding.take() {
                    Some(Rebuilding::Replaying(rebuild)) => Some(rebuild.started()),
                    _ => None,
                };
                self.rebuilding = Some(Rebuilding::Held(overtaken));
                let why = format!(
                    "the checkpoint at {cut} is below its gap at {gap}, and it cannot yet \
                     restore the checkpoint's bundle to replay from"
                );
                self.report_held("restore_unavailable", &why);
                Need::Hold
            }
        }
    }

    /// Leave any rebuild: the gap `cleared`, which ends a replay, or this
    /// replica is no longer named, which abandons it.
    fn end_rebuild(&mut self, cleared: bool) {
        match self.rebuilding.take() {
            Some(Rebuilding::Replaying(rebuild)) if cleared => {
                self.replays
                    .ended(self.group, rebuild.started().elapsed(), None);
                self.held = None;
            }
            Some(Rebuilding::Held(Some(started))) if cleared => {
                self.replays.ended(self.group, started.elapsed(), None);
                self.held = None;
            }
            Some(_) => self.held = None,
            None => {}
        }
    }

    /// Build the publication, reading the group's log id; `true` once
    /// there is one. Done without `kept` held, so a store that cannot be
    /// read costs no copy of what is kept, then [`Self::plan`] again.
    pub fn prepare(&mut self) -> bool {
        self.publication().is_some()
    }

    /// Publish what [`Self::plan`] decided, `out` being the step's output.
    /// Returns the gap a replay has just rebuilt through, for the caller to
    /// drop from `kept` ([`Kept::replayed`]) before planning again.
    pub fn publish(&mut self, need: Need, out: &Mined) -> Option<u64> {
        match need {
            Need::Replay { gap } => return self.replay(gap),
            Need::Hold => self.hold(),
            Need::Prepare => {}
            Need::Output => {
                if let Some(publication) = &self.publication {
                    publication.publish(&out.audit, &out.rows);
                }
            }
            Need::Backlog(backlog) => {
                let publication = self.publication()?;
                publication.resume();
                self.active = true;
                publication.publish(&backlog.audit, &backlog.rows);
                self.caught_up = true;
                self.current.send_replace(Some(publication));
            }
        }
        None
    }

    /// One slice of the replay through `gap`; the gap once it is rebuilt.
    fn replay(&mut self, gap: u64) -> Option<u64> {
        let publication = self.publication.clone()?;
        if self.rebuilding.is_none() {
            // Start from what is durable: what was handed on before the gap
            // and not yet written may be among what was dropped.
            self.hold();
            publication.resume();
            self.active = true;
            let rebuild = Rebuild::start(Arc::clone(&self.log), gap);
            self.rebuilding = Some(Rebuilding::Replaying(Box::new(rebuild)));
        }
        let Some(Rebuilding::Replaying(rebuild)) = &mut self.rebuilding else {
            return None;
        };
        match rebuild.slice(gap, &publication, self.source.flush.target_bytes) {
            Slice::Pending => None,
            Slice::Rebuilt => {
                self.replays
                    .ended(self.group, rebuild.started().elapsed(), None);
                self.rebuilding = None;
                Some(gap)
            }
            Slice::Stopped(error) => {
                let error_type = error.error_type();
                self.replays
                    .ended(self.group, rebuild.started().elapsed(), Some(error_type));
                self.hold();
                self.rebuilding = Some(Rebuilding::Held(None));
                let why = format!("the replay of its gap at {gap} stopped: {error}");
                self.report_held(error_type, &why);
                None
            }
        }
    }

    /// Stop publishing: what is not yet published stays kept, and the
    /// whole of it is published again once this replica may publish. The
    /// publication is kept, discarded, for that: one per group, so its
    /// sinks and their metrics are built once.
    fn hold(&mut self) {
        if self.active
            && let Some(publication) = &self.publication
        {
            publication.discard();
        }
        self.active = false;
        self.caught_up = false;
        self.current.send_replace(None);
    }

    /// This group's publication, built once its log id is readable.
    fn publication(&mut self) -> Option<Arc<Publication>> {
        if self.publication.is_none() {
            let log = match self.log_id() {
                Ok(log) => log,
                Err(Unread::Absent) => return None,
                Err(Unread::Failed { error_type, error }) => {
                    let why = format!("it cannot read the group's log id: {error}");
                    self.report_held(error_type, &why);
                    return None;
                }
            };
            self.held = None;
            let Source {
                store,
                flush,
                promoted,
                audit_ceiling,
                ..
            } = self.source.clone();
            let records =
                ParquetRecordSink::new(store.clone(), flush).with_promoted_attributes(promoted);
            let events = SharedParquetAuditSink::new(BufferingAuditSink::new(store, audit_ceiling));
            let publication = Publication::new(log, records, events);
            // RFC 0047 §3.3: the rows a flusher publishes feed the graph, as
            // the local receiver's do.
            #[cfg(feature = "openfga")]
            let publication = match &self.source.graph_emitter {
                Some(emitter) => publication.with_graph_emitter(Arc::clone(emitter)),
                None => publication,
            };
            self.publication = Some(Arc::new(publication));
        }
        self.publication.clone()
    }

    /// The log id `formed.json` records for this group.
    fn log_id(&self) -> Result<u32, Unread> {
        formed_log(&self.source, self.group)
    }

    /// Report being held for `error_type`, once until the reason changes.
    fn report_held(&mut self, error_type: &'static str, why: &str) {
        if self.held == Some(error_type) {
            return;
        }
        self.held = Some(error_type);
        tracing::warn!(
            name: semconv::EVENT_OURIOS_WAL_REPLICATION_PUBLICATION_HELD,
            {
                { semconv::OURIOS_WAL_GROUP } = i64::from(self.group),
                { crate::metrics::ERROR_TYPE } = error_type,
            },
            "the flusher of group {} publishes nothing: {why}",
            self.group,
        );
    }
}

/// Why the log id was not read.
pub(super) enum Unread {
    /// No record yet.
    Absent,
    Failed {
        error_type: &'static str,
        error: String,
    },
}

/// The log id `formed.json` records for `group`.
pub(super) fn formed_log(source: &Source, group: u32) -> Result<u32, Unread> {
    let bytes = match source.store.get_blocking_opt(&formed_key(group)) {
        Ok(Some(bytes)) => bytes,
        // Expected until the group forms.
        Ok(None) => return Err(Unread::Absent),
        Err(error) => {
            return Err(Unread::Failed {
                error_type: "store_read",
                error: error.to_string(),
            });
        }
    };
    let voters: Vec<&str> = source.voters.iter().map(String::as_str).collect();
    Formation::decode(&bytes, group, &voters)
        .map(|formation| formation.log())
        .map_err(|refused| Unread::Failed {
            error_type: refused.error_type(),
            error: refused.to_string(),
        })
}

mod rebuild;
#[cfg(test)]
mod tests;
