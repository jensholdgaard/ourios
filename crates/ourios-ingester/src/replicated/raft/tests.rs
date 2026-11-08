//! The group log against `openraft`'s storage suite, across reopen, and
//! under §8's crash model: each segment cut back to the offset the last
//! `Wal::sync` returned.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use openraft::storage::{IOFlushed, RaftLogReader, RaftLogStorage};
use openraft::testing::log::{StoreBuilder, Suite};
use openraft::type_config::TypeConfigExt;
use openraft::{EntryPayload, ErrorSubject, ErrorVerb, StorageError};
use ourios_wal::TenantBatch;
use tempfile::TempDir;

use super::cluster::{cut_to, wal_config};
use super::codec::raft_id;
use super::gate::SyncGate;
use super::log_store::{GroupLog, group_dir};
use super::state_machine::GroupSm;
use super::types::{
    Entry, GroupEntry, LogId, Membership, NodeId, RaftTypes, TenantBatchBytes, Vote,
};
use super::vote_log::{self, VoteLog};

const GROUP: u32 = 3;

fn open(wal_root: &Path) -> (GroupLog, Arc<SyncGate>) {
    vote_log::create(wal_root).expect("create");
    let votes = VoteLog::open(wal_root).expect("vote log");
    let gate = Arc::new(SyncGate::default());
    let config = wal_config(&group_dir(wal_root, GROUP));
    let log = GroupLog::open_gated(config, GROUP, votes, Arc::clone(&gate)).expect("group log");
    (log, gate)
}

fn log_id(term: u64, index: u64) -> LogId {
    raft_id(ourios_wal::RaftLogId { term, index })
}

fn entry(term: u64, index: u64) -> Entry {
    let batch = TenantBatch::encode("acme", format!("record {index}").as_bytes()).expect("batch");
    Entry {
        log_id: log_id(term, index),
        payload: EntryPayload::Normal(TenantBatchBytes::new(batch).expect("valid").into()),
    }
}

fn entries(term: u64, indexes: std::ops::RangeInclusive<u64>) -> Vec<Entry> {
    indexes.map(|i| entry(term, i)).collect()
}

/// Append and return the receiver its callback completes.
async fn append(
    log: &mut GroupLog,
    batch: Vec<Entry>,
) -> impl Future<Output = io::Result<()>> + use<> {
    let (tx, rx) = RaftTypes::oneshot();
    log.append(batch, IOFlushed::signal(tx))
        .await
        .expect("append");
    async move { rx.await.map_err(|_| io::Error::other("callback dropped"))? }
}

async fn indexes(log: &mut GroupLog) -> Vec<u64> {
    let all = log.try_get_log_entries(..).await.expect("read");
    all.iter().map(|e| e.log_id.index).collect()
}

struct Builder;

// The suite's builder trait is async; opening the log is not.
#[allow(clippy::unused_async_trait_impl)]
impl StoreBuilder<RaftTypes, GroupLog, GroupSm, TempDir> for Builder {
    async fn build(&self) -> Result<(TempDir, GroupLog, GroupSm), StorageError<RaftTypes>> {
        let store_err = |e| StorageError::from_io_error(ErrorSubject::Store, ErrorVerb::Read, e);
        let dir = tempfile::tempdir().map_err(store_err)?;
        vote_log::create(dir.path()).map_err(store_err)?;
        let votes = VoteLog::open(dir.path()).map_err(store_err)?;
        let config = wal_config(&group_dir(dir.path(), GROUP));
        let log = GroupLog::open(config, GROUP, votes).map_err(store_err)?;
        let sm = GroupSm::open(&group_dir(dir.path(), GROUP), Arc::default()).map_err(store_err)?;
        Ok((dir, log, sm))
    }
}

#[tokio::test]
async fn the_group_log_passes_openraft_storage_suite() {
    Suite::test_all(Builder)
        .await
        .expect("openraft storage suite");
}

