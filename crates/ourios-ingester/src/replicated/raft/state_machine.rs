//! A group's state machine (RFC 0065 §6.1): what it has applied, the
//! membership it applied last, and the flusher the last flusher
//! assignment named (§6.5).
//!
//! It lives in memory, except for its snapshot: `openraft` purges the log
//! up to a snapshot it installs or builds, so the snapshot is recorded
//! in `wal_root/groups/<group>/snapshot` before the call returns,
//! and a restart opens applied through it and replays the log after it.
//! The record holds the flusher, the highest control kind, the latest
//! id block and the miner configuration in force with the membership, so
//! a replica that opens from or installs it knows them before it applies
//! any entry after it.
//!
//! A miner configuration that changes the tree's shape from the one in
//! force fails the apply, which stops the replica: the group's first
//! configuration fixes `prefix_depth` and `max_node_children` for the life
//! of its log.
//!
//! A snapshot is only ever the group at a checkpoint's cut: applying a
//! checkpoint entry publishes its cut on `GroupSm::checkpoints`, the
//! host triggers the build, and the builder records the checkpoint's
//! snapshot rather than the state at the last entry applied.
//!
//! A tail replica opens it with `GroupSm::open_mining` and receives,
//! in log order, an [`Applied`] event for every entry its miner follows
//! (§6.5): the miner takes its id blocks and configuration only from
//! these, never from its own reading of the log. The channel is bounded
//! and `apply` waits for room, so a lagging miner holds back only this
//! replica's apply; a tail replica is a learner, whose apply no
//! acknowledgement waits on, and a voter opens with no channel. Each
//! event owns its data, so purging the log never races a lagging miner.
//! A snapshot the replica opens from or installs reaches the miner as
//! [`AppliedEvent::Installed`], ahead of every entry after it: the
//! entries below it never will.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures_util::TryStreamExt as _;
use openraft::storage::{EntryResponder, RaftSnapshotBuilder, RaftStateMachine};
use openraft::type_config::alias::{SnapshotOf, StoredMembershipOf};
use openraft::{EntryPayload, OptionalSend};
use ourios_wal::{Control, MinerConfigV1};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use super::checkpoint::{Change, History, Proposal};
use super::gate::SyncGate;
use super::snapshot_record::{
    self, GroupMinerConfig, GroupSnapshot, GroupState, IdBlock, SnapshotMeta,
};
use super::types::{GroupEntry, LogId, Membership, NodeId, RaftTypes, TimedBatch};

type StoredMembership = StoredMembershipOf<RaftTypes>;

/// What applying one entry changed for the group's miner.
#[derive(Debug, Clone, PartialEq)]
pub enum AppliedEvent {
    /// A tenant's batch and its time, owned.
    Batch(TimedBatch),
    /// An id block the group applied; an ignored block emits nothing.
    IdBlock(IdBlock),
    /// The miner configuration in force from this entry.
    MinerConfig(GroupMinerConfig),
    /// The replica a flusher assignment named.
    Flusher(NodeId),
    /// A checkpoint the group applied, with its cut.
    Checkpoint(LogId),
    /// A snapshot installed in place of the log through this position:
    /// the entries below it never reach the miner.
    Installed(GroupState),
}

