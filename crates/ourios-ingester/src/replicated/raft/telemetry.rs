//! RFC 0065 §6.9's per-group replication signals, read from each group's
//! `openraft` metrics: follower lag and the term as gauges, and a counter
//! of the terms this node leads.

use openraft::RaftMetrics;
use openraft::async_runtime::WatchReceiver as _;
use openraft::type_config::alias::WatchReceiverOf;
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Meter, ObservableGauge};
use ourios_semconv as semconv;
use tokio::task::JoinHandle;

use super::host::GroupRaft;
use super::types::RaftTypes;

type Watch = WatchReceiverOf<RaftTypes, RaftMetrics<RaftTypes>>;

/// The instruments for one node's groups. Dropping it stops the takeover
/// counting; a stopped group's gauges report nothing.
pub(crate) struct ReplicationTelemetry {
    _gauges: [ObservableGauge<u64>; 2],
    takeovers: Vec<JoinHandle<()>>,
}

impl ReplicationTelemetry {
    /// Watch `groups`, the rafts this node runs, on `meter`.
    pub(crate) fn watch<'a>(
        meter: &Meter,
        groups: impl Iterator<Item = (u32, &'a GroupRaft)>,
    ) -> Self {
        let watches: Vec<(u32, Watch)> = groups.map(|(g, raft)| (g, raft.metrics())).collect();
        let takeovers = meter
            .u64_counter(semconv::OURIOS_WAL_REPLICATION_TAKEOVERS)
            .with_description(semconv::OURIOS_WAL_REPLICATION_TAKEOVERS_DESCRIPTION)
            .with_unit(semconv::OURIOS_WAL_REPLICATION_TAKEOVERS_UNIT)
            .build();
        let counting = watches
            .iter()
            .map(|(group, watch)| {
                tokio::spawn(count_takeovers(watch.clone(), *group, takeovers.clone()))
            })
            .collect();
        let gauges = GAUGES.map(|spec| gauge(meter, spec, watches.clone()));
        Self {
            _gauges: gauges,
            takeovers: counting,
        }
    }
}

impl Drop for ReplicationTelemetry {
    fn drop(&mut self) {
        for counting in &self.takeovers {
            counting.abort();
        }
    }
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
    watches: Vec<(u32, Watch)>,
) -> ObservableGauge<u64> {
    meter
        .u64_observable_gauge(name)
        .with_description(description)
        .with_unit(unit)
        .with_callback(move |observer| {
            let points = watches
                .iter()
                .flat_map(|(group, watch)| read(*group, &watch.borrow_watched()));
            for (value, attributes) in points {
                observer.observe(value, &attributes);
            }
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

/// Count each term this node leads `group` in, once, until the group stops.
async fn count_takeovers(mut watch: Watch, group: u32, takeovers: Counter<u64>) {
    let mut counted = None;
    loop {
        let led = leading_term(&watch.borrow_watched());
        if let Some(term) = led
            && counted != Some(term)
        {
            takeovers.add(1, &[group_kv(group)]);
            counted = Some(term);
        }
        if watch.changed().await.is_err() {
            return;
        }
    }
}

fn leading_term(metrics: &RaftMetrics<RaftTypes>) -> Option<u64> {
    (metrics.current_leader == Some(metrics.id)).then_some(metrics.current_term)
}

#[cfg(test)]
mod tests;
