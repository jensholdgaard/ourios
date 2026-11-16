//! RFC 0065 §6.9's per-group replication signals. Follower lag and the
//! term are gauges read from each group's `openraft` metrics. A takeover
//! is counted where the group log saves the leader's committed vote, so a
//! term this node leads is counted however briefly it leads. The leader's
//! flusher pass counts the assignments it commits and the passes its kind
//! gate holds back (§6.5, §6.8).
//!
//! The SDK never unregisters an observable-instrument callback, so the
//! gauges are registered once per meter provider over a registry of weakly
//! held hosts, as the miner's are (`ourios-miner`'s `metrics/gauges.rs`):
//! a closed host stops reporting and frees its watches.
//!
//! The storage writers time each write-and-fsync, and count a writer's
//! first failure as a stop of every group it serves.

use std::io;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use openraft::RaftMetrics;
use openraft::async_runtime::WatchReceiver as _;
use openraft::type_config::alias::WatchReceiverOf;
use openraft::vote::RaftLeaderId as _;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter, MeterProvider, ObservableGauge};
use ourios_core::sync::lock_recover;
use ourios_semconv as semconv;

use super::flusher::{Hold, Reason};
use super::host::GroupRaft;
use super::kept::{Dropped, Kept};
use super::mining::GroupMinerError;
use super::types::{NodeId, RaftTypes, Vote};

type Watch = WatchReceiverOf<RaftTypes, RaftMetrics<RaftTypes>>;
type Watches = Vec<(u32, Watch)>;
type Hosts = Arc<Mutex<Vec<Weak<Watches>>>>;
type GlobalProvider = Arc<dyn MeterProvider + Send + Sync>;

/// Where a host reports: the gauges' registry and the counters.
#[derive(Clone)]
pub(crate) struct Meters {
    gauges: Arc<Gauges>,
    takeovers: Counter<u64>,
    flusher: FlusherCounts,
    storage: Storage,
    kept: KeptDrops,
    replays: Replays,
}

impl Meters {
    /// The meters on the installed global provider.
    pub(crate) fn global() -> Self {
        let provider = opentelemetry::global::meter_provider();
        let gauges = gauges_for(&provider);
        Self::with(&scoped(&provider), gauges)
    }

    /// Meters on `meter`, with gauge callbacks of their own.
    #[cfg(test)]
    pub(crate) fn on(meter: &Meter) -> Self {
        Self::with(meter, Arc::new(Gauges::register(meter)))
    }

    fn with(meter: &Meter, gauges: Arc<Gauges>) -> Self {
        let takeovers = counter(meter, TAKEOVERS);
        let flusher = FlusherCounts {
            assignments: counter(meter, ASSIGNMENTS),
            holds: counter(meter, HOLDS),
        };
        Self {
            gauges,
            takeovers,
            flusher,
            storage: Storage::on(meter),
            kept: KeptDrops::on(meter),
            replays: Replays::on(meter),
        }
    }

    /// What a tail replica counts of the output it drops.
    pub(crate) fn kept(&self) -> KeptDrops {
        self.kept.clone()
    }

    /// Report `group`'s kept output from `kept` for as long as it lives.
    pub(crate) fn keep(&self, group: u32, kept: &Arc<Mutex<Kept>>) {
        self.gauges.keep(group, kept);
    }

    /// What a named flusher's gap replays record.
    pub(crate) fn replays(&self) -> Replays {
        self.replays.clone()
    }

    /// What the storage writers record.
    pub(crate) fn storage(&self) -> Storage {
        self.storage.clone()
    }

    /// The counts of a group leader's flusher pass.
    pub(crate) fn flusher(&self) -> FlusherCounts {
        self.flusher.clone()
    }

    /// The takeover count of node `id` in `group`.
    pub(crate) fn takeovers(&self, id: NodeId, group: u32) -> Takeovers {
        Takeovers {
            id,
            group,
            counter: self.takeovers.clone(),
        }
    }

