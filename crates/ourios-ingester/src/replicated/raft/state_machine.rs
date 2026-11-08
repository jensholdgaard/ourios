//! A group's state machine (RFC 0065 §6.1): what it has applied, the
//! membership it applied last, and the flusher the last flusher
//! assignment named (§6.5).
//!
//! It lives in memory, except for its snapshot: `openraft` purges the log
//! up to a snapshot it installs or builds, so the snapshot is recorded
//! in `wal_root/groups/<group>/snapshot` before the call returns,
//! and a restart opens applied through it and replays the log after it.
//! The record holds the flusher, the highest control kind and the latest
//! id block with the membership, so a replica that opens from or installs
//! it knows them before it applies any entry after it.
//!
//! A snapshot is only ever the group at a checkpoint's cut: applying a
//! checkpoint entry publishes its cut on `GroupSm::checkpoints`, the
//! host triggers the build, and the builder records the checkpoint's
//! snapshot rather than the state at the last entry applied.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures_util::TryStreamExt as _;
use openraft::storage::{EntryResponder, RaftSnapshotBuilder, RaftStateMachine};
use openraft::type_config::alias::{SnapshotOf, StoredMembershipOf};
use openraft::{EntryPayload, OptionalSend};
use ourios_wal::Control;
use tokio::sync::watch;
use uuid::Uuid;

use super::checkpoint::{Change, History, Proposal};
use super::gate::SyncGate;
use super::snapshot_record::{self, GroupSnapshot, GroupState, IdBlock, SnapshotMeta};
use super::types::{GroupEntry, LogId, Membership, NodeId, RaftTypes};

type StoredMembership = StoredMembershipOf<RaftTypes>;

struct State {
    applied: Option<LogId>,
    membership: StoredMembership,
    group: GroupState,
    history: History,
    /// The snapshot recorded last.
    snapshot: Option<GroupSnapshot>,
}

impl State {
    fn opened(snapshot: Option<GroupSnapshot>) -> Self {
        Self {
            applied: snapshot.as_ref().and_then(|s| s.meta.last_log_id),
            membership: snapshot
                .as_ref()
                .map(|s| s.meta.last_membership.clone())
                .unwrap_or_default(),
            group: snapshot.as_ref().map(|s| s.state).unwrap_or_default(),
            history: History::new(snapshot.clone()),
            snapshot,
        }
    }

    fn as_applied(&self) -> GroupSnapshot {
        GroupSnapshot {
            meta: SnapshotMeta {
                last_log_id: self.applied,
                last_membership: self.membership.clone(),
            },
            state: self.group,
        }
    }

    /// Apply one entry; returns the cut of a checkpoint it applied.
    fn apply(
        &mut self,
        log_id: LogId,
        payload: EntryPayload<GroupEntry, Membership>,
    ) -> Option<LogId> {
        self.applied = Some(log_id);
        self.history.entered(log_id);
        match payload {
            EntryPayload::Membership(m) => {
                self.membership = StoredMembership::new(Some(log_id), m);
                let change = Change::Membership(self.membership.clone());
                self.history.changed(log_id.index, change);
                None
            }
            EntryPayload::Normal(GroupEntry::Control(control)) => {
                self.apply_control(log_id, control)
            }
            EntryPayload::Blank | EntryPayload::Normal(GroupEntry::Batch(_)) => None,
        }
    }

    fn apply_control(&mut self, at: LogId, control: Control) -> Option<LogId> {
        let kind = control.kind();
        if kind > self.group.highest_kind {
            self.group.highest_kind = kind;
            self.history.changed(at.index, Change::HighestKind(kind));
        }
        match control {
            Control::FlusherAssignment(replica) => {
                let flusher = NodeId::from(Uuid::from_bytes(replica));
                self.group.flusher = Some(flusher);
                self.history.changed(at.index, Change::Flusher(flusher));
                None
            }
            Control::Checkpoint {
                proposer,
                cut,
                highest_kind,
            } => {
                let proposal = Proposal::new(proposer, cut, highest_kind);
                self.history.apply(at, self.group.flusher, proposal)
            }
            Control::IdBlock { after, through } => {
                self.apply_id_block(at, IdBlock::new(after, through));
                None
            }
        }
    }

