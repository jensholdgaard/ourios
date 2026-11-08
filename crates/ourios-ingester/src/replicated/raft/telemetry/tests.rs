//! The replication signals on the in-process cluster.

use std::time::Duration;

use openraft::async_runtime::WatchReceiver as _;
use opentelemetry::metrics::{Meter, MeterProvider as _};
use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use ourios_semconv as semconv;

use super::Meters;
use crate::replicated::raft::cluster::{Cluster, name, write};
use crate::replicated::raft::flusher::{Hold, Reason};
use crate::replicated::raft::types::{NodeId, Vote};

/// One meter provider and what it exports.
pub(crate) struct Reader {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
}

impl Reader {
    pub(crate) fn new() -> Self {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();
        Self { provider, exporter }
    }

    pub(crate) fn meter(&self) -> Meter {
        self.provider
            .meter_with_scope(ourios_core::telemetry::scope("ourios.wal"))
    }

    fn collect(&self) -> Vec<ResourceMetrics> {
        self.exporter.reset();
        self.provider.force_flush().expect("flush");
        self.exporter.get_finished_metrics().expect("collect")
    }

    /// `name`'s points as `(attribute values, value)`, the attributes in
    /// key order.
    pub(crate) fn points(&self, name: &str) -> Vec<(Vec<String>, u64)> {
        self.collect()
            .iter()
            .flat_map(ResourceMetrics::scope_metrics)
            .flat_map(ScopeMetrics::metrics)
            .filter(|m| m.name() == name)
            .flat_map(|m| match m.data() {
                AggregatedMetrics::U64(MetricData::Gauge(gauge)) => gauge
                    .data_points()
                    .map(|dp| (values(dp.attributes()), dp.value()))
                    .collect::<Vec<_>>(),
                AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
                    .data_points()
                    .map(|dp| (values(dp.attributes()), dp.value()))
                    .collect(),
                other => panic!("{name} is not a u64 gauge or sum: {other:?}"),
            })
            .collect()
    }
}

fn values<'a>(attributes: impl Iterator<Item = &'a opentelemetry::KeyValue>) -> Vec<String> {
    let mut pairs: Vec<_> = attributes
        .map(|kv| (kv.key.to_string(), kv.value.to_string()))
        .collect();
    pairs.sort();
    pairs.into_iter().map(|(_, value)| value).collect()
}

/// Collect `name` from `reader` until `done` holds of its points.
async fn until(reader: &Reader, name: &str, done: impl Fn(&[(Vec<String>, u64)]) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let points = reader.points(name);
        if done(&points) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{name} never settled: {points:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The lag the leader reports for `replica`, if any.
fn lag_of(points: &[(Vec<String>, u64)], replica: &str) -> Option<u64> {
    points
        .iter()
        .find(|(attrs, _)| attrs.iter().any(|v| v == replica))
        .map(|(_, lag)| *lag)
}

/// Only the leader reports lag, one series per follower, and every voter
/// reports the term they agree on. A crashed follower's lag grows with the
/// writes it misses while the other's settles back to zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_leader_reports_each_followers_lag_and_every_voter_the_term() {
    let readers: Vec<_> = (0..3).map(|_| Reader::new()).collect();
    let mut cluster = Cluster::start_metered(3, &[0], |i| Meters::on(&readers[i].meter())).await;
    let leader = cluster.leader(0, None).await;
    write(cluster.node(leader).raft(0), 1).await;

    let term = cluster
        .node(leader)
        .raft(0)
        .metrics()
        .borrow_watched()
        .current_term;
    for reader in &readers {
        until(reader, semconv::OURIOS_WAL_REPLICATION_TERM, |p| {
            p == [(vec!["0".to_owned()], term)]
        })
        .await;
    }
    let followers: Vec<_> = (0..3).filter(|&i| i != leader).collect();
    let [down, up] = followers[..] else {
        unreachable!("three voters")
    };
    for follower in [down, up] {
        let lag = readers[follower].points(semconv::OURIOS_WAL_REPLICATION_LAG);
        assert!(lag.is_empty(), "a follower reports no lag: {lag:?}");
    }
    let (down_name, up_name) = (name(cluster.id(down)), name(cluster.id(up)));
    until(&readers[leader], semconv::OURIOS_WAL_REPLICATION_LAG, |p| {
        p.len() == 2 && lag_of(p, &down_name) == Some(0) && lag_of(p, &up_name) == Some(0)
    })
    .await;

    cluster.crash(down).await;
    for n in 2..7 {
        write(cluster.node(leader).raft(0), n).await;
    }
    until(&readers[leader], semconv::OURIOS_WAL_REPLICATION_LAG, |p| {
        lag_of(p, &down_name).is_some_and(|lag| lag >= 5) && lag_of(p, &up_name) == Some(0)
    })
    .await;
    cluster.shutdown().await;
}

/// Each term is counted once, by the node that leads it, so the sum over
/// the nodes is the number of leadership changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_term_a_node_leads_is_counted_once() {
    let reader = Reader::new();
    let mut cluster = Cluster::start_metered(3, &[0], |_| Meters::on(&reader.meter())).await;
    let old = cluster.leader(0, None).await;
    write(cluster.node(old).raft(0), 1).await;
    let takeovers = semconv::OURIOS_WAL_REPLICATION_TAKEOVERS;
    until(&reader, takeovers, |p| p == [(vec!["0".to_owned()], 1)]).await;

    cluster.crash(old).await;
    let new = cluster.leader(0, Some(old)).await;
    write(cluster.node(new).raft(0), 2).await;
    until(&reader, takeovers, |p| p == [(vec!["0".to_owned()], 2)]).await;
    write(cluster.node(new).raft(0), 3).await;
    assert_eq!(
        reader.points(takeovers),
        [(vec!["0".to_owned()], 2)],
        "a later write is not a takeover"
    );
    cluster.shutdown().await;
}

