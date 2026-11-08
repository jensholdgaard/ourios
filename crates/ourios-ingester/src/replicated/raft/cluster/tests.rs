//! Groups on the in-process cluster: log repair after a failover, a
//! crash under §8's model, two groups on one host, and a host whose
//! group fails to open.

use std::sync::Arc;
use std::time::Duration;

use openraft::EntryPayload;
use openraft::async_runtime::WatchReceiver as _;
use tokio::task::JoinHandle;

use openraft::type_config::alias::StoredMembershipOf;

use super::super::host::HostError;
use super::super::snapshot_record::{GroupSnapshot, IdBlock};
use super::super::types::{Entry, GroupEntry, LogId};
use super::{
    Cluster, GroupHost, GroupRaft, HostSpec, NodeId, Peers, RaftTypes, Replica, WAIT, batch,
    group_dir, name, propose, wal_config, write,
};

const GROUP: u32 = 0;

/// Write `ns` through node `i`, returning the last index.
async fn commit(cluster: &Cluster, i: usize, ns: std::ops::Range<u64>) -> u64 {
    let mut last = 0;
    for n in ns {
        last = write(cluster.node(i).raft(GROUP), n).await;
    }
    last
}

/// Node `i`'s log once it equals node `j`'s: an election after the last
/// write can append a blank entry to one before the other.
async fn converged(cluster: &Cluster, i: usize, j: usize) -> Vec<Entry> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (log, other) = (
            cluster.node(i).log(GROUP).await,
            cluster.node(j).log(GROUP).await,
        );
        if log == other {
            return log;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "node {i}'s log never matched node {j}'s:\n{log:?}\n{other:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Cut node `i` off from `others`, have it append an entry it cannot
/// commit, and return that entry and the write's task.
async fn strand(
    cluster: &Cluster,
    i: usize,
    others: &[usize],
    after: u64,
) -> (Entry, JoinHandle<Result<(), String>>) {
    for &other in others {
        cluster.cut(i, other);
    }
    let raft = cluster.node(i).raft(GROUP).clone();
    let write = tokio::spawn(async move {
        raft.client_write(batch(100).into())
            .await
            .map(drop)
            .map_err(|e| e.to_string())
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let log = cluster.node(i).log(GROUP).await;
        if let Some(last) = log.last().filter(|e| e.log_id.index > after) {
            return (last.clone(), write);
        }
        assert!(
            !write.is_finished(),
            "the write ended before appending: {:?}",
            write.await
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "node {i} appended nothing past {after}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Scenario RFC0065.7 — a diverged follower is repaired (B6).
/// A leader cut off from both followers appends an entry it can never
/// commit and is killed; the others elect a leader and commit past it.
/// The old leader comes back holding that entry; once its links heal its
/// log converges to the new leader's and no committed entry is removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0065_7_a_diverged_follower_is_repaired() {
    let mut cluster = Cluster::start(3, &[GROUP]).await;
    let old = cluster.leader(GROUP, None).await;
    let committed = commit(&cluster, old, 0..3).await;
    for i in 0..3 {
        cluster.node(i).applied_through(GROUP, committed).await;
    }
    let before = cluster.node(old).log(GROUP).await;
    assert_eq!(before.last().map(|e| e.log_id.index), Some(committed));

    let others: Vec<usize> = (0..3).filter(|&i| i != old).collect();
    let (diverged, stranded) = strand(&cluster, old, &others, committed).await;
    cluster.kill(old).await;
    let stranded = tokio::time::timeout(Duration::from_secs(10), stranded)
        .await
        .expect("the stranded write ends")
        .expect("join");
    assert!(stranded.is_err(), "the cut-off write never committed");

    let new = cluster.leader(GROUP, Some(old)).await;
    let last = commit(&cluster, new, 3..6).await;
    cluster.restart(old).await;
    let held = cluster.node(old).log(GROUP).await;
    assert!(
        held.contains(&diverged),
        "the returning replica holds the uncommitted entry"
    );

    for &i in &others {
        cluster.heal(old, i);
    }
    cluster.node(old).applied_through(GROUP, last).await;
    let repaired = converged(&cluster, old, new).await;
    assert!(
        !repaired.contains(&diverged),
        "the uncommitted entry is gone"
    );
    for entry in &before {
        let kept = repaired
            .iter()
            .find(|e| e.log_id.index == entry.log_id.index);
        assert_eq!(kept, Some(entry), "committed index {}", entry.log_id.index);
    }
    cluster.shutdown().await;
}

fn assign(flusher: NodeId) -> GroupEntry {
    GroupEntry::Control(ourios_wal::Control::FlusherAssignment(
        *flusher.uuid().as_bytes(),
    ))
}

/// A committed flusher assignment names the flusher on every replica, a
/// later one replaces it, and a restarted replica names it again from its
/// log; the batches around them commit as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_replica_applies_the_last_flusher_assignment() {
    let mut cluster = Cluster::start(3, &[GROUP]).await;
    let leader = cluster.leader(GROUP, None).await;
    let (first, second) = (cluster.id(1), cluster.id(2));
    commit(&cluster, leader, 0..2).await;
    propose(cluster.node(leader).raft(GROUP), assign(first)).await;
    let flusher_of = |cluster: &Cluster, i: usize| {
        let sm = cluster.node(i).host.state_machine(GROUP).expect("hosted");
        sm.flusher()
    };
    let last = commit(&cluster, leader, 2..4).await;
    for i in 0..3 {
        cluster.node(i).applied_through(GROUP, last).await;
        assert_eq!(flusher_of(&cluster, i), Some(first), "node {i}");
    }

    let follower = (leader + 1) % 3;
    cluster.kill(follower).await;
    cluster.restart(follower).await;
    cluster.node(follower).applied_through(GROUP, last).await;
    assert_eq!(flusher_of(&cluster, follower), Some(first), "replayed");

    propose(cluster.node(leader).raft(GROUP), assign(second)).await;
    let last = commit(&cluster, leader, 4..6).await;
    for i in 0..3 {
        cluster.node(i).applied_through(GROUP, last).await;
        assert_eq!(flusher_of(&cluster, i), Some(second), "node {i}");
    }
    let log = converged(&cluster, follower, leader).await;
    assert_eq!(normal(&log), 8, "six batches and two assignments");
    cluster.shutdown().await;
}

/// A follower crashed with appends written but not fsynced loses only
/// those, restarts from what it fsynced, and catches up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashed_follower_restarts_from_its_fsynced_log_and_catches_up() {
    let mut cluster = Cluster::start(3, &[GROUP]).await;
    let leader = cluster.leader(GROUP, None).await;
    let follower = (leader + 1) % 3;
    let first = write(cluster.node(leader).raft(GROUP), 0).await;
    cluster.node(follower).applied_through(GROUP, first).await;

    let gate = Arc::clone(&cluster.node(follower).gate);
    gate.close();
    let last = commit(&cluster, leader, 1..5).await;
    tokio::task::spawn_blocking(move || gate.wait_for_writer())
        .await
        .expect("join");
    let dropped = cluster.crash(follower).await;
    assert!(dropped > 0, "the held appends were on disk, unsynced");

    cluster.restart(follower).await;
    cluster.node(follower).applied_through(GROUP, last).await;
    converged(&cluster, follower, leader).await;
    cluster.shutdown().await;
}

/// A checkpoint entry proposed by `proposer` with its cut at `cut`, where
/// the log's highest kind is the flusher assignment's.
fn checkpoint(proposer: NodeId, cut: &Entry) -> GroupEntry {
    checkpoint_of_kind(proposer, cut, 1)
}

fn checkpoint_of_kind(proposer: NodeId, cut: &Entry, highest_kind: u8) -> GroupEntry {
    GroupEntry::Control(ourios_wal::Control::Checkpoint {
        proposer: *proposer.uuid().as_bytes(),
        cut: ourios_wal::RaftLogId {
            term: cut.log_id.leader_id.term,
            index: cut.log_id.index,
        },
        highest_kind,
    })
}

/// Node `i`'s entry at `index`.
async fn entry_at(cluster: &Cluster, i: usize, index: u64) -> Entry {
    let log = cluster.node(i).log(GROUP).await;
    log.into_iter()
        .find(|e| e.log_id.index == index)
        .expect("in the log")
}

/// Node `i`'s snapshot, flusher and membership once it has applied
/// through `index` and snapshotted and purged its log through `cut`,
/// both of which follow the apply.
async fn group_on(
    cluster: &Cluster,
    i: usize,
    (index, cut): (u64, LogId),
) -> (GroupSnapshot, GroupView) {
    let node = cluster.node(i);
    node.applied_through(GROUP, index).await;
    let cut = Some(cut);
    node.raft(GROUP)
        .wait(WAIT)
        .metrics(
            |m| m.snapshot >= cut && m.purged >= cut,
            "snapshotted and purged through the cut",
        )
        .await
        .expect("purged");
    let sm = node.host.state_machine(GROUP).expect("hosted");
    let snapshot = sm.snapshot().expect("a snapshot");
    let first = node.log(GROUP).await.first().map(|e| e.log_id);
    assert!(
        first > cut,
        "node {i} keeps {first:?}, below the cut {cut:?}"
    );
    (snapshot, (sm.flusher(), sm.membership()))
}

type GroupView = (Option<NodeId>, StoredMembershipOf<RaftTypes>);

/// RFC0065.31's arms on the flusher, membership and highest kind: after
/// a flusher change, a learner added and a checkpoint by a replica that is
/// not the flusher, the flusher's checkpoint snapshots the group at its
/// cut and purges the log through it; a replica whose log ends below the
/// cut installs the snapshot and ends as one that applied the whole log,
/// and a restart reopens from the snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_checkpoint_snapshots_the_group_at_its_cut_for_every_replica() {
    let mut cluster = Cluster::start_with_tail(3, 1, &[GROUP]).await;
    let leader = cluster.leader(GROUP, None).await;
    let (lagging, voter) = ((leader + 1) % 3, (leader + 2) % 3);
    cluster.kill(lagging).await;
    let (first, second, learner) = (cluster.id(leader), cluster.id(voter), cluster.id(3));
    let raft = cluster.node(leader).raft(GROUP);
    propose(raft, assign(first)).await;
    let cut = entry_at(&cluster, leader, commit(&cluster, leader, 0..2).await).await;
    let sm = cluster
        .node(leader)
        .host
        .state_machine(GROUP)
        .expect("hosted");
    let membership_at_cut = sm.membership();
    let reassigned = propose(raft, assign(second)).await;
    raft.add_learner(learner, name(learner), true)
        .await
        .expect("learner added");
    let not_the_flusher = entry_at(&cluster, leader, reassigned).await;
    propose(raft, checkpoint(first, &not_the_flusher)).await;
    propose(raft, checkpoint(second, &cut)).await;
    let last = commit(&cluster, leader, 2..4).await;

    let (snapshot, whole_log) = group_on(&cluster, leader, (last, cut.log_id)).await;
    assert_eq!(
        snapshot.meta.last_log_id,
        Some(cut.log_id),
        "the flusher's cut"
    );
    assert_eq!(snapshot.meta.last_membership, membership_at_cut);
    assert_eq!(
        (snapshot.state.flusher, snapshot.state.highest_kind),
        (Some(first), 1)
    );
    assert_eq!(whole_log.0, Some(second));
    assert!(whole_log.1.nodes().any(|(&id, _)| id == learner));
    assert_eq!(
        group_on(&cluster, 3, (last, cut.log_id)).await,
        (snapshot.clone(), whole_log.clone())
    );

    cluster.restart(lagging).await;
    let installed = group_on(&cluster, lagging, (last, cut.log_id)).await;
    assert_eq!(
        installed,
        (snapshot.clone(), whole_log.clone()),
        "installed and caught up"
    );

    cluster.kill(leader).await;
    cluster.restart(leader).await;
    let reopened = group_on(&cluster, leader, (last, cut.log_id)).await;
    assert_eq!(
        reopened,
        (snapshot, whole_log),
        "reopened from the snapshot"
    );
    cluster.shutdown().await;
}

fn id_block((after, through): (u64, u64)) -> GroupEntry {
    GroupEntry::Control(ourios_wal::Control::IdBlock { after, through })
}

fn id_block_on(cluster: &Cluster, i: usize) -> Option<IdBlock> {
    let sm = cluster.node(i).host.state_machine(GROUP).expect("hosted");
    sm.id_block()
}

/// RFC0065.31's id-block arms: with a block before a checkpoint's cut and
/// blocks after it, the snapshot holds the one in force at the cut; a
/// replica whose log ends below the cut installs it and ends with the same
/// latest block as one that applied the whole log, ignoring the blocks
/// below `(0, 100]` though that block's entry was purged before it ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_checkpoint_carries_the_id_block_in_force_at_its_cut() {
    let mut cluster = Cluster::start(3, &[GROUP]).await;
    let leader = cluster.leader(GROUP, None).await;
    let (lagging, voter) = ((leader + 1) % 3, (leader + 2) % 3);
    cluster.kill(lagging).await;
    let flusher = cluster.id(leader);
    let raft = cluster.node(leader).raft(GROUP);
    propose(raft, assign(flusher)).await;
    propose(raft, id_block((0, 100))).await;
    let cut = entry_at(&cluster, leader, commit(&cluster, leader, 0..2).await).await;
    for block in [(0, 50), (50, 150), (100, 200)] {
        propose(raft, id_block(block)).await;
    }
    propose(raft, checkpoint_of_kind(flusher, &cut, 3)).await;
    let last = commit(&cluster, leader, 2..4).await;

    let (snapshot, _) = group_on(&cluster, leader, (last, cut.log_id)).await;
    assert_eq!(snapshot.state.id_block, IdBlock::new(0, 100), "at the cut");
    let latest = IdBlock::new(100, 200);
    cluster.node(voter).applied_through(GROUP, last).await;
    for i in [leader, voter] {
        assert_eq!(id_block_on(&cluster, i), latest, "node {i}");
    }

    cluster.restart(lagging).await;
    let (installed, _) = group_on(&cluster, lagging, (last, cut.log_id)).await;
    assert_eq!(installed, snapshot, "installed");
    assert_eq!(id_block_on(&cluster, lagging), latest, "caught up");
    cluster.shutdown().await;
}