    /// Report `groups`, a host's rafts, until the returned handle drops.
    pub(crate) fn watch<'a>(
        &self,
        groups: impl Iterator<Item = (u32, &'a GroupRaft)>,
    ) -> ReplicationTelemetry {
        let watches = Arc::new(groups.map(|(g, raft)| (g, raft.metrics())).collect());
        self.gauges.add(&watches);
        ReplicationTelemetry { _watches: watches }
    }
}

/// A host's watches, reported while it is held.
pub(crate) struct ReplicationTelemetry {
    _watches: Arc<Watches>,
}

/// Counts the terms a node leads in one group, from the votes its log
/// saves. Winning an election saves the leader's committed vote, and
/// `openraft` saves a vote only when it changes, so each term this node
/// leads is counted once.
pub(crate) struct Takeovers {
    id: NodeId,
    group: u32,
    counter: Counter<u64>,
}

impl Takeovers {
    /// `vote` is durable in the group's vote log.
    pub(crate) fn saved(&self, vote: &Vote) {
        if vote.is_committed() && *vote.leader_id().node_id() == self.id {
            self.counter.add(1, &[group_kv(self.group)]);
        }
    }
}

/// What a leader's flusher pass counts: each assignment it commits, and
/// each pass whose kind gate held a new kind back.
#[derive(Clone, Debug)]
pub(crate) struct FlusherCounts {
    assignments: Counter<u64>,
    holds: Counter<u64>,
}

impl FlusherCounts {
    /// An assignment made for `reason` committed in `group`.
    pub(crate) fn assigned(&self, group: u32, reason: Reason) {
        let reason = KeyValue::new(
            semconv::OURIOS_WAL_REPLICATION_FLUSHER_ASSIGNMENT_REASON,
            reason.as_str(),
        );
        self.assignments.add(1, &[group_kv(group), reason]);
    }

    /// A pass in `group` held back a kind its log has never held.
    pub(crate) fn held(&self, group: u32, hold: Hold) {
        let reason = KeyValue::new(
            semconv::OURIOS_WAL_REPLICATION_CONTROL_HOLD_REASON,
            hold.as_str(),
        );
        self.holds.add(1, &[group_kv(group), reason]);
    }
}

/// The part of a group a signal is about: its log store or state machine,
/// or the node's vote log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Component {
    LogStore(u32),
    StateMachine(u32),
    VoteLog,
    Miner(u32),
}

impl Component {
    fn as_str(self) -> &'static str {
        match self {
            Self::LogStore(_) => "log_store",
            Self::StateMachine(_) => "state_machine",
            Self::VoteLog => "vote_log",
            Self::Miner(_) => "miner",
        }
    }

    fn kv(self) -> KeyValue {
        KeyValue::new(semconv::OURIOS_WAL_REPLICATION_COMPONENT, self.as_str())
    }

    /// Its attributes: with its group, unless it is the node's vote log.
    fn attributes(self) -> Vec<KeyValue> {
        match self {
            Self::LogStore(group) | Self::StateMachine(group) | Self::Miner(group) => {
                vec![self.kv(), group_kv(group)]
            }
            Self::VoteLog => vec![self.kv()],
        }
    }
}

/// A component's stop: the groups it stopped in, and why.
struct Stop<'a> {
    component: Component,
    groups: &'a [u32],
    error_type: &'a str,
    error: &'a dyn std::fmt::Display,
}

/// What a storage writer records: each write-and-fsync, and the stops its
/// first failure causes.
#[derive(Clone, Debug)]
pub(crate) struct Storage {
    syncs: Histogram<f64>,
    stops: Counter<u64>,
}

impl Storage {
    /// The instruments on the installed global provider.
    pub(crate) fn global() -> Self {
        Self::on(&scoped(&opentelemetry::global::meter_provider()))
    }

    fn on(meter: &Meter) -> Self {
        let (name, description, unit) = SYNCS;
        let syncs = meter
            .f64_histogram(name)
            .with_description(description)
            .with_unit(unit)
            .with_boundaries(ourios_core::histogram_buckets::FAST_DURATION_S.to_vec())
            .build();
        Self {
            syncs,
            stops: counter(meter, STOPS),
        }
    }