#[tokio::test]
async fn a_reopened_log_restores_what_was_written() {
    let dir = tempfile::tempdir().expect("tempdir");
    let vote = Vote::new_committed(4, NodeId::from(2));
    let nodes = [(NodeId::from(1), "ourios-receiver-0".to_owned())];
    let membership = Membership::new(
        vec![BTreeSet::from([NodeId::from(1)])],
        BTreeMap::from(nodes),
    )
    .expect("membership");
    {
        let (mut log, _) = open(dir.path());
        let mut batch = vec![Entry {
            log_id: log_id(1, 0),
            payload: EntryPayload::Membership(membership.clone()),
        }];
        batch.extend(entries(1, 1..=9));
        append(&mut log, batch).await.await.expect("fsynced");
        append(&mut log, entries(2, 6..=12))
            .await
            .await
            .expect("fsynced");
        log.save_vote(&vote).await.expect("vote");
        log.save_committed(Some(log_id(2, 8)))
            .await
            .expect("committed");
        log.truncate_after(Some(log_id(2, 10)))
            .await
            .expect("truncate");
        log.purge(log_id(1, 2)).await.expect("purge");
        log.shutdown();
    }
    let (mut log, _) = open(dir.path());
    let state = log.get_log_state().await.expect("state");
    assert_eq!(state.last_purged_log_id, Some(log_id(1, 2)));
    assert_eq!(state.last_log_id, Some(log_id(2, 10)));
    assert_eq!(indexes(&mut log).await, (3..=10).collect::<Vec<_>>());
    let read = log.try_get_log_entries(5..=6).await.expect("read");
    assert_eq!(read, vec![entry(1, 5), entry(2, 6)]);
    assert_eq!(log.read_vote().await.expect("vote"), Some(vote));
    assert_eq!(
        log.read_committed().await.expect("committed"),
        Some(log_id(2, 8))
    );
}

#[tokio::test]
async fn a_crash_keeps_every_fsynced_record_and_nothing_else() {
    let dir = tempfile::tempdir().expect("tempdir");
    let synced = {
        let (mut log, gate) = open(dir.path());
        append(&mut log, entries(1, 0..=5))
            .await
            .await
            .expect("fsynced");
        gate.close();
        let unsynced = append(&mut log, entries(1, 6..=8)).await;
        gate.wait_for_writer();
        let hooks = log.hooks();
        let truncate = tokio::spawn(async move { log.truncate_after(Some(log_id(1, 2))).await });
        tokio::task::yield_now().await;
        let synced = hooks.crash().expect("a sync happened");
        assert!(
            unsynced.await.is_err(),
            "the cut append was never acknowledged"
        );
        assert!(
            truncate.await.expect("join").is_err(),
            "nor the cut truncation"
        );
        synced
    };
    let lost = cut_to(&group_dir(dir.path(), GROUP), synced);
    assert!(lost > 0, "the unsynced frames were on disk before the cut");
    let (mut log, _) = open(dir.path());
    assert_eq!(indexes(&mut log).await, (0..=5).collect::<Vec<_>>());
    let state = log.get_log_state().await.expect("state");
    assert_eq!(state.last_log_id, Some(log_id(1, 5)));
}

#[tokio::test]
async fn a_crash_keeps_fsynced_markers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let synced = {
        let (mut log, gate) = open(dir.path());
        append(&mut log, entries(1, 0..=9))
            .await
            .await
            .expect("fsynced");
        log.truncate_after(Some(log_id(1, 7)))
            .await
            .expect("truncate");
        log.purge(log_id(1, 1)).await.expect("purge");
        gate.close();
        let unsynced = append(&mut log, entries(2, 8..=9)).await;
        gate.wait_for_writer();
        let synced = log.crash().expect("a sync happened");
        assert!(unsynced.await.is_err());
        synced
    };
    let lost = cut_to(&group_dir(dir.path(), GROUP), synced);
    assert!(lost > 0, "the unsynced frames were on disk before the cut");
    let (mut log, _) = open(dir.path());
    assert_eq!(indexes(&mut log).await, (2..=7).collect::<Vec<_>>());
    let state = log.get_log_state().await.expect("state");
    assert_eq!(state.last_purged_log_id, Some(log_id(1, 1)));
}

