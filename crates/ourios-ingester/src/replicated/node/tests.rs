use std::fs;
use std::num::NonZeroU32;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::Duration;

use ourios_parquet::Store;

use super::{NodeError, NodeStart, start};
use crate::replicated::layout::{LAYOUT_KEY, VotingLayout, record_or_check};
use crate::replicated::raft::cluster::{test_timings, wal_config};
use crate::replicated::raft::host::HostRole;
use crate::replicated::raft::log_ids::gated::{Fault, faulty, with_outage};
use crate::replicated::raft::log_ids::{LOG_IDS_KEY, RaftState, raft_state};
use crate::replicated::raft::log_store::group_dir;
use crate::replicated::raft::transport::Peers;
use crate::replicated::replica_id::REPLICA_ID_FILE;
use crate::template_ids::ScanOptions;

const VOTERS: [&str; 3] = ["n0", "n1", "n2"];

fn layout(voters: &[&str]) -> VotingLayout {
    VotingLayout::new(voters.iter().copied(), NonZeroU32::MIN, 3)
}

/// Node `n0`'s start, under `wal_root`, against `store`.
fn n0(wal_root: &Path, store: &Store) -> NodeStart {
    n0_named("n0", wal_root, store, Peers::plaintext())
}

/// Node `name`'s start among [`VOTERS`], reaching them through `peers`.
pub(super) fn n0_named(name: &str, wal_root: &Path, store: &Store, peers: Peers) -> NodeStart {
    let layout = layout(&VOTERS);
    NodeStart {
        node_id: name.to_owned(),
        wal: wal_config(wal_root),
        groups: layout.groups_of(name),
        layout,
        role: HostRole::Voter,
        peers,
        timings: test_timings(),
        store: store.clone(),
        allow_bootstrap: true,
        scan: ScanOptions::default(),
        retry: Duration::from_millis(10),
    }
}

fn exists(store: &Store, key: &str) -> bool {
    store.get_blocking_opt(key).expect("readable").is_some()
}

/// RFC 0065 §6.1, §6.6: a fresh node records the voting layout, creates
/// its identity, passes the bootstrap lock and opens its groups.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_node_records_its_layout_and_identity_and_opens_its_groups() {
    let wal_root = tempfile::tempdir().expect("tempdir");
    let store = Store::in_memory();

    let node = start(n0(wal_root.path(), &store), &mut |_| {})
        .await
        .expect("started");

    assert_eq!(node.host.groups().collect::<Vec<_>>(), [0]);
    assert!(wal_root.path().join(REPLICA_ID_FILE).exists());
    assert!(exists(&store, LAYOUT_KEY));
    assert!(exists(&store, LOG_IDS_KEY), "the lock was passed");
    node.host.shutdown().await.expect("shutdown");
}

/// RFC0065.24's unreadable-disk arm: a node that cannot read its groups'
/// directories fails its start and writes nothing, here or on the store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_cannot_read_its_disk_starts_nothing() {
    let wal_root = tempfile::tempdir().expect("tempdir");
    let store = Store::in_memory();
    let dir = group_dir(wal_root.path(), 0);
    fs::create_dir_all(&dir).expect("group dir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).expect("chmod");

    let refused = start(n0(wal_root.path(), &store), &mut |_| {}).await;

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("restore");
    assert!(
        matches!(refused, Err(NodeError::RaftState(_))),
        "{:?}",
        refused.err()
    );
    assert!(!wal_root.path().join(REPLICA_ID_FILE).exists());
    assert!(!exists(&store, LAYOUT_KEY));
    assert!(!exists(&store, LOG_IDS_KEY));
}

/// A node whose configured layout differs from the recorded one could
/// never join: its start fails before it creates an identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_whose_layout_differs_creates_no_identity() {
    let wal_root = tempfile::tempdir().expect("tempdir");
    let store = Store::in_memory();
    record_or_check(&store, &layout(&["n0", "n1", "n9"])).expect("recorded");

    let refused = start(n0(wal_root.path(), &store), &mut |_| {}).await;

    assert!(
        matches!(refused, Err(NodeError::Layout(_))),
        "{:?}",
        refused.err()
    );
    assert!(!wal_root.path().join(REPLICA_ID_FILE).exists());
    assert!(!exists(&store, LOG_IDS_KEY));
}

/// RFC0065.31, RFC 0065 §6.6: a replica holding Raft state, restarted with
/// the store unreachable, opens its groups and keeps taking the log; the
/// voting layout it cannot read is not checked until a start that can.
/// One holding none still fails, having nothing to open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_holding_raft_state_starts_while_the_store_is_unreachable() {
    let wal_root = tempfile::tempdir().expect("tempdir");
    let (store, outage) = with_outage(Store::in_memory());
    let node = start(n0(wal_root.path(), &store), &mut |_| {})
        .await
        .expect("started");
    node.host.shutdown().await.expect("shutdown");
    // Dropping the host releases the vote log the next start takes.
    drop(node);
    assert_eq!(
        raft_state(wal_root.path(), &[0]).ok(),
        Some(RaftState::Held)
    );
    outage.begin();

    let node = start(n0(wal_root.path(), &store), &mut |_| {})
        .await
        .expect("a node holding Raft state starts while the store is out");
    node.host.shutdown().await.expect("shutdown");
    drop(node);

    let fresh = tempfile::tempdir().expect("tempdir");
    let refused = start(n0(fresh.path(), &store), &mut |_| {}).await;
    assert!(
        matches!(refused, Err(NodeError::Layout(_))),
        "{:?}",
        refused.err()
    );
}

/// The outage exception covers only a layout the node cannot read: one it
/// cannot record, the store read but `cluster.json` gone, still fails the
/// start, so a node never runs without a recorded layout to be checked
/// against.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_layout_the_node_cannot_record_still_fails_its_start() {
    let wal_root = tempfile::tempdir().expect("tempdir");
    let store = Store::in_memory();
    let node = start(n0(wal_root.path(), &store), &mut |_| {})
        .await
        .expect("started");
    node.host.shutdown().await.expect("shutdown");
    drop(node);
    store
        .delete_blocking(LAYOUT_KEY)
        .expect("delete the layout");

    let failing = faulty(store.clone(), Fault::FailLayoutCreate);
    let refused = start(n0(wal_root.path(), &failing), &mut |_| {}).await;

    assert!(
        matches!(refused, Err(NodeError::Layout(_))),
        "{:?}",
        refused.err()
    );
}

/// The unchecked start is reported once, as the registered event with
/// `error.type` `store_read`.
#[test]
fn an_unchecked_layout_is_reported_once() {
    let wal_root = tempfile::tempdir().expect("tempdir");
    let (store, outage) = with_outage(Store::in_memory());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let node = start(n0(wal_root.path(), &store), &mut |_| {})
            .await
            .expect("started");
        node.host.shutdown().await.expect("shutdown");
    });
    outage.begin();

    let logged = crate::replicated::raft::telemetry::storage_tests::events(|| {
        runtime.block_on(async {
            let node = start(n0(wal_root.path(), &store), &mut |_| {})
                .await
                .expect("started");
            node.host.shutdown().await.expect("shutdown");
        });
    });

    let unchecked: Vec<_> = logged
        .into_iter()
        .filter(|(name, _)| name == ourios_semconv::EVENT_OURIOS_WAL_REPLICATION_LAYOUT_UNCHECKED)
        .collect();
    assert_eq!(unchecked.len(), 1, "{unchecked:?}");
    assert_eq!(unchecked[0].1, ["error.type=store_read".to_owned()]);
}