/// A closed host stops reporting and its watches are freed, so a node
/// reopened in its place reports once and the gauges hold one host, not
/// one per host the node has had.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_closed_host_stops_reporting_and_a_reopened_one_reports_once() {
    let readers: Vec<_> = (0..3).map(|_| Reader::new()).collect();
    let meters: Vec<_> = readers.iter().map(|r| Meters::on(&r.meter())).collect();
    let mut cluster = Cluster::start_metered(3, &[0], |i| meters[i].clone()).await;
    let held = |i: usize| super::live(&meters[i].gauges.hosts).len();
    let leader = cluster.leader(0, None).await;
    write(cluster.node(leader).raft(0), 1).await;
    let follower = (0..3).find(|&i| i != leader).expect("a follower");
    let term = semconv::OURIOS_WAL_REPLICATION_TERM;
    until(&readers[follower], term, |p| p.len() == 1).await;

    cluster.crash(follower).await;
    let reported = readers[follower].points(term);
    assert_eq!(reported, [], "a closed host reports nothing");
    assert_eq!(held(follower), 0, "a closed host's watches are freed");
    for _ in 0..2 {
        cluster.restart(follower).await;
        until(&readers[follower], term, |p| p.len() == 1).await;
        assert_eq!(held(follower), 1, "one host per node");
        cluster.crash(follower).await;
    }
    cluster.shutdown().await;
}

/// A takeover is a saved vote that is committed and names this node:
/// each such vote counts, whatever else the node saw in between, and no
/// other vote does.
#[test]
fn every_committed_vote_for_this_node_is_one_takeover() {
    let reader = Reader::new();
    let (me, other) = (NodeId::from(1), NodeId::from(2));
    let takeovers = Meters::on(&reader.meter()).takeovers(me, 0);
    for vote in [
        Vote::new(1, me),
        Vote::new_committed(1, me),
        Vote::new_committed(2, other),
        Vote::new(3, me),
        Vote::new_committed(3, me),
        Vote::new_committed(4, other),
    ] {
        takeovers.saved(&vote);
    }
    let counted = reader.points(semconv::OURIOS_WAL_REPLICATION_TAKEOVERS);
    assert_eq!(counted, [(vec!["0".to_owned()], 2)], "terms 1 and 3");
}

/// Each count carries its group and the registry's value for its reason.
#[test]
fn flusher_counts_carry_the_group_and_the_reason() {
    let reader = Reader::new();
    let counts = Meters::on(&reader.meter()).flusher();
    for reason in [Reason::Unassigned, Reason::NotLearner, Reason::Unhealthy] {
        counts.assigned(3, reason);
    }
    counts.assigned(4, Reason::Unhealthy);
    counts.held(3, Hold::MemberUnconfirmed);
    counts.held(3, Hold::MembershipChange);
    let point = |group: &str, reason: &str| (vec![group.to_owned(), reason.to_owned()], 1);

    let mut assigned = reader.points(semconv::OURIOS_WAL_REPLICATION_FLUSHER_ASSIGNMENTS);
    assigned.sort();
    let expected = [
        point("3", "not_learner"),
        point("3", "unassigned"),
        point("3", "unhealthy"),
        point("4", "unhealthy"),
    ];
    assert_eq!(assigned, expected);
    let mut held = reader.points(semconv::OURIOS_WAL_REPLICATION_CONTROL_HOLDS);
    held.sort();
    let expected = [
        point("3", "member_unconfirmed"),
        point("3", "membership_change"),
    ];
    assert_eq!(held, expected);
}