#[tokio::test]
async fn one_fsync_completes_every_append_queued_behind_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut log, gate) = open(dir.path());
    let opened = gate.passed();
    gate.close();
    let first = append(&mut log, entries(1, 0..=0)).await;
    gate.wait_for_writer();
    let mut queued = Vec::new();
    for i in 1..=200 {
        queued.push(append(&mut log, entries(1, i..=i)).await);
    }
    assert_eq!(
        indexes(&mut log).await.len(),
        201,
        "appended entries are readable at once"
    );
    gate.open();
    first.await.expect("first");
    for flushed in queued {
        flushed.await.expect("queued");
    }
    assert_eq!(
        gate.passed() - opened,
        2,
        "the 200 queued appends shared one fsync"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_stalled_fsync_does_not_block_the_runtime() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut log, gate) = open(dir.path());
    gate.close();
    let mut flushed = std::pin::pin!(append(&mut log, entries(1, 0..=3)).await);
    gate.wait_for_writer();
    let mut reader = log.get_log_reader().await;
    let mut truncate = tokio::spawn(async move { log.truncate_after(Some(log_id(1, 1))).await });
    let stalled = tokio::time::timeout(Duration::from_millis(50), &mut flushed).await;
    assert!(stalled.is_err(), "no callback before the fsync");
    let stalled = tokio::time::timeout(Duration::from_millis(50), &mut truncate).await;
    assert!(stalled.is_err(), "no truncate return before its fsync");
    let read = reader.try_get_log_entries(..).await.expect("read");
    assert_eq!(
        read,
        entries(1, 0..=1),
        "reads answer while the disk stalls"
    );
    gate.open();
    flushed.await.expect("flushed");
    truncate.await.expect("join").expect("truncate");
}

#[tokio::test]
async fn an_append_that_leaves_a_hole_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut log, _) = open(dir.path());
    append(&mut log, entries(1, 0..=2))
        .await
        .await
        .expect("fsynced");
    let (tx, _rx) = RaftTypes::oneshot();
    let err = log
        .append(entries(1, 4..=4), IOFlushed::signal(tx))
        .await
        .expect_err("a hole");
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert_eq!(indexes(&mut log).await, vec![0, 1, 2]);
}

#[tokio::test]
async fn a_vote_waits_for_every_earlier_write_of_its_group() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut log, gate) = open(dir.path());
    gate.close();
    let mut flushed = std::pin::pin!(append(&mut log, entries(1, 0..=3)).await);
    gate.wait_for_writer();
    let vote = Vote::new(2, NodeId::from(7));
    let mut reader = log.get_log_reader().await;
    let mut saved = tokio::spawn(async move { log.save_vote(&vote).await });
    let stalled = tokio::time::timeout(Duration::from_millis(50), &mut saved).await;
    assert!(stalled.is_err(), "the vote waits behind the stalled append");
    assert_eq!(reader.read_vote().await.expect("vote"), None);
    gate.open();
    saved.await.expect("join").expect("vote");
    let done = futures::FutureExt::now_or_never(flushed.as_mut());
    assert!(matches!(done, Some(Ok(()))), "the append was durable first");
    assert_eq!(reader.read_vote().await.expect("vote"), Some(vote));
}

#[tokio::test]
async fn an_earlier_wal_fsync_failure_fails_a_later_vote() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut log, gate) = open(dir.path());
    gate.fail();
    let flushed = append(&mut log, entries(1, 0..=1)).await;
    assert!(flushed.await.is_err(), "the append's fsync failed");
    let vote = Vote::new(2, NodeId::from(7));
    assert!(log.save_vote(&vote).await.is_err());
    assert_eq!(log.read_vote().await.expect("vote"), None);
}

/// The group's directories are left to `Wal::open`, whose root
/// preparation fsyncs every directory it creates and the one holding the
/// outermost (`ourios-wal`'s `prepare_root` tests pin those fsyncs).
#[tokio::test]
async fn a_fresh_wal_root_gets_its_group_tree_from_the_wal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wal_root = dir.path().join("node");
    {
        let (mut log, _) = open(&wal_root);
        append(&mut log, entries(1, 0..=1))
            .await
            .await
            .expect("fsynced");
        log.shutdown();
    }
    let segments = std::fs::read_dir(group_dir(&wal_root, GROUP))
        .expect("group dir")
        .filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.path().extension().is_some_and(|x| x == "wal"))
        })
        .count();
    assert_eq!(segments, 1);
    let (mut log, _) = open(&wal_root);
    assert_eq!(indexes(&mut log).await, vec![0, 1]);
}

