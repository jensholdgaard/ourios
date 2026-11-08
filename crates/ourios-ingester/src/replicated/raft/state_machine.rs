//! A group's state machine (RFC 0065 §6.1): what it has applied, the
//! membership it applied last, and the flusher the last flusher
//! assignment named (§6.5).
//!
//! It lives in memory, except for its snapshot: `openraft` purges the log
//! up to a snapshot it installs or builds, so the snapshot is recorded
//! in `wal_root/groups/<group>/snapshot` before the call returns,
//! and a restart opens applied through it and replays the log after it.
//! The record holds the flusher and the highest control kind with the
//! membership, so a replica that opens from or installs it knows them
//! before it applies any entry after it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures_util::TryStreamExt as _;
use openraft::storage::{EntryResponder, RaftSnapshotBuilder, RaftStateMachine};
use openraft::type_config::alias::{SnapshotOf, StoredMembershipOf};
use openraft::{EntryPayload, OptionalSend};
use ourios_wal::Control;
use uuid::Uuid;

use super::gate::SyncGate;
use super::snapshot_record::{self, GroupSnapshot, GroupState, SnapshotMeta};
use super::types::{GroupEntry, LogId, NodeId, RaftTypes};

type StoredMembership = StoredMembershipOf<RaftTypes>;

struct State {
    applied: Option<LogId>,
    membership: StoredMembership,
    group: GroupState,
    snapshot: Option<GroupSnapshot>,
}

/// Where the snapshot is recorded, and the gate its fsyncs pass.
struct Record {
    dir: PathBuf,
    gate: Arc<SyncGate>,
    /// Held by a build or an install from before it reads or replaces the
    /// snapshot until its record and memory both hold the new one;
    /// `openraft` builds on its own task, so the two can overlap.
    publishing: tokio::sync::Mutex<()>,
}

/// Clones share one state.
#[derive(Clone)]
pub struct GroupSm {
    state: Arc<Mutex<State>>,
    record: Arc<Record>,
}

impl GroupSm {
    /// The state machine of the group whose WAL is in `dir`, applied
    /// through the snapshot recorded there, if any. Blocking.
    ///
    /// # Errors
    ///
    /// The record cannot be read, or fails its CRC or version check.
    pub(super) fn open(dir: &Path, gate: Arc<SyncGate>) -> io::Result<Self> {
        let snapshot = snapshot_record::read(dir)?;
        let state = State {
            applied: snapshot.as_ref().and_then(|s| s.meta.last_log_id),
            membership: snapshot
                .as_ref()
                .map(|s| s.meta.last_membership.clone())
                .unwrap_or_default(),
            group: snapshot.as_ref().map(|s| s.state).unwrap_or_default(),
            snapshot,
        };
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
            record: Arc::new(Record {
                dir: dir.to_path_buf(),
                gate,
                publishing: tokio::sync::Mutex::new(()),
            }),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record `snapshot` durably, off the runtime's workers, then make it
    /// the current one. The caller holds `publishing`.
    async fn record(&self, snapshot: GroupSnapshot) -> io::Result<()> {
        let record = Arc::clone(&self.record);
        let written = snapshot.clone();
        tokio::task::spawn_blocking(move || {
            snapshot_record::write(&record.dir, &written, &record.gate)
        })
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)))?;
        self.state().snapshot = Some(snapshot);
        Ok(())
    }

    /// The last entry applied.
    #[must_use]
    pub fn applied(&self) -> Option<LogId> {
        self.state().applied
    }

    #[must_use]
    pub fn membership(&self) -> StoredMembership {
        self.state().membership.clone()
    }

    /// The replica the last applied flusher assignment named.
    #[must_use]
    pub fn flusher(&self) -> Option<NodeId> {
        self.state().group.flusher
    }
}

fn apply_control(group: &mut GroupState, control: Control) {
    group.highest_kind = group.highest_kind.max(control.kind());
    match control {
        Control::FlusherAssignment(replica) => {
            group.flusher = Some(NodeId::from(Uuid::from_bytes(replica)));
        }
    }
}

impl RaftSnapshotBuilder<RaftTypes> for GroupSm {
    type SnapshotData = GroupState;

