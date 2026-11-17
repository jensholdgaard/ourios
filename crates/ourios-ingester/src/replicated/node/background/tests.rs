use std::collections::BTreeMap;
use std::time::Duration;

use openraft::async_runtime::WatchReceiver as _;
use ourios_config::MinerConfig;
use ourios_core::audit::SharedAuditSink;
use ourios_parquet::Store;
use ourios_serving::AuthResolver;
use tokio::sync::watch;

use super::super::serving_tests::three_served;
use super::super::start;
use super::super::tests::n0_named;
use super::{Background, Once};
use crate::replicated::layout::VotingLayout;
use crate::replicated::raft::telemetry::storage_tests::events;
use crate::replicated::raft::transport::Peers;

fn background(store: &Store) -> Background {
    Background {
        store: store.clone(),
        miner: MinerConfig::default(),
        placed: BTreeMap::new(),
    }
}

#[test]
fn a_reason_is_reported_once_until_it_changes_or_clears() {
    let mut once = Once::default();

    assert_eq!(once.changed(Some("a")), Some("a"));
    assert_eq!(once.changed(Some("a")), None);
    assert_eq!(once.changed(Some("b")), Some("b"));
    assert_eq!(once.changed(None), None);
    assert_eq!(once.changed(Some("b")), Some("b"));
}

/// RFC 0065 §6.1, §6.5: served nodes form their group in the background,
/// from `formed.json`, with no one initialising it, and its leader's steward
/// puts the miner configuration in force. Id blocks follow the flusher's
/// reports, so a group with no tail replica takes none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn served_nodes_form_their_group_and_its_leader_stewards_it() {
    let served = three_served(&AuthResolver::static_only(None), &SharedAuditSink::new()).await;
    let (stop, stopped) = watch::channel(false);
    let mut tasks = Vec::new();
    for node in &served.nodes {
        tasks.extend(node.run(&background(&served.store), &stopped));
    }

    let raft = served.nodes[0].host.raft(0).expect("hosted");
    let led = raft
        .wait(Some(Duration::from_secs(30)))
        .metrics(|m| m.current_leader.is_some(), "the group forms")
        .await
        .expect("the group forms");
    let leader = led.current_leader.expect("a leader");
    let leading = served
        .nodes
        .iter()
        .find(|n| n.id == leader)
        .expect("the leader");
    let sm = leading.host.state_machine(0).expect("hosted");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while sm.miner_config().is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the steward configures the group"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    stop.send_replace(true);
    for task in tasks {
        task.await.expect("stopped");
    }
    served.stop().await;
}

/// A voter whose group has no formation record waits for the first voter to
/// record it, and says so once, naming the group.
#[test]
fn a_waiting_formation_is_reported_once() {
    let layout = VotingLayout::new(["n0", "n1", "n2"], std::num::NonZeroU32::MIN, 3);
    let second = layout.voters(0)[1].to_owned();
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::in_memory();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let logged = events(|| {
        runtime.block_on(async {
            let start_of = n0_named(&second, root.path(), &store, Peers::plaintext());
            let node = start(start_of, &mut |_| {}).await.expect("started");
            let (stop, stopped) = watch::channel(false);
            let tasks = node.run(&background(&store), &stopped);
            tokio::time::sleep(Duration::from_millis(1_500)).await;
            stop.send_replace(true);
            for task in tasks {
                task.await.expect("stopped");
            }
            node.host.shutdown().await.expect("shutdown");
        });
    });

    let waiting: Vec<_> = logged
        .into_iter()
        .filter(|(name, _)| name == ourios_semconv::EVENT_OURIOS_WAL_REPLICATION_START_WAITING)
        .collect();
    let fields = ["error.type=no_record", "ourios.wal.group=0"].map(str::to_owned);
    assert_eq!(waiting.len(), 1, "{waiting:?}");
    assert_eq!(waiting[0].1, fields);
}

/// A start that finds the bootstrap lock held and unseeded waits, and says
/// so once, naming no group.
#[test]
fn a_start_waiting_on_the_lock_is_reported_once() {
    let store = Store::in_memory();
    let holder = tempfile::tempdir().expect("tempdir");
    let waiter = tempfile::tempdir().expect("tempdir");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    // A first start, not allowed to bootstrap over data the store holds,
    // takes the lock and leaves it unseeded.
    runtime.block_on(async {
        store
            .put("data/tenant_id=t/notes.txt", b"not parquet".to_vec())
            .await
            .expect("data");
        let mut first = n0_named("n1", holder.path(), &store, Peers::plaintext());
        first.allow_bootstrap = false;
        assert!(
            start(first, &mut |_| {}).await.is_err(),
            "refused, lock held"
        );
    });

    let logged = events(|| {
        runtime.block_on(async {
            let second = n0_named("n0", waiter.path(), &store, Peers::plaintext());
            let mut ignore = |_: &crate::replicated::raft::log_ids::Waiting| {};
            let starting = start(second, &mut ignore);
            let held = tokio::time::timeout(Duration::from_millis(1_500), starting).await;
            assert!(held.is_err(), "it waits on the lock");
        });
    });

    let waiting: Vec<_> = logged
        .into_iter()
        .filter(|(name, _)| name == ourios_semconv::EVENT_OURIOS_WAL_REPLICATION_START_WAITING)
        .collect();
    assert_eq!(waiting.len(), 1, "{waiting:?}");
    assert_eq!(waiting[0].1, ["error.type=log_ids_unseeded".to_owned()]);
}

/// RFC 0065 §6.1: a group whose formation record does not read stops, its
/// Raft shut down so no peer's traffic joins it to a membership it refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_group_that_cannot_form_is_shut_down() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = Store::in_memory();
    store
        .put(
            &crate::replicated::raft::formation::formed_key(0),
            b"not a formation record".to_vec(),
        )
        .await
        .expect("record");
    let node = start(
        n0_named("n0", root.path(), &store, Peers::plaintext()),
        &mut |_| {},
    )
    .await
    .expect("started");
    let (_stop, stopped) = watch::channel(false);

    let tasks = node.run(&background(&store), &stopped);

    let raft = node.host.raft(0).expect("hosted");
    let shut = raft
        .wait(Some(Duration::from_secs(10)))
        .metrics(
            |m| m.state == openraft::ServerState::Shutdown,
            "the group shuts down",
        )
        .await;
    assert!(shut.is_ok(), "{shut:?}");
    drop(tasks);
}

/// A stop ends a steward whose pass waits on a proposal that cannot commit,
/// here with no quorum, rather than holding the node's shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_ends_a_steward_whose_pass_cannot_commit() {
    let served = three_served(&AuthResolver::static_only(None), &SharedAuditSink::new()).await;
    let follower = super::super::serving_tests::formed(&served.nodes).await;
    let leader = served.nodes[follower]
        .host
        .raft(0)
        .expect("hosted")
        .metrics()
        .borrow_watched()
        .current_leader
        .expect("a leader");
    for node in served.nodes.iter().filter(|n| n.id != leader) {
        node.host.shutdown().await.expect("shutdown");
    }
    let leading = served
        .nodes
        .iter()
        .find(|n| n.id == leader)
        .expect("the leader");
    let (stop, stopped) = watch::channel(false);
    let tasks = leading.run(&background(&served.store), &stopped);
    // Long enough for a pass to start proposing the miner configuration.
    tokio::time::sleep(Duration::from_millis(1_500)).await;

    stop.send_replace(true);
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        for task in tasks {
            task.await.expect("stopped");
        }
    })
    .await;

    assert!(ended.is_ok(), "the stop ends every task");
}
