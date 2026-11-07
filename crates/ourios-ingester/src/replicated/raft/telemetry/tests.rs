//! The replication signals on the in-process cluster.

use std::time::Duration;

use openraft::async_runtime::WatchReceiver as _;
use opentelemetry::metrics::{Meter, MeterProvider as _};
use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use ourios_semconv as semconv;

use super::ReplicationTelemetry;
use crate::replicated::raft::cluster::{Cluster, Node, name, write};

/// One meter provider and what it exports.
struct Reader {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
}

impl Reader {
    fn new() -> Self {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();
        Self { provider, exporter }
    }

    fn meter(&self) -> Meter {
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
    fn points(&self, name: &str) -> Vec<(Vec<String>, u64)> {
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

fn watch(meter: &Meter, node: &Node) -> ReplicationTelemetry {
    ReplicationTelemetry::watch(meter, node.host.groups().map(|g| (g, node.raft(g))))
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
    let mut cluster = Cluster::start(3, &[0]).await;
    let leader = cluster.leader(0, None).await;
    write(cluster.node(leader).raft(0), 1).await;
    let readers: Vec<_> = (0..3).map(|_| Reader::new()).collect();
    let _telemetry: Vec<_> = (0..3)
        .map(|i| watch(&readers[i].meter(), cluster.node(i)))
        .collect();

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
    let mut cluster = Cluster::start(3, &[0]).await;
    let reader = Reader::new();
    let _telemetry: Vec<_> = (0..3)
        .map(|i| watch(&reader.meter(), cluster.node(i)))
        .collect();
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