    async fn build_snapshot(&mut self) -> Result<SnapshotOf<RaftTypes, GroupState>, io::Error> {
        let record = Arc::clone(&self.record);
        let _publishing = record.publishing.lock().await;
        // Read once publishing is ours: an install that overlapped this
        // build is then in the state, so it never records an older one.
        let snapshot = {
            let state = self.state();
            GroupSnapshot {
                meta: SnapshotMeta {
                    last_log_id: state.applied,
                    last_membership: state.membership.clone(),
                },
                state: state.group,
            }
        };
        self.record(snapshot.clone()).await?;
        Ok(snapshot.into())
    }
}

// openraft's traits are async; only the snapshot calls wait, on the
// record's fsyncs.
#[allow(clippy::unused_async_trait_impl)]
impl RaftStateMachine<RaftTypes> for GroupSm {
    type SnapshotData = GroupState;
    type SnapshotBuilder = Self;

    async fn applied_state(&mut self) -> Result<(Option<LogId>, StoredMembership), io::Error> {
        let state = self.state();
        Ok((state.applied, state.membership.clone()))
    }

    async fn apply<S>(&mut self, mut entries: S) -> Result<(), io::Error>
    where
        S: futures_util::Stream<Item = Result<EntryResponder<RaftTypes>, io::Error>>
            + Unpin
            + OptionalSend,
    {
        while let Some((entry, responder)) = entries.try_next().await? {
            {
                let mut state = self.state();
                state.applied = Some(entry.log_id);
                match entry.payload {
                    EntryPayload::Membership(m) => {
                        state.membership = StoredMembership::new(Some(entry.log_id), m);
                    }
                    EntryPayload::Normal(GroupEntry::Control(control)) => {
                        apply_control(&mut state.group, control);
                    }
                    EntryPayload::Blank | EntryPayload::Normal(GroupEntry::Batch(_)) => {}
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
        meta: &SnapshotMeta,
        group: GroupState,
    ) -> Result<(), io::Error> {
        let record = Arc::clone(&self.record);
        let _publishing = record.publishing.lock().await;
        let snapshot = GroupSnapshot {
            meta: meta.clone(),
            state: group,
        };
        self.record(snapshot).await?;
        let mut state = self.state();
        state.applied = meta.last_log_id;
        state.membership = meta.last_membership.clone();
        state.group = group;
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<SnapshotOf<RaftTypes, GroupState>>, io::Error> {
        let snapshot = self.state().snapshot.clone();
        Ok(snapshot.map(Into::into))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use openraft::vote::leader_id_std::CommittedLeaderId;

    use super::super::types::Membership;

    use super::*;

    fn at(index: u64) -> SnapshotMeta {
        SnapshotMeta {
            last_log_id: Some(LogId {
                leader_id: CommittedLeaderId::new(2),
                index,
            }),
            last_membership: StoredMembership::default(),
        }
    }

    fn entry(
        index: u64,
        payload: EntryPayload<GroupEntry, Membership>,
    ) -> EntryResponder<RaftTypes> {
        let log_id = LogId {
            leader_id: CommittedLeaderId::new(2),
            index,
        };
        (super::super::types::Entry { log_id, payload }, None)
    }

    fn assign(index: u64, replica: u64) -> EntryResponder<RaftTypes> {
        let replica = *NodeId::from(replica).uuid().as_bytes();
        let control = GroupEntry::Control(Control::FlusherAssignment(replica));
        entry(index, EntryPayload::Normal(control))
    }

    fn batch(index: u64) -> EntryResponder<RaftTypes> {
        let payload = ourios_wal::TenantBatch::encode("acme", b"p").expect("batch");
        let batch = super::super::types::TenantBatchBytes::new(payload).expect("valid");
        entry(index, EntryPayload::Normal(GroupEntry::Batch(batch)))
    }

    async fn apply(sm: &mut GroupSm, entries: Vec<EntryResponder<RaftTypes>>) {
        let stream = futures_util::stream::iter(entries.into_iter().map(Ok));
        sm.apply(stream).await.expect("apply");
    }

    /// The last assignment names the flusher; batches and blanks leave it.
    #[tokio::test]
    async fn the_last_flusher_assignment_applied_names_the_flusher() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sm = GroupSm::open(dir.path(), Arc::default()).expect("open");
        apply(&mut sm, vec![batch(1)]).await;
        assert_eq!(sm.flusher(), None);
        apply(&mut sm, vec![assign(2, 7), batch(3)]).await;
        assert_eq!(sm.flusher(), Some(NodeId::from(7)));
        apply(
            &mut sm,
            vec![entry(4, EntryPayload::Blank), assign(5, 9), batch(6)],
        )
        .await;
        assert_eq!(sm.flusher(), Some(NodeId::from(9)));
        assert_eq!(sm.applied().map(|id| id.index), Some(6));
    }

    /// A snapshot holds no flusher, so an install leaves none known.
    #[tokio::test]
    #[ignore = "superseded: RFC 0065 snapshot record v2 carries the flusher"]
    async fn an_installed_snapshot_leaves_no_flusher_known() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sm = GroupSm::open(dir.path(), Arc::default()).expect("open");
        apply(&mut sm, vec![assign(1, 7)]).await;
        sm.install_snapshot(&at(5), GroupState::default())
            .await
            .expect("install");
        assert_eq!(sm.flusher(), None);
    }

    /// An install takes the flusher the snapshot names, and a reopen
    /// restores it from the record.
    #[tokio::test]
    async fn an_installed_snapshot_names_its_flusher() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sm = GroupSm::open(dir.path(), Arc::default()).expect("open");
        apply(&mut sm, vec![assign(1, 7)]).await;
        let group = GroupState {
            flusher: Some(NodeId::from(9)),
            highest_kind: 1,
            id_block: None,
        };
        sm.install_snapshot(&at(5), group).await.expect("install");
        assert_eq!(sm.flusher(), Some(NodeId::from(9)));
        let reopened = GroupSm::open(dir.path(), Arc::default()).expect("reopen");
        assert_eq!(reopened.flusher(), Some(NodeId::from(9)));
    }

    /// A build records the flusher and the highest control kind applied,
    /// and a reopen restores the flusher from it.
    #[tokio::test]
    async fn a_built_snapshot_records_the_flusher_and_highest_kind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sm = GroupSm::open(dir.path(), Arc::default()).expect("open");
        apply(&mut sm, vec![batch(1)]).await;
        let before = sm.build_snapshot().await.expect("build");
        assert_eq!(before.snapshot, GroupState::default());
        apply(&mut sm, vec![assign(2, 7), batch(3)]).await;
        let built = sm.build_snapshot().await.expect("build");
        let expected = GroupState {
            flusher: Some(NodeId::from(7)),
            highest_kind: 1,
            id_block: None,
        };
        assert_eq!(built.snapshot, expected);
        let reopened = GroupSm::open(dir.path(), Arc::default()).expect("reopen");
        assert_eq!(reopened.flusher(), Some(NodeId::from(7)));
        assert_eq!(reopened.applied().map(|id| id.index), Some(3));
    }

    /// A build that starts while an install is held at its fsync waits for
    /// it, then builds from the installed state rather than recording the
    /// older one it would have read first.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_build_overlapping_an_install_records_nothing_older() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gate = Arc::new(SyncGate::default());
        let sm = GroupSm::open(dir.path(), Arc::clone(&gate)).expect("open");
        sm.clone()
            .install_snapshot(&at(5), GroupState::default())
            .await
            .expect("install 5");

        gate.close();
        let mut installing = sm.clone();
        let install = tokio::spawn(async move {
            installing
                .install_snapshot(&at(9), GroupState::default())
                .await
        });
        let held = Arc::clone(&gate);
        tokio::task::spawn_blocking(move || held.wait_for_writer())
            .await
            .expect("join");
        let mut building = sm.clone();
        let build = tokio::spawn(async move { building.build_snapshot().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        gate.open();

        install.await.expect("join").expect("install 9");
        let snapshot = build.await.expect("join").expect("build");
        assert_eq!(snapshot.meta.last_log_id, at(9).last_log_id);
        let reopened = GroupSm::open(dir.path(), Arc::default()).expect("reopen");
        assert_eq!(reopened.applied(), at(9).last_log_id);
    }
}