/// One [`AppliedEvent`] and the log position it applied at.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub at: LogId,
    pub event: AppliedEvent,
}

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

    /// Apply one entry; returns what it changed for the group's miner.
    ///
    /// # Errors
    ///
    /// A miner configuration the group cannot apply (see
    /// [`Self::apply_miner_config`]).
    fn apply(
        &mut self,
        log_id: LogId,
        payload: EntryPayload<GroupEntry, Membership>,
    ) -> io::Result<Option<AppliedEvent>> {
        self.applied = Some(log_id);
        self.history.entered(log_id);
        match payload {
            EntryPayload::Membership(m) => {
                self.membership = StoredMembership::new(Some(log_id), m);
                let change = Change::Membership(self.membership.clone());
                self.history.changed(log_id.index, change);
                Ok(None)
            }
            EntryPayload::Normal(GroupEntry::Control(control)) => {
                self.apply_control(log_id, control)
            }
            EntryPayload::Normal(GroupEntry::Batch(batch)) => Ok(Some(AppliedEvent::Batch(batch))),
            EntryPayload::Blank => Ok(None),
        }
    }

    fn apply_control(&mut self, at: LogId, control: Control) -> io::Result<Option<AppliedEvent>> {
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
                Ok(Some(AppliedEvent::Flusher(flusher)))
            }
            Control::Checkpoint {
                proposer,
                cut,
                highest_kind,
            } => {
                let proposal = Proposal::new(proposer, cut, highest_kind);
                let applied = self.history.apply(at, self.group.flusher, proposal);
                Ok(applied.map(AppliedEvent::Checkpoint))
            }
            Control::IdBlock { after, through } => Ok(self
                .apply_id_block(at, IdBlock::new(after, through))
                .map(AppliedEvent::IdBlock)),
            Control::MinerConfig(body) => {
                let config = self.apply_miner_config(at, body)?;
                Ok(Some(AppliedEvent::MinerConfig(config)))
            }
        }
    }

    /// Put the configuration in force from `at`.
    ///
    /// # Errors
    ///
    /// `InvalidData` when `MinerConfig` refuses the body, or it changes
    /// `prefix_depth` or `max_node_children` from the configuration in
    /// force: a snapshot's routes are valid only at the shape it was mined
    /// at, so every replica stops rather than apply it.
    fn apply_miner_config(
        &mut self,
        at: LogId,
        body: MinerConfigV1,
    ) -> io::Result<GroupMinerConfig> {
        let refused = |why: String| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("miner configuration at {at}: {why}"),
            )
        };
        let config = GroupMinerConfig::new(body).map_err(|e| refused(e.to_string()))?;
        match self.group.miner_config {
            Some(first) if !first.same_shape(config) => Err(refused(format!(
                "prefix_depth {} and max_node_children {} are fixed for the log, not {} and {}",
                first.body().prefix_depth,
                first.body().max_node_children,
                body.prefix_depth,
                body.max_node_children
            ))),
            _ => {
                self.group.miner_config = Some(config);
                self.history.changed(at.index, Change::MinerConfig(config));
                Ok(config)
            }
        }
    }

    /// Apply the block unless it starts below the end of the latest one
    /// applied: allocating from it could issue an id twice. An ignored
    /// block never becomes the one the next is compared with. Returns the
    /// block if it applied.
    fn apply_id_block(&mut self, at: LogId, block: Option<IdBlock>) -> Option<IdBlock> {
        let previous = self.group.id_block;
        let follows = |block: &IdBlock| previous.is_none_or(|p| block.after() >= p.through());
        let block = block.filter(follows)?;
        self.group.id_block = Some(block);
        self.history.changed(at.index, Change::IdBlock(block));
        Some(block)
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
    /// Where a tail replica's miner receives what each entry applied.
    events: Option<mpsc::Sender<Applied>>,
}

impl GroupSm {
    /// The state machine of the group whose WAL is in `dir`, applied
    /// through the snapshot recorded there, if any. Blocking.
    ///
    /// # Errors
    ///
    /// The record cannot be read, or fails its CRC or version check.
    pub(super) fn open(dir: &Path, gate: Arc<SyncGate>) -> io::Result<Self> {
        Self::opened(dir, gate, None)
    }