    /// One write-and-fsync by `component`, taking `elapsed` and failing
    /// with `failed`.
    pub(crate) fn synced(
        &self,
        component: Component,
        elapsed: Duration,
        failed: Option<io::ErrorKind>,
    ) {
        let mut attributes = component.attributes();
        attributes.extend(failed.map(error_kv));
        self.syncs.record(elapsed.as_secs_f64(), &attributes);
    }

    /// `component` failed with `error` and stopped serving `groups`: logged
    /// once, and counted once per group.
    pub(crate) fn stopped(&self, component: Component, groups: &[u32], error: &io::Error) {
        self.report(&Stop {
            component,
            groups,
            error_type: &error_type(error.kind()),
            error,
        });
    }

    /// `group`'s miner stopped on `error`: logged and counted once. The
    /// replica applies on; only its mining stops.
    pub(crate) fn miner_stopped(&self, group: u32, error: &GroupMinerError) {
        self.report(&Stop {
            component: Component::Miner(group),
            groups: &[group],
            error_type: error.error_type(),
            error,
        });
    }

    fn report(&self, stop: &Stop<'_>) {
        let Stop {
            component,
            groups,
            error_type,
            error,
        } = *stop;
        let group = match component {
            Component::LogStore(group)
            | Component::StateMachine(group)
            | Component::Miner(group) => Some(i64::from(group)),
            Component::VoteLog => None,
        };
        tracing::error!(
            name: semconv::EVENT_OURIOS_WAL_REPLICATION_STOPPED,
            {
                { semconv::OURIOS_WAL_REPLICATION_COMPONENT } = component.as_str(),
                { crate::metrics::ERROR_TYPE } = error_type,
                { semconv::OURIOS_WAL_GROUP } = group,
            },
            "the replicated WAL's {} stopped in groups {groups:?} until the node restarts: {error}",
            component.as_str(),
        );
        for &group in groups {
            self.count_stop(component, group, error_type.to_owned());
        }
    }

    /// `component`, already stopped on an error of `kind`, stops `group`
    /// as well: counted, not logged again.
    pub(crate) fn also_stopped(&self, component: Component, group: u32, kind: io::ErrorKind) {
        self.count_stop(component, group, error_type(kind));
    }

    fn count_stop(&self, component: Component, group: u32, error_type: String) {
        let error = KeyValue::new(crate::metrics::ERROR_TYPE, error_type);
        self.stops.add(1, &[component.kv(), group_kv(group), error]);
    }
}

fn error_kv(kind: io::ErrorKind) -> KeyValue {
    KeyValue::new(crate::metrics::ERROR_TYPE, error_type(kind))
}

/// `kind` as `error.type`: its name in snake case.
pub(super) fn error_type(kind: io::ErrorKind) -> String {
    let name = format!("{kind:?}");
    let mut snake = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            snake.push('_');
        }
        snake.push(c.to_ascii_lowercase());
    }
    snake
}

/// What a tail replica counts of the output it drops from what it keeps
/// for a takeover (§6.7). The audit bytes it keeps and its gap are gauges
/// read from the live [`Kept`] each group registers.
#[derive(Clone, Debug)]
pub(crate) struct KeptDrops {
    audit: Counter<u64>,
    rows: Counter<u64>,
}

impl KeptDrops {
    fn on(meter: &Meter) -> Self {
        Self {
            audit: counter(meter, KEPT_AUDIT_DROPPED),
            rows: counter(meter, KEPT_ROW_DROPPED),
        }
    }

    /// `group` has dropped `now` in all, `before` of it until this step.
    pub(crate) fn dropped(&self, group: u32, now: Dropped, before: Dropped) {
        let attributes = [group_kv(group)];
        if now.audit > before.audit {
            self.audit.add(now.audit - before.audit, &attributes);
        }
        if now.rows > before.rows {
            self.rows.add(now.rows - before.rows, &attributes);
        }
    }
}