fn normal(log: &[Entry]) -> usize {
    log.iter()
        .filter(|e| matches!(e.payload, EntryPayload::Normal(_)))
        .count()
}

/// Write `writes` entries to `group` through its leader.
async fn write_to(cluster: &Cluster, group: u32, writes: u64) {
    let leader = cluster.leader(group, None).await;
    for n in 0..writes {
        write(cluster.node(leader).raft(group), n).await;
    }
}

/// Every node's log of `group` is its leader's, which holds `writes`
/// entries of its own.
async fn assert_holds_only(cluster: &Cluster, group: u32, writes: u64) {
    let leader = cluster.leader(group, None).await;
    let expected = cluster.node(leader).log(group).await;
    let last = expected.last().expect("entries").log_id.index;
    assert_eq!(normal(&expected) as u64, writes, "group {group}");
    for i in 0..3 {
        cluster.node(i).applied_through(group, last).await;
        let mut log = cluster.node(i).log(group).await;
        log.retain(|e| e.log_id.index <= last);
        assert_eq!(log, expected, "node {i}, group {group}");
    }
}

/// Two groups on the same three nodes keep separate logs: each node's log
/// of a group is its leader's, holding only that group's writes, after a
/// node restarts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_groups_on_the_same_nodes_keep_separate_logs() {
    const WRITES: [(u32, u64); 2] = [(0, 3), (1, 1)];
    let mut cluster = Cluster::start(3, &[0, 1]).await;
    for (group, writes) in WRITES {
        write_to(&cluster, group, writes).await;
    }
    cluster.kill(2).await;
    cluster.restart(2).await;
    for (group, writes) in WRITES {
        assert_holds_only(&cluster, group, writes).await;
    }
    cluster.shutdown().await;
}