    /// [`Self::open`], sending an [`Applied`] event on `events` for every
    /// entry applied after the snapshot it opens through, if any, and
    /// first [`AppliedEvent::Installed`] with that snapshot. Once the
    /// receiver is dropped, the events are discarded.
    ///
    /// # Errors
    ///
    /// As [`Self::open`], or `events` has no room for the snapshot's
    /// event.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no host opens a tail replica's group yet")
    )]
    pub(super) fn open_mining(
        dir: &Path,
        gate: Arc<SyncGate>,
        events: mpsc::Sender<Applied>,
    ) -> io::Result<Self> {
        let sm = Self::opened(dir, gate, Some(events))?;
        sm.send_opened_snapshot()?;
        Ok(sm)
    }

    /// Send the snapshot the group opened from, if any, as the first
    /// event: the miner never sees the entries below it.
    fn send_opened_snapshot(&self) -> io::Result<()> {
        let installed = self.state().snapshot.as_ref().and_then(|snapshot| {
            Some(Applied {
                at: snapshot.meta.last_log_id?,
                event: AppliedEvent::Installed(snapshot.state),
            })
        });
        let (Some(events), Some(applied)) = (&self.events, installed) else {
            return Ok(());
        };
        match events.try_send(applied) {
            Err(TrySendError::Full(_)) => Err(io::Error::other(
                "the miner's channel is full before the group opened",
            )),
            Ok(()) | Err(TrySendError::Closed(_)) => Ok(()),
        }
    }

    fn opened(
        dir: &Path,
        gate: Arc<SyncGate>,
        events: Option<mpsc::Sender<Applied>>,
    ) -> io::Result<Self> {
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
            events,
        })
    }

    /// Publish a checkpoint's cut, and hand the event to the miner,
    /// waiting while its channel is full.
    async fn publish(&self, applied: Applied) {
        if let AppliedEvent::Checkpoint(cut) = applied.event {
            self.cuts.send_replace(Some(cut));
        }
        if let Some(events) = &self.events {
            // A miner that stopped has dropped its receiver; the replica
            // applies on without it.
            events.send(applied).await.ok();
        }
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

    /// The miner configuration in force: the latest applied, or the
    /// snapshot's when none has been since.
    #[must_use]
    pub fn miner_config(&self) -> Option<GroupMinerConfig> {
        self.state().group.miner_config
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
            let at = entry.log_id;
            let event = self.state().apply(at, entry.payload)?;
            if let Some(event) = event {
                self.publish(Applied { at, event }).await;
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
        if let Some(at) = meta.last_log_id {
            let event = AppliedEvent::Installed(group);
            self.publish(Applied { at, event }).await;
        }
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
        entry(index, EntryPayload::Normal(GroupEntry::from(batch)))
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
            miner_config: None,
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
            miner_config: None,
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
            miner_config: None,
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

    const FIRST: MinerConfigV1 = MinerConfigV1 {
        similarity_threshold: 0.7,
        similarity_floor: 0.4,
        param_byte_limit: 256,
        prefix_depth: 2,
        max_node_children: 100,
        max_templates: 20_000,
        max_line_tokens: 512,
        upstream_templates: ourios_wal::UpstreamTemplates::Ignore,
        upstream_template_byte_limit: 8192,
        upstream_association_limit: 4,
    };
    const RAISED: MinerConfigV1 = MinerConfigV1 {
        similarity_threshold: 0.8,
        ..FIRST
    };
    const DEEPER: MinerConfigV1 = MinerConfigV1 {
        prefix_depth: 3,
        ..RAISED
    };
    const ABOVE_ONE: MinerConfigV1 = MinerConfigV1 {
        similarity_threshold: 1.5,
        ..FIRST
    };

    fn configured(body: MinerConfigV1) -> EntryPayload<GroupEntry, Membership> {
        EntryPayload::Normal(GroupEntry::Control(Control::MinerConfig(body)))
    }

    fn in_force(body: MinerConfigV1) -> GroupMinerConfig {
        GroupMinerConfig::new(body).expect("valid")
    }

    async fn applied(sm: &mut GroupSm, entry: EntryResponder<RaftTypes>) -> io::Result<()> {
        sm.apply(futures_util::stream::iter([Ok(entry)])).await
    }

    /// The first configuration fixes the tree's shape: a later one that
    /// keeps it takes effect from its position, whatever else it changes,
    /// and one that changes `prefix_depth` or `max_node_children` fails the
    /// apply, leaving the one in force.
    #[tokio::test]
    async fn a_configuration_changing_the_trees_shape_stops_the_apply() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sm = GroupSm::open(dir.path(), Arc::default()).expect("open");
        assert_eq!(sm.miner_config(), None);
        apply(&mut sm, vec![batch(1), entry(2, configured(FIRST))]).await;
        assert_eq!(sm.miner_config(), Some(in_force(FIRST)));
        assert_eq!(sm.state().group.highest_kind, 4);
        apply(&mut sm, vec![entry(3, configured(RAISED))]).await;
        assert_eq!(sm.miner_config(), Some(in_force(RAISED)), "the threshold");

        let deeper = entry(4, configured(DEEPER));
        let e = applied(&mut sm, deeper).await.expect_err("prefix_depth");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(e.to_string().contains("prefix_depth 2"), "{e}");
        let wider = MinerConfigV1 {
            max_node_children: 101,
            ..RAISED
        };
        let e = applied(&mut sm, entry(5, configured(wider)))
            .await
            .expect_err("width");
        assert!(e.to_string().contains("max_node_children 100"), "{e}");
        assert_eq!(sm.miner_config(), Some(in_force(RAISED)));
    }

    /// A body `MinerConfig` refuses fails the apply; the codec refuses it
    /// first, so only a body that skipped it gets here.
    #[tokio::test]
    async fn a_configuration_minerconfig_refuses_stops_the_apply() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sm = GroupSm::open(dir.path(), Arc::default()).expect("open");
        let e = applied(&mut sm, entry(1, configured(ABOVE_ONE)))
            .await
            .expect_err("a threshold above 1");
        assert!(e.to_string().contains("similarity_threshold"), "{e}");
        assert_eq!(sm.miner_config(), None);
    }

    /// The checkpoint's snapshot holds the configuration in force at its
    /// cut; a reopen and an install take it from the record, so it is in
    /// force until an entry after the cut replaces it and it fixes the
    /// shape for the entries after the cut.
    #[tokio::test]
    async fn a_snapshot_holds_the_miner_configuration_in_force_at_its_cut() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sm = GroupSm::open(dir.path(), Arc::default()).expect("open");
        let entries = vec![
            assign(1, 7),
            entry(2, configured(FIRST)),
            batch(3),
            entry(4, configured(RAISED)),
            checkpoint(5, 7, (2, 3), 4),
        ];
        apply(&mut sm, entries).await;
        assert_eq!(sm.miner_config(), Some(in_force(RAISED)), "applied");
        let built = sm.build_snapshot().await.expect("build");
        assert_eq!(built.meta.last_log_id, cut(3));
        assert_eq!(
            built.snapshot.miner_config,
            Some(in_force(FIRST)),
            "at the cut"
        );

        let mut reopened = GroupSm::open(dir.path(), Arc::default()).expect("reopen");
        assert_eq!(reopened.miner_config(), Some(in_force(FIRST)));
        let deeper = applied(&mut reopened, entry(4, configured(DEEPER))).await;
        deeper.expect_err("the snapshot's shape is fixed");

        let other = tempfile::tempdir().expect("tempdir");
        let mut installed = GroupSm::open(other.path(), Arc::default()).expect("open");
        installed
            .install_snapshot(&built.meta, built.snapshot)
            .await
            .expect("install");
        assert_eq!(installed.miner_config(), Some(in_force(FIRST)));
        apply(&mut installed, vec![batch(4)]).await;
        assert_eq!(
            installed.miner_config(),
            Some(in_force(FIRST)),
            "none since"
        );
        apply(&mut installed, vec![entry(5, configured(RAISED))]).await;
        assert_eq!(
            installed.miner_config(),
            sm.miner_config(),
            "as the whole log"
        );
    }

    /// Batches, the first miner configuration at index 4, and a batch.
    fn configured_at_4() -> Vec<EntryResponder<RaftTypes>> {
        vec![
            assign(1, 7),
            batch(2),
            batch(3),
            entry(4, configured(FIRST)),
            batch(5),
        ]
    }

    /// A flusher's checkpoint whose cut lies before the group's first
    /// miner configuration is ignored and offers no build, so nothing is
    /// purged.
    #[tokio::test]
    async fn a_checkpoint_before_the_first_miner_configuration_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sm = GroupSm::open(dir.path(), Arc::default()).expect("open");
        let cuts = sm.checkpoints();
        apply(&mut sm, configured_at_4()).await;
        apply(&mut sm, vec![checkpoint(6, 7, (2, 3), 1)]).await;
        assert_eq!(*cuts.borrow(), None, "no configuration at the cut");
        let builder = sm.try_create_snapshot_builder(false).await;
        assert!(builder.is_none(), "nothing to build, nothing purged");
    }

    /// A flusher's checkpoint at or above the first miner configuration
    /// applies and snapshots the configuration.
    #[tokio::test]
    async fn a_checkpoint_at_or_above_the_first_miner_configuration_applies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sm = GroupSm::open(dir.path(), Arc::default()).expect("open");
        let cuts = sm.checkpoints();
        apply(&mut sm, configured_at_4()).await;
        apply(&mut sm, vec![checkpoint(7, 7, (2, 4), 4)]).await;
        assert_eq!(*cuts.borrow(), cut(4), "at the configuration");
        apply(&mut sm, vec![checkpoint(8, 7, (2, 5), 4)]).await;
        assert_eq!(*cuts.borrow(), cut(5), "above it");
        let built = sm.build_snapshot().await.expect("build");
        assert_eq!(built.snapshot.miner_config, Some(in_force(FIRST)));
    }

    fn mining(dir: &Path, capacity: usize) -> (GroupSm, mpsc::Receiver<Applied>) {
        let (events, received) = mpsc::channel(capacity);
        let sm = GroupSm::open_mining(dir, Arc::default(), events).expect("open");
        (sm, received)
    }

    fn drained(received: &mut mpsc::Receiver<Applied>) -> Vec<(u64, AppliedEvent)> {
        std::iter::from_fn(|| received.try_recv().ok())
            .map(|applied| (applied.at.index, applied.event))
            .collect()
    }

    fn batch_bytes() -> TimedBatch {
        let payload = ourios_wal::TenantBatch::encode("acme", b"p").expect("batch");
        TimedBatch {
            time: ourios_wal::BatchTime::default(),
            batch: super::super::types::TenantBatchBytes::new(payload).expect("valid"),
        }
    }

    /// A mining replica receives, in log order, each batch, flusher
    /// assignment, applied id block, configuration in force and applied
    /// checkpoint; membership, blank and ignored entries send nothing.
    #[tokio::test]
    async fn a_mining_replica_receives_what_each_entry_applied_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut sm, mut received) = mining(dir.path(), 64);
        let block = IdBlock::new;
        let entries = vec![
            entry(1, EntryPayload::Membership(voters(&[1, 2]))),
            batch(2),
            assign(3, 7),
            entry(4, id_block(block(0, 100))),
            entry(5, id_block(block(0, 50))),
            entry(6, EntryPayload::Blank),
            entry(7, configured(FIRST)),
            checkpoint(8, 9, (2, 2), 0),
            checkpoint(9, 7, (2, 2), 0),
            entry(10, configured(RAISED)),
            batch(11),
        ];
        apply(&mut sm, entries).await;
        let expected = vec![
            (2, AppliedEvent::Batch(batch_bytes())),
            (3, AppliedEvent::Flusher(NodeId::from(7))),
            (4, AppliedEvent::IdBlock(block(0, 100).expect("valid"))),
            (7, AppliedEvent::MinerConfig(in_force(FIRST))),
            (9, AppliedEvent::Checkpoint(cut(2).expect("cut"))),
            (10, AppliedEvent::MinerConfig(in_force(RAISED))),
            (11, AppliedEvent::Batch(batch_bytes())),
        ];
        assert_eq!(drained(&mut received), expected);
    }

    /// A voter opens with no channel and applies the same log.
    #[tokio::test]
    async fn a_voter_opens_with_no_channel() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut sm = GroupSm::open(dir.path(), Arc::default()).expect("open");
        assert!(sm.events.is_none());
        apply(&mut sm, vec![batch(1), assign(2, 7), batch(3)]).await;
        assert_eq!(sm.applied().map(|id| id.index), Some(3));
    }

    /// A full channel holds the apply until the miner takes an event, and
    /// a miner that dropped its receiver holds it back no more.
    #[tokio::test]
    async fn a_full_channel_holds_back_only_the_apply() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (sm, mut received) = mining(dir.path(), 1);
        let mut applying = sm.clone();
        let mut apply_task =
            tokio::spawn(async move { apply(&mut applying, vec![batch(1), batch(2)]).await });
        let held = tokio::time::timeout(Duration::from_millis(50), &mut apply_task).await;
        assert!(held.is_err(), "the second batch waits for room");
        assert_eq!(
            sm.applied().map(|id| id.index),
            Some(2),
            "applied, not sent"
        );
        assert_eq!(received.recv().await.map(|a| a.at.index), Some(1));
        apply_task.await.expect("join");
        assert_eq!(received.recv().await.map(|a| a.at.index), Some(2));

        drop(received);
        let mut sm = sm;
        apply(&mut sm, vec![batch(3), batch(4)]).await;
        assert_eq!(sm.applied().map(|id| id.index), Some(4));
    }

    /// An install sends the state it installed at the snapshot's cut.
    #[tokio::test]
    async fn an_install_sends_the_state_it_installed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut sm, mut received) = mining(dir.path(), 4);
        let group = GroupState {
            flusher: Some(NodeId::from(9)),
            highest_kind: 4,
            id_block: IdBlock::new(0, 100),
            miner_config: Some(in_force(FIRST)),
        };
        sm.install_snapshot(&at(5), group).await.expect("install");
        assert_eq!(
            drained(&mut received),
            vec![(5, AppliedEvent::Installed(group))]
        );
    }

    /// A mining open of a group with a recorded snapshot sends the
    /// snapshot first, then the entries after it; one with no recorded
    /// snapshot sends nothing at open.
    #[tokio::test]
    async fn a_mining_reopen_sends_its_snapshot_before_any_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut sm, mut received) = mining(dir.path(), 8);
        assert!(drained(&mut received).is_empty(), "no snapshot recorded");
        let entries = vec![
            assign(1, 7),
            entry(2, configured(FIRST)),
            batch(3),
            checkpoint(4, 7, (2, 3), 4),
        ];
        apply(&mut sm, entries).await;
        let built = sm.build_snapshot().await.expect("build");
        drop(sm);

        let (mut reopened, mut received) = mining(dir.path(), 8);
        apply(&mut reopened, vec![batch(4)]).await;
        assert_eq!(
            drained(&mut received),
            vec![
                (3, AppliedEvent::Installed(built.snapshot)),
                (4, AppliedEvent::Batch(batch_bytes())),
            ]
        );

        let (events, _received) = mpsc::channel(1);
        events
            .try_send(Applied {
                at: cut(1).expect("id"),
                event: AppliedEvent::Flusher(NodeId::from(7)),
            })
            .expect("room");
        let full = GroupSm::open_mining(dir.path(), Arc::default(), events);
        assert!(full.is_err(), "no room for the snapshot");
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