    /// Apply the block unless it starts below the end of the latest one
    /// applied: allocating from it could issue an id twice. An ignored
    /// block never becomes the one the next is compared with.
    fn apply_id_block(&mut self, at: LogId, block: Option<IdBlock>) {
        let previous = self.group.id_block;
        let follows = |block: &IdBlock| previous.is_none_or(|p| block.after() >= p.through());
        if let Some(block) = block.filter(follows) {
            self.group.id_block = Some(block);
            self.history.changed(at.index, Change::IdBlock(block));
        }
    }
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
    /// The cut of the group's checkpoint.
    cuts: Arc<watch::Sender<Option<LogId>>>,
}

impl GroupSm {
    /// The state machine of the group whose WAL is in `dir`, applied
    /// through the snapshot recorded there, if any. Blocking.
    ///
    /// # Errors
    ///
    /// The record cannot be read, or fails its CRC or version check.
    pub(super) fn open(dir: &Path, gate: Arc<SyncGate>) -> io::Result<Self> {
        let state = State::opened(snapshot_record::read(dir)?);
        let cuts = watch::Sender::new(state.history.cut());
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
            record: Arc::new(Record {
                dir: dir.to_path_buf(),
                gate,
                publishing: tokio::sync::Mutex::new(()),
            }),
            cuts: Arc::new(cuts),
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

    /// The latest id block applied, or the snapshot's when none has been
    /// since; the group's miner allocates from it.
    #[must_use]
    pub fn id_block(&self) -> Option<IdBlock> {
        self.state().group.id_block
    }

    /// The group's current snapshot, opened, built or installed.
    pub(super) fn snapshot(&self) -> Option<GroupSnapshot> {
        self.state().snapshot.clone()
    }

    /// The cut of the group's checkpoint, the last applied or the one its
    /// snapshot was opened or installed at; it changes as each applies.
    pub(super) fn checkpoints(&self) -> watch::Receiver<Option<LogId>> {
        self.cuts.subscribe()
    }
}

impl RaftSnapshotBuilder<RaftTypes> for GroupSm {
    type SnapshotData = GroupState;

    /// The group's checkpoint snapshot, recorded unless it already is.
    /// Without a checkpoint only a forced build gets here, and it gets the
    /// group as applied: `openraft` forces one only for a log purged with
    /// no snapshot recorded, which recording before every purge rules out.
    async fn build_snapshot(&mut self) -> Result<SnapshotOf<RaftTypes, GroupState>, io::Error> {
        let record = Arc::clone(&self.record);
        let _publishing = record.publishing.lock().await;
        // Read once publishing is ours: an install that overlapped this
        // build is then in the state, so it never records an older one.
        let (checkpoint, recorded) = {
            let state = self.state();
            let recorded = state.snapshot.as_ref().map(|s| s.meta.last_log_id);
            let checkpoint = state.history.checkpoint().cloned();
            (checkpoint.unwrap_or_else(|| state.as_applied()), recorded)
        };
        if recorded.is_none_or(|last| last < checkpoint.meta.last_log_id) {
            self.record(checkpoint.clone()).await?;
        }
        Ok(checkpoint.into())
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
            let checkpoint = self.state().apply(entry.log_id, entry.payload);
            if let Some(cut) = checkpoint {
                self.cuts.send_replace(Some(cut));
            }
            if let Some(responder) = responder {
                responder.send(());
            }
        }
        Ok(())
    }