#[tokio::test]
async fn a_recovered_log_is_fsynced_before_its_index_is_exposed() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let (mut log, _) = open(dir.path());
        append(&mut log, entries(1, 0..=2))
            .await
            .await
            .expect("fsynced");
        log.shutdown();
    }
    let (mut log, gate) = open(dir.path());
    assert_eq!(
        gate.passed(),
        3,
        "the open fsynced groups/, wal_root and the replayed WAL"
    );
    assert_eq!(indexes(&mut log).await, vec![0, 1, 2]);
}

#[test]
fn a_failed_recovery_fsync_fails_the_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    vote_log::create(dir.path()).expect("create");
    let votes = VoteLog::open(dir.path()).expect("vote log");
    let gate = Arc::new(SyncGate::default());
    gate.fail_after(2);
    let config = wal_config(&group_dir(dir.path(), GROUP));
    assert!(GroupLog::open_gated(config, GROUP, votes, Arc::clone(&gate)).is_err());
    assert_eq!(
        gate.passed(),
        2,
        "the directory fsyncs passed; the WAL's failed"
    );
}

/// Run `open` on a thread and report whether it is still waiting after
/// `patience`; the caller then releases the old writer and joins.
fn open_in_background<T: Send + 'static>(
    open: impl FnOnce() -> T + Send + 'static,
    patience: Duration,
) -> (bool, std::thread::JoinHandle<T>) {
    let (opened_tx, opened) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let value = open();
        let _ = opened_tx.send(());
        value
    });
    (opened.recv_timeout(patience).is_err(), handle)
}

async fn queue_behind_a_stalled_fsync(log: &mut GroupLog, gate: &SyncGate) {
    append(log, entries(1, 0..=2)).await.await.expect("fsynced");
    gate.close();
    let queued = append(log, entries(1, 3..=5)).await;
    gate.wait_for_writer();
    drop(queued);
}

#[tokio::test]
async fn a_reopen_waits_for_a_retiring_group_writer_with_queued_appends() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut log, gate) = open(dir.path());
    queue_behind_a_stalled_fsync(&mut log, &gate).await;
    drop(log);
    let root = dir.path().to_path_buf();
    let (waiting, reopen) = open_in_background(move || open(&root).0, Duration::from_millis(200));
    assert!(
        waiting,
        "the reopen waits while the old writer holds queued appends"
    );
    gate.open();
    let mut log = reopen.join().expect("join");
    let read = log.try_get_log_entries(..).await.expect("read");
    assert_eq!(read, entries(1, 0..=5), "nothing lost or reordered");
}

#[tokio::test]
async fn a_group_reopen_waits_for_its_retiring_writer_while_the_vote_log_lives() {
    let dir = tempfile::tempdir().expect("tempdir");
    vote_log::create(dir.path()).expect("create");
    let votes = VoteLog::open(dir.path()).expect("vote log");
    let gate = Arc::new(SyncGate::default());
    let config = wal_config(&group_dir(dir.path(), GROUP));
    let open_group = {
        let votes = votes.clone();
        move |gate| GroupLog::open_gated(config, GROUP, votes, gate).expect("group log")
    };
    let mut log = open_group.clone()(Arc::clone(&gate));
    queue_behind_a_stalled_fsync(&mut log, &gate).await;
    drop(log);
    let (waiting, reopen) = open_in_background(
        move || open_group(Arc::default()),
        Duration::from_millis(200),
    );
    assert!(
        waiting,
        "the group lock holds the reopen, though the vote log is live"
    );
    gate.open();
    let mut log = reopen.join().expect("join");
    let read = log.try_get_log_entries(..).await.expect("read");
    assert_eq!(read, entries(1, 0..=5));
    drop(votes);
}

#[tokio::test]
async fn a_lost_group_wal_with_a_surviving_vote_fails_the_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let (mut log, _) = open(dir.path());
        append(&mut log, entries(1, 0..=1))
            .await
            .await
            .expect("fsynced");
        log.save_vote(&Vote::new_committed(3, NodeId::from(1)))
            .await
            .expect("vote");
        log.shutdown();
    }
    std::fs::remove_dir_all(group_dir(dir.path(), GROUP)).expect("lose the group WAL");
    let votes = VoteLog::open(dir.path()).expect("vote log");
    let config = wal_config(&group_dir(dir.path(), GROUP));
    let err = GroupLog::open(config, GROUP, votes)
        .err()
        .expect("must refuse");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert!(err.to_string().contains("new replica"), "{err}");
    assert!(
        !group_dir(dir.path(), GROUP).exists(),
        "no fresh WAL was made"
    );
}