/// A group that cannot be opened fails the host and stops the groups
/// opened before it, so the root opens again once the fault is gone.
#[tokio::test]
async fn a_group_that_fails_to_open_stops_the_ones_before_it() {
    let replica = Replica::new(1, &[0, 1]);
    let blocked = group_dir(replica.dir.path(), 1);
    std::fs::create_dir_all(blocked.parent().expect("the groups directory")).expect("mkdir");
    std::fs::write(&blocked, b"").expect("a file where group 1's directory goes");
    let spec = || HostSpec {
        id: replica.id,
        wal: wal_config(replica.dir.path()),
        groups: replica.groups.clone(),
        peers: Peers::plaintext(),
    };
    match GroupHost::open(spec()).await {
        Err(HostError::Log { group: 1, .. }) => {}
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("opened with group 1 blocked"),
    }

    std::fs::remove_file(&blocked).expect("unblock");
    let host = tokio::time::timeout(Duration::from_secs(10), GroupHost::open(spec()))
        .await
        .expect("group 0's log and the vote log were released")
        .expect("open");
    assert_eq!(host.groups().collect::<Vec<_>>(), [0, 1]);
    host.shutdown().await.expect("shutdown");
}

/// The highest index `leader` has seen node `i` match in `group`.
fn matched(cluster: &Cluster, leader: usize, group: u32, i: usize) -> Option<u64> {
    let metrics = cluster
        .node(leader)
        .raft(group)
        .metrics()
        .borrow_watched()
        .clone();
    let replication = metrics.replication.expect("the leader's replication");
    replication
        .get(&cluster.id(i))
        .copied()
        .flatten()
        .map(|id| id.index)
}