    async fn try_create_snapshot_builder(&mut self, force: bool) -> Option<Self> {
        let checkpointed = self.state().history.checkpoint().is_some();
        (force || checkpointed).then(|| self.clone())
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
        self.record(snapshot.clone()).await?;
        *self.state() = State::opened(Some(snapshot));
        self.cuts.send_replace(meta.last_log_id);
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
    use std::collections::BTreeMap;
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
    #[ignore = "superseded: RFC 0065 §6.5 builds a snapshot only at a checkpoint's cut; see \
                a_built_snapshot_is_the_group_at_the_checkpoints_cut"]
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

    fn checkpoint(
        index: u64,
        proposer: u64,
        (term, cut): (u64, u64),
        highest_kind: u8,
    ) -> EntryResponder<RaftTypes> {
        let control = Control::Checkpoint {
            proposer: *NodeId::from(proposer).uuid().as_bytes(),
            cut: ourios_wal::RaftLogId { term, index: cut },
            highest_kind,
        };
        entry(index, EntryPayload::Normal(GroupEntry::Control(control)))
    }

    fn voters(ids: &[u64]) -> Membership {
        let ids = ids.iter().map(|&n| NodeId::from(n));
        let nodes: BTreeMap<_, _> = ids.clone().map(|id| (id, id.to_string())).collect();
        Membership::new(vec![ids.collect()], nodes).expect("membership")
    }

    fn cut(index: u64) -> Option<LogId> {
        at(index).last_log_id
    }

    /// A checkpoint snapshots the group at its cut: the membership,
    /// flusher and highest kind there, not those changed after it, and a
    /// reopen starts applied through the cut.
    #[tokio::test]
    async fn a_built_snapshot_is_the_group_at_the_checkpoints_cut() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sm = GroupSm::open(dir.path(), Arc::default()).expect("open");
        let cuts = sm.checkpoints();
        let entries = vec![
            entry(1, EntryPayload::Membership(voters(&[1, 2]))),
            assign(2, 7),
            batch(3),
            entry(4, EntryPayload::Membership(voters(&[1, 2, 3]))),
            assign(5, 9),
            checkpoint(6, 9, (2, 3), 1),
        ];
        apply(&mut sm, entries).await;
        assert_eq!(*cuts.borrow(), cut(3));
        assert_eq!(sm.flusher(), Some(NodeId::from(9)), "applied, not built");

        let built = sm.build_snapshot().await.expect("build");
        let at_cut = StoredMembership::new(cut(1), voters(&[1, 2]));
        assert_eq!(built.meta.last_log_id, cut(3));
        assert_eq!(built.meta.last_membership, at_cut);
        let expected = GroupState {
            flusher: Some(NodeId::from(7)),
            highest_kind: 1,
            id_block: None,
        };
        assert_eq!(built.snapshot, expected);
        let reopened = GroupSm::open(dir.path(), Arc::default()).expect("reopen");
        assert_eq!(reopened.applied(), cut(3));
        assert_eq!(reopened.flusher(), Some(NodeId::from(7)));
        assert_eq!(reopened.membership(), at_cut);
        assert_eq!(*reopened.checkpoints().borrow(), cut(3));
    }

    /// Checkpoints every replica refuses change nothing, though the log's
    /// highest kind counts them; with no checkpoint applied only a forced
    /// build is offered.
    #[tokio::test]
    async fn a_checkpoint_every_replica_would_refuse_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sm = GroupSm::open(dir.path(), Arc::default()).expect("open");
        let cuts = sm.checkpoints();
        apply(&mut sm, vec![batch(1), assign(2, 7), batch(3)]).await;
        let refused = [
            (checkpoint(4, 9, (2, 3), 1), "not the flusher"),
            (checkpoint(5, 7, (2, 6), 1), "a cut not below the entry"),
            (checkpoint(6, 7, (3, 3), 1), "a cut not in the log"),
            (
                checkpoint(7, 7, (2, 3), 2),
                "a highest kind not the log's at the cut",
            ),
        ];
        for (entry, why) in refused {
            apply(&mut sm, vec![entry]).await;
            assert_eq!(*cuts.borrow(), None, "{why}");
            let builder = sm.try_create_snapshot_builder(false).await;
            assert!(builder.is_none(), "{why}");
        }
        let forced = sm.try_create_snapshot_builder(true).await;
        assert!(forced.is_some(), "openraft must get a forced builder");