#[tokio::test]
async fn a_vote_with_an_empty_log_opens_while_its_segments_exist() {
    let dir = tempfile::tempdir().expect("tempdir");
    let vote = Vote::new(2, NodeId::from(1));
    {
        let (mut log, _) = open(dir.path());
        log.save_vote(&vote).await.expect("vote");
        log.shutdown();
    }
    let (mut log, _) = open(dir.path());
    assert_eq!(log.read_vote().await.expect("vote"), Some(vote));
    assert!(indexes(&mut log).await.is_empty());
}

#[tokio::test]
async fn a_purged_empty_log_admits_only_the_index_after_the_purge() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut log, _) = open(dir.path());
    append(&mut log, entries(1, 0..=3))
        .await
        .await
        .expect("fsynced");
    log.purge(log_id(1, 3)).await.expect("purge");
    let (tx, _rx) = RaftTypes::oneshot();
    let err = log
        .append(entries(1, 5..=5), IOFlushed::signal(tx))
        .await
        .expect_err("index 5 after a purge to 3");
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    append(&mut log, entries(1, 4..=4))
        .await
        .await
        .expect("fsynced");
    assert_eq!(indexes(&mut log).await, vec![4]);
}

#[tokio::test]
async fn a_panicking_writer_fails_the_queued_callbacks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut log, gate) = open(dir.path());
    gate.panic();
    let flushed = append(&mut log, entries(1, 0..=1)).await;
    let err = flushed.await.expect_err("the writer panicked");
    assert!(
        err.to_string().contains("writer thread has stopped"),
        "the guard completed the callback: {err}"
    );
}

/// A first open interrupted after creating `groups/<g>/` but before its
/// fsyncs leaves the retry with an existing root, which `prepare_root`
/// fsyncs alone; the open itself must fsync the two directories above it.
#[tokio::test]
async fn a_retried_first_open_fsyncs_the_group_roots_ancestors() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(group_dir(dir.path(), GROUP)).expect("an interrupted first open");
    let (mut log, gate) = open(dir.path());
    assert_eq!(
        gate.passed(),
        3,
        "groups/ and wal_root are fsynced before the replayed WAL"
    );
    append(&mut log, entries(1, 0..=0))
        .await
        .await
        .expect("fsynced");
}

#[test]
fn a_failed_ancestor_fsync_fails_the_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    vote_log::create(dir.path()).expect("create");
    let votes = VoteLog::open(dir.path()).expect("vote log");
    let gate = Arc::new(SyncGate::default());
    gate.fail_after(1);
    let config = wal_config(&group_dir(dir.path(), GROUP));
    assert!(GroupLog::open_gated(config, GROUP, votes, Arc::clone(&gate)).is_err());
    assert_eq!(
        gate.passed(),
        1,
        "groups/ was fsynced; wal_root's fsync failed"
    );
}

/// RFC0065.29, the log arm: a control entry this replica does not apply
/// yet stops the group log's open rather than being skipped, and the
/// entries before it are what a later release would replay.
#[tokio::test]
#[ignore = "superseded: control entries apply since kind 0x01 (RFC 0065 §6.5)"]
async fn a_control_entry_this_replica_cannot_apply_stops_the_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let (mut log, _) = open(dir.path());
        append(&mut log, entries(1, 0..=2))
            .await
            .await
            .expect("fsynced");
        log.shutdown();
    }
    let config = wal_config(&group_dir(dir.path(), GROUP));
    let mut wal = ourios_wal::Wal::open(config).expect("the group WAL");
    let control = ourios_wal::ReplicatedRecord::Entry {
        log_id: ourios_wal::RaftLogId { term: 1, index: 3 },
        body: ourios_wal::EntryBody::Control(ourios_wal::Control::FlusherAssignment([7; 16])),
    };
    let frame = control.encode().expect("encode");
    wal.append(ourios_wal::FrameKind::ReplicatedEntry, &frame)
        .expect("append");
    wal.sync().expect("sync");
    drop(wal);

    let refused = std::thread::spawn({
        let root = dir.path().to_path_buf();
        move || {
            vote_log::create(&root).expect("create");
            let votes = VoteLog::open(&root).expect("vote log");
            let config = wal_config(&group_dir(&root, GROUP));
            GroupLog::open(config, GROUP, votes).map(|_| ())
        }
    })
    .join()
    .expect("join");
    let error = refused.expect_err("the control entry stops the open");
    assert!(error.to_string().contains("control entry"), "{error}");
}

