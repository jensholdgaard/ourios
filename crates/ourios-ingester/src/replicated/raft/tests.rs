//! The group log against `openraft`'s storage suite, across reopen, and
//! under §8's crash model: each segment cut back to the offset the last
//! `Wal::sync` returned.

// openraft's traits are async; the test state machine answers from memory.
#![allow(clippy::unused_async_trait_impl)]

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::TryStreamExt;
use openraft::storage::{
    EntryResponder, IOFlushed, RaftLogReader, RaftLogStorage, RaftSnapshotBuilder, RaftStateMachine,
};
use openraft::testing::log::{StoreBuilder, Suite};
use openraft::type_config::TypeConfigExt;
use openraft::type_config::alias::{SnapshotMetaOf, SnapshotOf, StoredMembershipOf};
use openraft::{EntryPayload, ErrorSubject, ErrorVerb, OptionalSend, StorageError};
use ourios_wal::{TenantBatch, WalConfig, WalOffset};
use tempfile::TempDir;

use super::codec::raft_id;
use super::gate::SyncGate;
use super::log_store::{GroupLog, group_dir};
use super::types::{Entry, LogId, Membership, NodeId, RaftTypes, TenantBatchBytes, Vote};
use super::vote_log::VoteLog;

const GROUP: u32 = 3;

fn wal_config(root: &Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 2,
        segment_size_bytes: ourios_wal::MIN_SEGMENT_SIZE_BYTES,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
        rotation_retry_attempts: ourios_wal::DEFAULT_ROTATION_RETRY_ATTEMPTS,
        macos_full_fsync: false,
    }
}

fn open(wal_root: &Path) -> (GroupLog, Arc<SyncGate>) {
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
        payload: EntryPayload::Normal(TenantBatchBytes::new(batch).expect("valid")),
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

/// §8's crash model: drop every byte past the boundary `synced` names.
/// Returns the bytes dropped.
fn cut_to(dir: &Path, synced: WalOffset) -> u64 {
    let mut lost = 0;
    for dirent in std::fs::read_dir(dir).expect("read_dir") {
        let path = dirent.expect("dirent").path();
        let Some(uuid) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".wal"))
            .and_then(|stem| uuid::Uuid::parse_str(stem).ok())
        else {
            continue;
        };
        let len = std::fs::metadata(&path).expect("stat").len();
        if uuid > synced.segment {
            lost += len;
            std::fs::remove_file(&path).expect("remove");
        } else if uuid == synced.segment {
            lost += len - synced.byte;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("open");
            file.set_len(synced.byte).expect("cut");
        }
    }
    lost
}

#[derive(Default)]
struct SmState {
    applied: Option<LogId>,
    membership: StoredMembershipOf<RaftTypes>,
    snapshot: Option<SnapshotMetaOf<RaftTypes>>,
}

/// The suite needs a state machine; this one holds only what it asserts.
#[derive(Clone, Default)]
struct MemSm(Arc<Mutex<SmState>>);

impl MemSm {
    fn state(&self) -> std::sync::MutexGuard<'_, SmState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl RaftSnapshotBuilder<RaftTypes> for MemSm {
    type SnapshotData = ();

    async fn build_snapshot(&mut self) -> Result<SnapshotOf<RaftTypes, ()>, io::Error> {
        let mut state = self.state();
        let meta = SnapshotMetaOf::<RaftTypes> {
            last_log_id: state.applied,
            last_membership: state.membership.clone(),
        };
        state.snapshot = Some(meta.clone());
        Ok(SnapshotOf::<RaftTypes, ()> { meta, snapshot: () })
    }
}