/// How long a named flusher's gap replays take (§6.7, §6.9).
#[derive(Clone, Debug)]
pub(crate) struct Replays {
    duration: Histogram<f64>,
}

impl Replays {
    /// The instrument on the installed global provider.
    #[must_use]
    pub(crate) fn global() -> Self {
        Self::on(&scoped(&opentelemetry::global::meter_provider()))
    }

    pub(crate) fn on(meter: &Meter) -> Self {
        let (name, description, unit) = REPLAY_DURATION;
        let duration = meter
            .f64_histogram(name)
            .with_description(description)
            .with_unit(unit)
            .with_boundaries(ourios_core::histogram_buckets::LONG_DURATION_S.to_vec())
            .build();
        Self { duration }
    }

    /// A replay of `group`'s gap ended after `elapsed`, stopped by
    /// `error_type` if it did not rebuild the gap.
    pub(crate) fn ended(&self, group: u32, elapsed: Duration, error_type: Option<&'static str>) {
        let mut attributes = vec![group_kv(group)];
        attributes.extend(error_type.map(|e| KeyValue::new(crate::metrics::ERROR_TYPE, e)));
        self.duration.record(elapsed.as_secs_f64(), &attributes);
    }
}

/// A counter's name, description and unit.
type Spec = (&'static str, &'static str, &'static str);

const TAKEOVERS: Spec = (
    semconv::OURIOS_WAL_REPLICATION_TAKEOVERS,
    semconv::OURIOS_WAL_REPLICATION_TAKEOVERS_DESCRIPTION,
    semconv::OURIOS_WAL_REPLICATION_TAKEOVERS_UNIT,
);

const ASSIGNMENTS: Spec = (
    semconv::OURIOS_WAL_REPLICATION_FLUSHER_ASSIGNMENTS,
    semconv::OURIOS_WAL_REPLICATION_FLUSHER_ASSIGNMENTS_DESCRIPTION,
    semconv::OURIOS_WAL_REPLICATION_FLUSHER_ASSIGNMENTS_UNIT,
);

const HOLDS: Spec = (
    semconv::OURIOS_WAL_REPLICATION_CONTROL_HOLDS,
    semconv::OURIOS_WAL_REPLICATION_CONTROL_HOLDS_DESCRIPTION,
    semconv::OURIOS_WAL_REPLICATION_CONTROL_HOLDS_UNIT,
);

const SYNCS: Spec = (
    semconv::OURIOS_WAL_REPLICATION_SYNC_DURATION,
    semconv::OURIOS_WAL_REPLICATION_SYNC_DURATION_DESCRIPTION,
    semconv::OURIOS_WAL_REPLICATION_SYNC_DURATION_UNIT,
);

const KEPT_AUDIT_DROPPED: Spec = (
    semconv::OURIOS_WAL_REPLICATION_KEPT_AUDIT_DROPPED,
    semconv::OURIOS_WAL_REPLICATION_KEPT_AUDIT_DROPPED_DESCRIPTION,
    semconv::OURIOS_WAL_REPLICATION_KEPT_AUDIT_DROPPED_UNIT,
);

const KEPT_ROW_DROPPED: Spec = (
    semconv::OURIOS_WAL_REPLICATION_KEPT_ROW_DROPPED,
    semconv::OURIOS_WAL_REPLICATION_KEPT_ROW_DROPPED_DESCRIPTION,
    semconv::OURIOS_WAL_REPLICATION_KEPT_ROW_DROPPED_UNIT,
);

const REPLAY_DURATION: Spec = (
    semconv::OURIOS_WAL_REPLICATION_REPLAY_DURATION,
    semconv::OURIOS_WAL_REPLICATION_REPLAY_DURATION_DESCRIPTION,
    semconv::OURIOS_WAL_REPLICATION_REPLAY_DURATION_UNIT,
);

const STOPS: Spec = (
    semconv::OURIOS_WAL_REPLICATION_STOPS,
    semconv::OURIOS_WAL_REPLICATION_STOPS_DESCRIPTION,
    semconv::OURIOS_WAL_REPLICATION_STOPS_UNIT,
);

fn counter(meter: &Meter, (name, description, unit): Spec) -> Counter<u64> {
    meter
        .u64_counter(name)
        .with_description(description)
        .with_unit(unit)
        .build()
}

/// Each tail-replica group's kept output, held weakly.
type KeptGroups = Arc<Mutex<Vec<(u32, Weak<Mutex<Kept>>)>>>;

/// One provider's gauges and the live hosts and kept outputs they read.
struct Gauges {
    hosts: Hosts,
    kept: KeptGroups,
    _instruments: [ObservableGauge<u64>; 2],
    _kept: [ObservableGauge<u64>; 2],
}

impl Gauges {
    fn register(meter: &Meter) -> Self {
        let hosts = Hosts::default();
        let instruments = GAUGES.map(|spec| gauge(meter, spec, Arc::clone(&hosts)));
        let kept = KeptGroups::default();
        let kept_gauges = KEPT_GAUGES.map(|spec| kept_gauge(meter, spec, Arc::clone(&kept)));
        Self {
            hosts,
            kept,
            _instruments: instruments,
            _kept: kept_gauges,
        }
    }

    fn keep(&self, group: u32, kept: &Arc<Mutex<Kept>>) {
        let mut groups = lock_recover(&self.kept);
        groups.retain(|(_, kept)| kept.strong_count() > 0);
        groups.push((group, Arc::downgrade(kept)));
    }

    fn add(&self, watches: &Arc<Watches>) {
        let mut hosts = lock_recover(&self.hosts);
        hosts.retain(|host| host.strong_count() > 0);
        hosts.push(Arc::downgrade(watches));
    }
}

/// The live hosts, pruning the closed ones.
fn live(hosts: &Hosts) -> Vec<Arc<Watches>> {
    let mut hosts = lock_recover(hosts);
    let mut live = Vec::with_capacity(hosts.len());
    hosts.retain(|host| host.upgrade().map(|watches| live.push(watches)).is_some());
    live
}

/// One gauge set per global installation still alive.
static GLOBAL: Mutex<Vec<Binding>> = Mutex::new(Vec::new());

/// An installation, held weakly, and the gauges registered on it.
type Binding = (Weak<dyn MeterProvider + Send + Sync>, Arc<Gauges>);

/// The gauges reporting to `provider`, registered on it the first time
/// this installation is seen.
fn gauges_for(provider: &GlobalProvider) -> Arc<Gauges> {
    let mut bound = lock_recover(&GLOBAL);
    bound.retain(|(installed, _)| installed.strong_count() > 0);
    let key = Arc::downgrade(provider);
    // A held `Weak` keeps the allocation, so a live key's address is never
    // reused and the comparison cannot alias.
    let known = bound
        .iter()
        .find(|(installed, _)| Weak::ptr_eq(installed, &key));
    if let Some((_, gauges)) = known {
        return Arc::clone(gauges);
    }
    let gauges = Arc::new(Gauges::register(&scoped(provider)));
    bound.push((key, Arc::clone(&gauges)));
    gauges
}

fn scoped(provider: &GlobalProvider) -> Meter {
    provider.meter_with_scope(ourios_core::telemetry::scope("ourios.wal"))
}

/// One group's points of a gauge: each value with its attributes.
type Read = fn(u32, &RaftMetrics<RaftTypes>) -> Vec<(u64, Vec<KeyValue>)>;

const GAUGES: [(&str, &str, &str, Read); 2] = [
    (
        semconv::OURIOS_WAL_REPLICATION_LAG,
        semconv::OURIOS_WAL_REPLICATION_LAG_DESCRIPTION,
        semconv::OURIOS_WAL_REPLICATION_LAG_UNIT,
        |group, metrics| {
            let lags = lags(metrics).into_iter();
            lags.map(|(replica, lag)| (lag, vec![group_kv(group), replica_kv(replica)]))
                .collect()
        },
    ),
    (
        semconv::OURIOS_WAL_REPLICATION_TERM,
        semconv::OURIOS_WAL_REPLICATION_TERM_DESCRIPTION,
        semconv::OURIOS_WAL_REPLICATION_TERM_UNIT,
        |group, metrics| {
            let running = metrics.running_state.is_ok();
            running
                .then(|| (metrics.current_term, vec![group_kv(group)]))
                .into_iter()
                .collect()
        },
    ),
];

fn gauge(
    meter: &Meter,
    (name, description, unit, read): (&'static str, &'static str, &'static str, Read),
    hosts: Hosts,
) -> ObservableGauge<u64> {
    meter
        .u64_observable_gauge(name)
        .with_description(description)
        .with_unit(unit)
        .with_callback(move |observer| {
            let live = live(&hosts);
            let groups = live.iter().flat_map(|watches| watches.iter());
            let points = groups.flat_map(|(group, watch)| read(*group, &watch.borrow_watched()));
            for (value, attributes) in points {
                observer.observe(value, &attributes);
            }
        })
        .build()
}

/// A kept-output gauge: its name, description, unit and value.
type KeptSpec = (&'static str, &'static str, &'static str, fn(&Kept) -> u64);

const KEPT_GAUGES: [KeptSpec; 2] = [
    (
        semconv::OURIOS_WAL_REPLICATION_KEPT_AUDIT_USAGE,
        semconv::OURIOS_WAL_REPLICATION_KEPT_AUDIT_USAGE_DESCRIPTION,
        semconv::OURIOS_WAL_REPLICATION_KEPT_AUDIT_USAGE_UNIT,
        |kept| u64::try_from(kept.audit_bytes()).unwrap_or(u64::MAX),
    ),
    (
        semconv::OURIOS_WAL_REPLICATION_KEPT_GAP,
        semconv::OURIOS_WAL_REPLICATION_KEPT_GAP_DESCRIPTION,
        semconv::OURIOS_WAL_REPLICATION_KEPT_GAP_UNIT,
        Kept::gap_reach,
    ),
];

fn kept_gauge(
    meter: &Meter,
    (name, description, unit, read): KeptSpec,
    groups: KeptGroups,
) -> ObservableGauge<u64> {
    meter
        .u64_observable_gauge(name)
        .with_description(description)
        .with_unit(unit)
        .with_callback(move |observer| {
            let mut groups = lock_recover(&groups);
            groups.retain(|(group, kept)| {
                let Some(kept) = kept.upgrade() else {
                    return false;
                };
                observer.observe(read(&lock_recover(&kept)), &[group_kv(*group)]);
                true
            });
        })
        .build()
}

fn group_kv(group: u32) -> KeyValue {
    KeyValue::new(semconv::OURIOS_WAL_GROUP, i64::from(group))
}

fn replica_kv(replica: String) -> KeyValue {
    KeyValue::new(semconv::OURIOS_WAL_REPLICA, replica)
}

/// Each follower's `node_id` and the entries the leader has appended
/// that it has not seen the follower match; none unless this node leads
/// a running group.
fn lags(metrics: &RaftMetrics<RaftTypes>) -> Vec<(String, u64)> {
    let Some(replication) = &metrics.replication else {
        return Vec::new();
    };
    if metrics.running_state.is_err() {
        return Vec::new();
    }
    let appended = metrics.last_log_index.map_or(0, |last| last + 1);
    let membership = metrics.membership_config.membership();
    replication
        .iter()
        .filter(|(follower, _)| **follower != metrics.id)
        .map(|(follower, matched)| {
            let replica = membership
                .get_node(follower)
                .cloned()
                .unwrap_or_else(|| follower.to_string());
            let matched = matched.map_or(0, |m| m.index + 1);
            (replica, appended.saturating_sub(matched))
        })
        .collect()
}

#[cfg(test)]
mod state_machine_tests;
#[cfg(test)]
mod storage_tests;
#[cfg(test)]
pub(crate) mod tests;