/// Append `frames`, each a `0x04` payload, to the group WAL after `entries`
/// were appended through the log, and open the log again.
async fn reopen_after(wal_root: &Path, entries: Vec<Entry>, frames: &[Vec<u8>]) -> io::Result<()> {
    {
        let (mut log, _) = open(wal_root);
        append(&mut log, entries).await.await.expect("fsynced");
        log.shutdown();
    }
    let mut wal = ourios_wal::Wal::open(wal_config(&group_dir(wal_root, GROUP))).expect("WAL");
    for frame in frames {
        wal.append(ourios_wal::FrameKind::ReplicatedEntry, frame)
            .expect("append");
    }
    wal.sync().expect("sync");
    drop(wal);
    let root = wal_root.to_path_buf();
    std::thread::spawn(move || {
        let votes = VoteLog::open(&root).expect("vote log");
        let config = wal_config(&group_dir(&root, GROUP));
        GroupLog::open(config, GROUP, votes).map(drop)
    })
    .join()
    .expect("join")
}

fn flusher(term: u64, index: u64, replica: u64) -> Entry {
    let replica = *NodeId::from(replica).uuid().as_bytes();
    Entry {
        log_id: log_id(term, index),
        payload: EntryPayload::Normal(GroupEntry::Control(ourios_wal::Control::FlusherAssignment(
            replica,
        ))),
    }
}

/// RFC0065.29, the log arm: a flusher assignment reopens as written, and
/// a control kind above the highest this release reads stops the open
/// rather than being skipped.
#[tokio::test]
async fn a_flusher_assignment_reopens_and_an_unknown_kind_stops_the_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut written = entries(1, 0..=1);
    written.push(flusher(1, 2, 7));
    reopen_after(dir.path(), written.clone(), &[])
        .await
        .expect("an assignment reopens");
    let (mut log, _) = open(dir.path());
    let all = log.try_get_log_entries(..).await.expect("read");
    assert_eq!(all, written);
    log.shutdown();

    let mut unknown = ourios_wal::ReplicatedRecord::Entry {
        log_id: ourios_wal::RaftLogId { term: 1, index: 3 },
        body: ourios_wal::EntryBody::Control(ourios_wal::Control::FlusherAssignment([7; 16])),
    }
    .encode()
    .expect("encode");
    unknown[17] = ourios_wal::Control::HIGHEST_KIND + 1;
    let dir = tempfile::tempdir().expect("tempdir");
    let refused = reopen_after(dir.path(), written, &[unknown]).await;
    let error = refused.expect_err("the unknown kind stops the open");
    let kind = format!(
        "control kind 0x{:02x}",
        ourios_wal::Control::HIGHEST_KIND + 1
    );
    assert!(error.to_string().contains(&kind), "{error}");
}

fn assignment(at: LogId, replica: NodeId) -> Entry {
    let uuid = replica.uuid();
    Entry {
        log_id: at,
        payload: EntryPayload::Normal(GroupEntry::Control(ourios_wal::Control::FlusherAssignment(
            *uuid.as_bytes(),
        ))),
    }
}