        apply(&mut sm, vec![checkpoint(8, 7, (2, 6), 2)]).await;
        assert_eq!(*cuts.borrow(), cut(6), "kind 2 since the first refused");
        apply(&mut sm, vec![checkpoint(9, 7, (2, 3), 2)]).await;
        assert_eq!(*cuts.borrow(), cut(6), "a cut not above the last");
        let built = sm.build_snapshot().await.expect("build");
        assert_eq!(built.meta.last_log_id, cut(6));
        assert_eq!(built.snapshot.highest_kind, 2);
    }

    fn id_block(block: Option<IdBlock>) -> EntryPayload<GroupEntry, Membership> {
        let block = block.expect("a valid block");
        let control = Control::IdBlock {
            after: block.after(),
            through: block.through(),
        };
        EntryPayload::Normal(GroupEntry::Control(control))
    }

    /// A block below the latest applied one is ignored, and an ignored
    /// block is never the one the next is compared with; a block starting
    /// where the latest ends applies.
    #[tokio::test]
    async fn a_block_below_the_latest_applied_one_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sm = GroupSm::open(dir.path(), Arc::default()).expect("open");
        let block = IdBlock::new;
        let blocks = [
            (block(0, 100), block(0, 100), "the first"),
            (block(0, 50), block(0, 100), "below (0, 100]"),
            (
                block(50, 150),
                block(0, 100),
                "below (0, 100], not the ignored (0, 50]",
            ),
            (block(100, 200), block(100, 200), "from where (0, 100] ends"),
            (block(300, 400), block(300, 400), "above, past a gap"),
        ];
        for (index, (body, latest, why)) in (1..).zip(blocks) {
            apply(&mut sm, vec![entry(index, id_block(body))]).await;
            assert_eq!(sm.id_block(), latest, "{why}");
        }
        assert_eq!(sm.state().group.highest_kind, 3);
    }

    /// The checkpoint's snapshot holds the block in force at its cut, a
    /// reopen and an install restore it, and the restored block is the one
    /// the next is compared with.
    #[tokio::test]
    async fn a_snapshot_holds_the_id_block_in_force_at_its_cut() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sm = GroupSm::open(dir.path(), Arc::default()).expect("open");
        let block = IdBlock::new;
        let entries = vec![
            assign(1, 7),
            entry(2, id_block(block(0, 100))),
            batch(3),
            entry(4, id_block(block(100, 200))),
            checkpoint(5, 7, (2, 3), 3),
        ];
        apply(&mut sm, entries).await;
        assert_eq!(sm.id_block(), block(100, 200), "applied, not built");
        let built = sm.build_snapshot().await.expect("build");
        assert_eq!(built.snapshot.id_block, block(0, 100), "as of the cut");

        let mut reopened = GroupSm::open(dir.path(), Arc::default()).expect("reopen");
        assert_eq!(reopened.id_block(), block(0, 100));
        apply(&mut reopened, vec![entry(4, id_block(block(50, 150)))]).await;
        assert_eq!(reopened.id_block(), block(0, 100), "below the snapshot's");

        let other = tempfile::tempdir().expect("tempdir");
        let mut installed = GroupSm::open(other.path(), Arc::default()).expect("open");
        installed
            .install_snapshot(&built.meta, built.snapshot)
            .await
            .expect("install");
        assert_eq!(installed.id_block(), block(0, 100));
        apply(&mut installed, vec![entry(4, id_block(block(100, 200)))]).await;
        assert_eq!(installed.id_block(), sm.id_block(), "as the whole log");
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