/// Wait until the leader `raft` has seen node `id` match `index`: a node
/// applying an entry does not mean its ack has reached the leader yet.
async fn seen_matching(raft: &GroupRaft, id: NodeId, index: u64) {
    raft.wait(WAIT)
        .metrics(
            |m| {
                let replication = m.replication.as_ref();
                let seen = replication.and_then(|r| r.get(&id).copied().flatten());
                seen.map(|log_id| log_id.index) == Some(index)
            },
            "the leader sees the match",
        )
        .await
        .expect("the leader sees the match");
}

/// With node `held`'s fsyncs stopped, commit four writes to `group` and check
/// its leader never saw `held` match past `synced`.
async fn commit_past(cluster: &Cluster, group: u32, held: usize, synced: u64) -> u64 {
    let leader = cluster.leader(group, None).await;
    let mut last = 0;
    for n in 1..5 {
        last = write(cluster.node(leader).raft(group), n).await;
    }
    assert_eq!(
        matched(cluster, leader, group, held),
        Some(synced),
        "group {group}"
    );
    last
}

/// A crash takes every group on a node at once. Each group's log keeps
/// exactly what it had fsynced, which no peer saw it match past, and
/// each catches up once the node is back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_keeps_each_group_on_the_node_through_its_own_last_fsync() {
    const GROUPS: [u32; 2] = [0, 1];
    let mut cluster = Cluster::start(3, &GROUPS).await;
    let mut leaders = Vec::new();
    for group in GROUPS {
        leaders.push(cluster.leader(group, None).await);
    }
    let held = (0..3)
        .find(|i| !leaders.contains(i))
        .expect("a node leading neither group");
    let mut synced = Vec::new();
    for (group, &leader) in GROUPS.iter().zip(&leaders) {
        let index = write(cluster.node(leader).raft(*group), 0).await;
        cluster.node(held).applied_through(*group, index).await;
        let raft = cluster.node(leader).raft(*group);
        seen_matching(raft, cluster.id(held), index).await;
        synced.push(index);
    }

    let gate = Arc::clone(&cluster.node(held).gate);
    gate.close();
    let mut last = Vec::new();
    for (&group, &index) in GROUPS.iter().zip(&synced) {
        last.push(commit_past(&cluster, group, held, index).await);
    }
    assert!(
        cluster.crash(held).await > 0,
        "appends were written, unsynced"
    );

    let others: Vec<_> = (0..3).filter(|&i| i != held).collect();
    for &other in &others {
        cluster.cut(held, other);
    }
    cluster.restart(held).await;
    for (&group, &index) in GROUPS.iter().zip(&synced) {
        let log = cluster.node(held).log(group).await;
        assert_eq!(
            log.last().map(|e| e.log_id.index),
            Some(index),
            "group {group}"
        );
    }
    for &other in &others {
        cluster.heal(held, other);
    }
    for (&group, &index) in GROUPS.iter().zip(&last) {
        cluster.node(held).applied_through(group, index).await;
    }
    cluster.shutdown().await;
}