/// The log's control view follows the entries it holds, committed or
/// not: an entry replaced or truncated takes its kind and assignment with
/// it, a purged one keeps only its kind, and a reopen restores the view.
#[tokio::test]
async fn the_log_controls_follow_its_entries() {
    use super::log_store::LogControls;

    let dir = tempfile::tempdir().expect("tempdir");
    let (mut log, _) = open(dir.path());
    let reader = log.get_log_reader().await;
    assert_eq!(reader.controls(), LogControls::default());

    let mut batch = entries(1, 1..=2);
    batch.extend([
        assignment(log_id(1, 3), NodeId::from(7)),
        entry(1, 4),
        assignment(log_id(1, 5), NodeId::from(9)),
    ]);
    append(&mut log, batch).await.await.expect("fsynced");
    let both = LogControls {
        highest_kind: 1,
        introduced: Some(3),
        flusher: Some(NodeId::from(9)),
    };
    assert_eq!(reader.controls(), both);

    append(&mut log, entries(2, 5..=6))
        .await
        .await
        .expect("fsynced");
    let first = LogControls {
        flusher: Some(NodeId::from(7)),
        ..both
    };
    assert_eq!(reader.controls(), first, "a replaced assignment is gone");

    log.truncate_after(Some(log_id(1, 2)))
        .await
        .expect("truncate");
    assert_eq!(reader.controls(), LogControls::default());

    append(
        &mut log,
        vec![assignment(log_id(3, 3), NodeId::from(7)), entry(3, 4)],
    )
    .await
    .await
    .expect("fsynced");
    log.purge(log_id(3, 3)).await.expect("purge");
    let purged = LogControls {
        highest_kind: 1,
        introduced: None,
        flusher: None,
    };
    assert_eq!(reader.controls(), purged, "a purged entry keeps its kind");
    drop(reader);
    log.shutdown();

    let (mut log, _) = open(dir.path());
    let reopened = log.get_log_reader().await.controls();
    assert_eq!(reopened, purged);
}

/// Over the group's snapshot, the view takes the snapshot's flusher and
/// kind for what the log purged, and an assignment after the snapshot
/// takes precedence.
#[tokio::test]
async fn the_log_controls_fall_back_to_the_snapshot() {
    use super::log_store::LogControls;
    use super::snapshot_record::{GroupSnapshot, GroupState, SnapshotMeta};

    let dir = tempfile::tempdir().expect("tempdir");
    let (mut log, _) = open(dir.path());
    let reader = log.get_log_reader().await;
    let mut batch = entries(1, 1..=2);
    batch.push(assignment(log_id(1, 3), NodeId::from(7)));
    batch.extend(entries(1, 4..=5));
    append(&mut log, batch).await.await.expect("fsynced");
    log.purge(log_id(1, 4)).await.expect("purge");
    assert_eq!(reader.controls().flusher, None, "the log alone lost it");

    let snapshot = GroupSnapshot {
        meta: SnapshotMeta {
            last_log_id: Some(log_id(1, 4)),
            ..SnapshotMeta::default()
        },
        state: GroupState {
            flusher: Some(NodeId::from(7)),
            highest_kind: 1,
            id_block: None,
        },
    };
    let over = LogControls {
        highest_kind: 1,
        introduced: None,
        flusher: Some(NodeId::from(7)),
    };
    assert_eq!(reader.held_controls().over(Some(&snapshot)), over);

    let newer = vec![assignment(log_id(1, 6), NodeId::from(9))];
    append(&mut log, newer).await.await.expect("fsynced");
    let after = reader.held_controls().over(Some(&snapshot));
    assert_eq!(after.flusher, Some(NodeId::from(9)), "the log's newer one");
    assert_eq!(after.introduced, None, "the snapshot committed the kind");
}

/// The controls read from the log before a purge, over the snapshot
/// recorded for that purge, still name the purged assignment's flusher:
/// the order `Group::controls` reads them in.
#[tokio::test]
async fn controls_read_before_a_purge_over_its_snapshot_keep_the_flusher() {
    use super::snapshot_record::{GroupSnapshot, GroupState, SnapshotMeta};

    let dir = tempfile::tempdir().expect("tempdir");
    let (mut log, _) = open(dir.path());
    let reader = log.get_log_reader().await;
    let mut batch = entries(1, 1..=2);
    batch.push(assignment(log_id(1, 3), NodeId::from(7)));
    batch.extend(entries(1, 4..=5));
    append(&mut log, batch).await.await.expect("fsynced");

    let before_purge = reader.held_controls();
    let recorded = GroupSnapshot {
        meta: SnapshotMeta {
            last_log_id: Some(log_id(1, 4)),
            ..SnapshotMeta::default()
        },
        state: GroupState {
            flusher: Some(NodeId::from(7)),
            highest_kind: 1,
            id_block: None,
        },
    };
    log.purge(log_id(1, 4)).await.expect("purge");
    let after_purge = reader.held_controls();

    for (held, read) in [(before_purge, "before"), (after_purge, "after")] {
        let flusher = held.over(Some(&recorded)).flusher;
        assert_eq!(flusher, Some(NodeId::from(7)), "log read {read} the purge");
    }
}