impl RaftStateMachine<RaftTypes> for MemSm {
    type SnapshotData = ();
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogId>, StoredMembershipOf<RaftTypes>), io::Error> {
        let state = self.state();
        Ok((state.applied, state.membership.clone()))
    }

    async fn apply<S>(&mut self, mut entries: S) -> Result<(), io::Error>
    where
        S: futures::Stream<Item = Result<EntryResponder<RaftTypes>, io::Error>>
            + Unpin
            + OptionalSend,
    {
        while let Some((entry, responder)) = entries.try_next().await? {
            {
                let mut state = self.state();
                state.applied = Some(entry.log_id);
                if let EntryPayload::Membership(m) = entry.payload {
                    state.membership = StoredMembershipOf::<RaftTypes>::new(Some(entry.log_id), m);
                }
            }
            if let Some(responder) = responder {
                responder.send(());
            }
        }
        Ok(())
    }

    async fn get_snapshot_builder(&mut self) -> Self {
        self.clone()
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMetaOf<RaftTypes>,
        (): (),
    ) -> Result<(), io::Error> {
        let mut state = self.state();
        state.applied = meta.last_log_id;
        state.membership = meta.last_membership.clone();
        state.snapshot = Some(meta.clone());
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<SnapshotOf<RaftTypes, ()>>, io::Error> {
        let snapshot = self.state().snapshot.clone();
        Ok(snapshot.map(|meta| SnapshotOf::<RaftTypes, ()> { meta, snapshot: () }))
    }
}

struct Builder;

impl StoreBuilder<RaftTypes, GroupLog, MemSm, TempDir> for Builder {
    async fn build(&self) -> Result<(TempDir, GroupLog, MemSm), StorageError<RaftTypes>> {
        let store_err = |e| StorageError::from_io_error(ErrorSubject::Store, ErrorVerb::Read, e);
        let dir = tempfile::tempdir().map_err(store_err)?;
        let votes = VoteLog::open(dir.path()).map_err(store_err)?;
        let config = wal_config(&group_dir(dir.path(), GROUP));
        let log = GroupLog::open(config, GROUP, votes).map_err(store_err)?;
        Ok((dir, log, MemSm::default()))
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
        let mut truncating = log.clone();
        let truncate =
            tokio::spawn(async move { truncating.truncate_after(Some(log_id(1, 2))).await });
        tokio::task::yield_now().await;
        let synced = log.crash().expect("a sync happened");
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
    let mut truncating = log.clone();
    let mut truncate =
        tokio::spawn(async move { truncating.truncate_after(Some(log_id(1, 1))).await });
    let stalled = tokio::time::timeout(Duration::from_millis(50), &mut flushed).await;
    assert!(stalled.is_err(), "no callback before the fsync");
    let stalled = tokio::time::timeout(Duration::from_millis(50), &mut truncate).await;
    assert!(stalled.is_err(), "no truncate return before its fsync");
    let state = log.get_log_state().await.expect("state");
    assert_eq!(
        state.last_log_id,
        Some(log_id(1, 1)),
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
    let mut voting = log.clone();
    let mut saved = tokio::spawn(async move { voting.save_vote(&vote).await });
    let stalled = tokio::time::timeout(Duration::from_millis(50), &mut saved).await;
    assert!(stalled.is_err(), "the vote waits behind the stalled append");
    assert_eq!(log.read_vote().await.expect("vote"), None);
    gate.open();
    saved.await.expect("join").expect("vote");
    let done = futures::FutureExt::now_or_never(flushed.as_mut());
    assert!(matches!(done, Some(Ok(()))), "the append was durable first");
    assert_eq!(log.read_vote().await.expect("vote"), Some(vote));
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
    assert_eq!(gate.passed(), 1, "the open fsynced the replayed WAL");
    assert_eq!(indexes(&mut log).await, vec![0, 1, 2]);
}

#[test]
fn a_failed_recovery_fsync_fails_the_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let votes = VoteLog::open(dir.path()).expect("vote log");
    let gate = Arc::new(SyncGate::default());
    gate.fail();
    let config = wal_config(&group_dir(dir.path(), GROUP));
    assert!(GroupLog::open_gated(config, GROUP, votes, gate).is_err());
}
