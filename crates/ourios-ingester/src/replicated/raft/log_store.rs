//! One group's Raft log over its own RFC 0008 WAL (RFC 0065 §4.3, §4.4,
//! §6.1), at `wal_root/groups/<group>/`.
//!
//! Every record is a `0x04` frame. Replay rebuilds the log in WAL order:
//! an entry at index `i` replaces everything from `i` up, a truncate or
//! purge marker drops what it names, and the latest committed marker is
//! kept. The whole log above the purge point is held in memory, so reads
//! never touch the disk.
//!
//! A writer thread owns the WAL. `append` makes its entries readable at
//! once and hands their frames to the writer, which appends every command
//! queued while the previous fsync ran, fsyncs once, and only then
//! completes each `append`'s callback; truncate and purge markers resolve
//! after the fsync that covers them. A committed marker is written
//! without one of its own and rides the next.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::io;
use std::ops::RangeBounds;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};

use openraft::storage::{IOFlushed, LogState, RaftLogReader, RaftLogStorage};
use openraft::{EntryPayload, OptionalSend};
use ourios_wal::{
    Control, FrameKind, FrameSink, RecoveryError, ReplicatedRecord, Wal, WalConfig, WalOffset,
};
use tokio::sync::oneshot;
use uuid::Uuid;

use super::codec::{self, Replayed};
use super::gate::SyncGate;
use super::telemetry::Takeovers;
use super::types::{Entry, GroupEntry, LogId, NodeId, RaftTypes, Vote};
use super::vote_log::VoteLog;
use super::{Flush, OWNER_WAIT, Owner, sync_through, take_ownership, writer_gone};

/// The group's ownership lock, inside its WAL root (the WAL reads only
/// `*.wal` names there).
pub const GROUP_LOCK_FILE: &str = "group.lock";

/// A group's WAL root under the node's `wal_root`.
#[must_use]
pub fn group_dir(wal_root: &Path, group: u32) -> PathBuf {
    wal_root.join("groups").join(group.to_string())
}

/// The log as the WAL holds it.
#[derive(Debug, Default)]
struct Index {
    entries: BTreeMap<u64, Entry>,
    last_purged: Option<LogId>,
    committed: Option<LogId>,
    /// The control entries `entries` holds, kept beside it so a leader's
    /// pass reads them without scanning the log under its lock.
    controls: BTreeMap<u64, Control>,
    /// The highest control kind purged: a purged entry was committed, so
    /// the log still holds its kind (§6.5).
    purged_kind: u8,
}

impl Index {
    /// An entry may replace a kept one or follow the last, and never sits
    /// at or below the purge point: an empty, purged log takes only the
    /// index after it. An empty log with nothing purged takes any first
    /// index, since `openraft`'s storage suite starts logs above 0.
    fn admits(&self, index: u64) -> io::Result<()> {
        let floor = self.last_purged.map(|p| p.index + 1);
        let next = self.entries.keys().next_back().map(|last| last + 1);
        match next.or(floor) {
            Some(next) if !(floor.unwrap_or(0)..=next).contains(&index) => Err(invalid(format!(
                "entry {index} is outside {}..={next}, which would leave a hole",
                floor.unwrap_or(0)
            ))),
            _ => Ok(()),
        }
    }

    /// An entry replaces every entry at or above its index.
    fn place(&mut self, entry: Entry) {
        let index = entry.log_id.index;
        self.entries.split_off(&index);
        self.controls.split_off(&index);
        if self.last_purged.is_none_or(|p| index > p.index) {
            if let EntryPayload::Normal(GroupEntry::Control(control)) = &entry.payload {
                self.controls.insert(index, *control);
            }
            self.entries.insert(index, entry);
        }
    }

    /// Admit a contiguous run of entries, all or none.
    fn extend(&mut self, entries: Vec<Entry>) -> io::Result<()> {
        let mut next = None;
        for entry in &entries {
            let index = entry.log_id.index;
            match next {
                None => self.admits(index)?,
                Some(n) if n != index => {
                    return Err(invalid(format!("entry {index} does not follow {}", n - 1)));
                }
                Some(_) => {}
            }
            next = Some(index + 1);
        }
        for entry in entries {
            self.place(entry);
        }
        Ok(())
    }

    fn truncate_after(&mut self, last: Option<LogId>) {
        let from = last.map_or(0, |l| l.index + 1);
        self.entries.split_off(&from);
        self.controls.split_off(&from);
    }

    fn purge(&mut self, upto: LogId) {
        self.entries = self.entries.split_off(&(upto.index + 1));
        let kept = self.controls.split_off(&(upto.index + 1));
        let purged = std::mem::replace(&mut self.controls, kept);
        self.purged_kind = purged
            .values()
            .map(|c| c.kind())
            .fold(self.purged_kind, u8::max);
        if self.last_purged.is_none_or(|p| upto > p) {
            self.last_purged = Some(upto);
        }
    }

    fn commit(&mut self, committed: LogId) {
        if self.committed.is_none_or(|c| committed > c) {
            self.committed = Some(committed);
        }
    }

    fn apply(&mut self, record: Replayed) -> io::Result<()> {
        match record {
            Replayed::Entry(entry) => {
                self.admits(entry.log_id.index)?;
                self.place(entry);
            }
            Replayed::TruncateAfter(last) => self.truncate_after(last),
            Replayed::Purge(upto) => self.purge(upto),
            Replayed::Committed(c) => self.commit(c),
        }
        Ok(())
    }

    fn log_controls(&self) -> LogControls {
        let highest_kind = self
            .controls
            .values()
            .map(|c| c.kind())
            .fold(self.purged_kind, u8::max);
        let introduced = self
            .controls
            .iter()
            .find(|(_, c)| c.kind() == highest_kind && highest_kind > self.purged_kind)
            .map(|(&index, _)| index);
        // Every kind is an assignment so far; the next one filters here.
        let flusher = self.controls.values().next_back().map(|c| match *c {
            Control::FlusherAssignment(replica) => NodeId::from(Uuid::from_bytes(replica)),
        });
        LogControls {
            highest_kind,
            introduced,
            flusher,
        }
    }

    fn last_log_id(&self) -> Option<LogId> {
        self.entries
            .values()
            .next_back()
            .map(|e| e.log_id)
            .or(self.last_purged)
    }
}

/// What the control entries of a log, committed or not, say (§6.5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LogControls {
    /// The highest control kind the log holds, `0` with none.
    pub highest_kind: u8,
    /// The index of the first entry of `highest_kind`, the newest entry of
    /// a new kind, unless it was purged (and so committed).
    pub introduced: Option<u64>,
    /// The replica the newest flusher assignment names.
    pub flusher: Option<NodeId>,
}

/// Fsync `groups/` and `wal_root`, the directories holding the group
/// root's entry and `groups/`'s, on every open: `prepare_root` fsyncs them
/// only when it creates them, so a first open interrupted after creating
/// the tree would otherwise leave a retry with an entry a power loss can
/// still drop.
fn sync_ancestors(root: &Path, gate: &SyncGate) -> io::Result<()> {
    for dir in root.ancestors().skip(1).take(2) {
        let sync = || std::fs::File::open(dir)?.sync_all();
        sync_through(gate, sync).unwrap_or_else(|| Err(writer_gone()))?;
    }
    Ok(())
}

/// Whether `root` holds a WAL segment; a missing root holds none.
fn holds_segments(root: &Path) -> io::Result<bool> {
    let dir = match std::fs::read_dir(root) {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    for entry in dir {
        if entry?.path().extension().is_some_and(|x| x == "wal") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A group whose vote survived but whose log did not: coming up with an
/// empty log under the same identity could vote away entries this replica
/// acknowledged (RFC 0065 §4.5).
fn lost(root: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "{} holds no WAL segment but this replica has voted in its group; rejoin it as \
             a new replica (RFC 0065 §6.1, B4) rather than restart with an empty log",
            root.display()
        ),
    )
}

fn invalid(detail: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail)
}

fn wal_error(op: &str, e: impl Debug) -> io::Error {
    io::Error::other(format!("{op}: {e:?}"))
}

/// Open and replay the group's WAL, then fsync it before its index is
/// exposed: after a process crash, replay may read frames that are still
/// only in the page cache, and `openraft` skips the I/O for a retried
/// entry it already holds, so this replica could count toward a quorum
/// for an entry a power loss would then take. Older segments were synced
/// when they rotated, so the current one is all that can be unsynced.
fn recover(config: WalConfig, gate: &SyncGate) -> io::Result<(Wal, Index, WalOffset)> {
    let mut wal = Wal::open(config).map_err(|e| wal_error("open the group WAL", e))?;
    let mut replay = Replay(Index::default());
    wal.replay(&mut replay)
        .map_err(|e| wal_error("replay the group WAL", e))?;
    wal.rebuild_ledger()
        .map_err(|e| wal_error("rebuild the group WAL ledger", e))?;
    let sync = || {
        wal.sync()
            .map_err(|e| wal_error("sync the recovered group WAL", e))
    };
    let synced = sync_through(gate, sync).unwrap_or_else(|| Err(writer_gone()))?;
    Ok((wal, replay.0, synced))
}

struct Replay(Index);

impl FrameSink for Replay {
    fn consume(
        &mut self,
        offset: WalOffset,
        kind: FrameKind,
        payload: &[u8],
    ) -> Result<(), RecoveryError> {
        let applied = match kind {
            FrameKind::ReplicatedEntry => codec::decode(payload).and_then(|r| self.0.apply(r)),
            other => Err(invalid(format!(
                "a group WAL holds only 0x04 frames, not {other:?}"
            ))),
        };
        applied.map_err(|e| RecoveryError::SinkRejected {
            detail: format!("group log frame at {}+{}: {e}", offset.segment, offset.byte),
        })
    }
}

/// An append's callback, completed with an error if the writer drops it
/// unanswered (a panic unwinding the writer thread, or the thread gone
/// with the command still queued), so `openraft` never waits on it.
struct Flushed(Option<IOFlushed<RaftTypes>>);

impl Flushed {
    fn complete(mut self, result: io::Result<()>) {
        if let Some(callback) = self.0.take() {
            callback.io_completed(result);
        }
    }
}

impl Drop for Flushed {
    fn drop(&mut self) {
        if let Some(callback) = self.0.take() {
            callback.io_completed(Err(writer_gone()));
        }
    }
}

enum Command {
    Append {
        frames: Vec<Vec<u8>>,
        done: Flushed,
    },
    Durable {
        frame: Vec<u8>,
        done: oneshot::Sender<io::Result<()>>,
    },
    Lazy {
        frame: Vec<u8>,
    },
    /// Resolves once every earlier command is done: each batch holding
    /// an append or a marker is fsynced before the next is taken, so it
    /// needs no fsync of its own.
    Fence {
        done: oneshot::Sender<io::Result<()>>,
    },
}

impl Command {
    fn frames(&self) -> &[Vec<u8>] {
        match self {
            Self::Append { frames, .. } => frames,
            Self::Durable { frame, .. } | Self::Lazy { frame } => std::slice::from_ref(frame),
            Self::Fence { .. } => &[],
        }
    }

    fn needs_sync(&self) -> bool {
        matches!(self, Self::Append { .. } | Self::Durable { .. })
    }

    fn complete(self, flush: &Flush) {
        match self {
            Self::Append { done, .. } => done.complete(flush.reply()),
            Self::Durable { done, .. } | Self::Fence { done } => {
                let _ = done.send(flush.reply());
            }
            Self::Lazy { .. } => {}
        }
    }
}

/// `RaftLogStorage` for one group: its only writer. It is not `Clone`,
/// and every write takes `&mut self`, so writes reach the WAL thread in
/// the order they change the index; readers come from
/// [`RaftLogStorage::get_log_reader`].
///
/// ```compile_fail
/// use ourios_ingester::replicated::raft::log_store::GroupLog;
/// fn second_writer(log: &GroupLog) -> GroupLog {
///     log.clone()
/// }
/// ```
pub struct GroupLog {
    group: u32,
    index: Arc<Mutex<Index>>,
    votes: VoteLog,
    tx: Sender<Command>,
    takeovers: Option<Takeovers>,
    #[cfg(test)]
    hooks: Arc<test_hooks::Hooks>,
}

/// A read-only view of a group's log, sharing the writer's index.
#[derive(Clone)]
pub struct GroupLogReader {
    group: u32,
    index: Arc<Mutex<Index>>,
    votes: VoteLog,
}

fn lock_index(index: &Mutex<Index>) -> std::sync::MutexGuard<'_, Index> {
    index.lock().unwrap_or_else(PoisonError::into_inner)
}

fn entries_in(index: &Mutex<Index>, range: impl RangeBounds<u64>) -> Vec<Entry> {
    lock_index(index)
        .entries
        .range(range)
        .map(|(_, e)| e.clone())
        .collect()
}

impl GroupLog {
    /// Recover the group's log from `wal.root` (see [`group_dir`]) and
    /// start its writer thread. Blocking: it replays the whole WAL.
    ///
    /// # Errors
    ///
    /// The WAL cannot be opened or replayed, or holds a record this
    /// build cannot read or a log with a hole.
    pub fn open(wal: WalConfig, group: u32, votes: VoteLog) -> io::Result<Self> {
        Self::open_with(wal, group, votes, Arc::default())
    }

    pub(super) fn open_with(
        wal: WalConfig,
        group: u32,
        votes: VoteLog,
        gate: Arc<SyncGate>,
    ) -> io::Result<Self> {
        if votes.latest(group).is_some() && !holds_segments(&wal.root)? {
            return Err(lost(&wal.root));
        }
        ourios_wal::prepare_root(&wal.root)
            .map_err(|e| wal_error("prepare the group WAL root", e))?;
        sync_ancestors(&wal.root, &gate)?;
        // The writer thread holds this lock and a vote-log handle until it
        // exits, after its last queued command, so the group's WAL is never
        // replayed while a retiring writer may still append to it, and the
        // node's `votes.lock` outlives every group writer.
        let owner = take_ownership(&wal.root.join(GROUP_LOCK_FILE), OWNER_WAIT, "group log")?;
        let (wal, index, recovered) = recover(wal, &gate)?;
        let (tx, rx) = mpsc::channel();
        #[cfg(test)]
        let hooks_gate = Arc::clone(&gate);
        let writer = Writer {
            wal,
            failed: None,
            gate,
            _owner: owner,
            _votes: votes.clone(),
            #[cfg(test)]
            synced: Arc::default(),
        };
        writer.record_synced(recovered);
        #[cfg(test)]
        let synced = Arc::clone(&writer.synced);
        let thread = std::thread::Builder::new()
            .name(format!("ourios-wal-g{group}"))
            .spawn(move || writer.run(&rx))?;
        #[cfg(not(test))]
        drop(thread);
        Ok(Self {
            group,
            index: Arc::new(Mutex::new(index)),
            votes,
            tx,
            takeovers: None,
            #[cfg(test)]
            hooks: Arc::new(test_hooks::Hooks::new(hooks_gate, synced, thread)),
        })
    }

    /// Count the terms this node leads from the votes the log saves.
    pub(super) fn count_takeovers(&mut self, takeovers: Takeovers) {
        self.takeovers = Some(takeovers);
    }

    fn index(&self) -> std::sync::MutexGuard<'_, Index> {
        lock_index(&self.index)
    }

    /// Change the index and queue the command under one index lock, so
    /// the WAL always receives writes in the order the index saw them.
    /// The channel is unbounded, so the send never blocks under the lock.
    fn write(
        &mut self,
        mutate: impl FnOnce(&mut Index) -> io::Result<()>,
        command: Command,
    ) -> io::Result<()> {
        let mut index = lock_index(&self.index);
        mutate(&mut index)?;
        self.tx.send(command).map_err(|_| writer_gone())
    }

    /// Change the index, queue a marker and wait for the fsync that
    /// covers it.
    async fn durable(
        &mut self,
        mutate: impl FnOnce(&mut Index),
        record: ReplicatedRecord<'_>,
    ) -> io::Result<()> {
        let frame = codec::encode_marker(record)?;
        let (done, rx) = oneshot::channel();
        let mutate = |index: &mut Index| {
            mutate(index);
            Ok(())
        };
        self.write(mutate, Command::Durable { frame, done })?;
        rx.await.map_err(|_| writer_gone())?
    }

    /// Wait until every write queued before it is durable, failing if
    /// any of them failed.
    async fn fence(&mut self) -> io::Result<()> {
        let (done, rx) = oneshot::channel();
        self.write(|_| Ok(()), Command::Fence { done })?;
        rx.await.map_err(|_| writer_gone())?
    }
}

impl GroupLogReader {
    /// The group whose log this reads.
    #[must_use]
    pub fn group(&self) -> u32 {
        self.group
    }

    #[must_use]
    pub fn controls(&self) -> LogControls {
        lock_index(&self.index).log_controls()
    }
}

// openraft's storage traits are async; these methods answer from memory
// or hand off to the writer thread, so most never await.
#[allow(clippy::unused_async_trait_impl)]
impl RaftLogReader<RaftTypes> for GroupLogReader {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry>, io::Error> {
        Ok(entries_in(&self.index, range))
    }

    async fn read_vote(&mut self) -> Result<Option<Vote>, io::Error> {
        Ok(self.votes.latest(self.group))
    }
}

// As above.
#[allow(clippy::unused_async_trait_impl)]
impl RaftLogReader<RaftTypes> for GroupLog {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry>, io::Error> {
        Ok(entries_in(&self.index, range))
    }

    async fn read_vote(&mut self) -> Result<Option<Vote>, io::Error> {
        Ok(self.votes.latest(self.group))
    }
}

// As above: only the vote and the markers wait for an fsync.
#[allow(clippy::unused_async_trait_impl)]
impl RaftLogStorage<RaftTypes> for GroupLog {
    type LogReader = GroupLogReader;

    async fn get_log_state(&mut self) -> Result<LogState<RaftTypes>, io::Error> {
        let index = self.index();
        Ok(LogState {
            last_purged_log_id: index.last_purged,
            last_log_id: index.last_log_id(),
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        GroupLogReader {
            group: self.group,
            index: Arc::clone(&self.index),
            votes: self.votes.clone(),
        }
    }

    /// openraft requires vote and log I/O serialized: the vote waits for
    /// every earlier write of this group to be durable before it is
    /// queued on the node's shared vote log.
    async fn save_vote(&mut self, vote: &Vote) -> Result<(), io::Error> {
        self.fence().await?;
        self.votes.save(self.group, *vote).await?;
        if let Some(takeovers) = &self.takeovers {
            takeovers.saved(vote);
        }
        Ok(())
    }

    /// Not fsynced on its own: `enable_leader_restore = false` (RFC 0065
    /// §4.5) means a stale committed id only delays re-applying the log
    /// until the leader's next commit, and it can never run ahead of the
    /// entries, which precede it in the WAL.
    async fn save_committed(&mut self, committed: Option<LogId>) -> Result<(), io::Error> {
        let Some(c) = committed else {
            return Ok(());
        };
        let frame = codec::encode_marker(ReplicatedRecord::Committed(codec::wal_id(&c)))?;
        let commit = |index: &mut Index| {
            index.commit(c);
            Ok(())
        };
        self.write(commit, Command::Lazy { frame })
    }

    async fn read_committed(&mut self) -> Result<Option<LogId>, io::Error> {
        Ok(self.index().committed)
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: IOFlushed<RaftTypes>,
    ) -> Result<(), io::Error>
    where
        I: IntoIterator<Item = Entry> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let entries: Vec<Entry> = entries.into_iter().collect();
        let frames = entries
            .iter()
            .map(codec::encode_entry)
            .collect::<io::Result<Vec<_>>>()?;
        if let Some(big) = frames
            .iter()
            .find(|f| f.len() > ourios_wal::MAX_FRAME_BYTES)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("a {}-byte entry exceeds the WAL frame cap", big.len()),
            ));
        }
        let command = Command::Append {
            frames,
            done: Flushed(Some(callback)),
        };
        self.write(|index| index.extend(entries), command)
    }

    async fn truncate_after(&mut self, last: Option<LogId>) -> Result<(), io::Error> {
        let record = ReplicatedRecord::TruncateAfter(last.as_ref().map(codec::wal_id));
        self.durable(|index| index.truncate_after(last), record)
            .await
    }

    async fn purge(&mut self, upto: LogId) -> Result<(), io::Error> {
        let record = ReplicatedRecord::Purge(codec::wal_id(&upto));
        self.durable(|index| index.purge(upto), record).await
    }
}

struct Writer {
    wal: Wal,
    /// Set by the first failed append or sync: the WAL's tail is then
    /// unknown, so every later command fails.
    failed: Option<String>,
    gate: Arc<SyncGate>,
    /// Both released when the thread exits; see [`GroupLog::open`].
    _owner: Owner,
    _votes: VoteLog,
    #[cfg(test)]
    synced: Arc<Mutex<Option<WalOffset>>>,
}

impl Writer {
    fn run(mut self, rx: &Receiver<Command>) {
        while let Ok(first) = rx.recv() {
            let mut batch = vec![first];
            batch.extend(rx.try_iter());
            let flush = self.flush(&batch);
            let crashed = matches!(flush, Flush::Crashed);
            for command in batch {
                command.complete(&flush);
            }
            if crashed {
                return;
            }
        }
    }

    fn flush(&mut self, batch: &[Command]) -> Flush {
        if let Some(reason) = &self.failed {
            return Flush::Failed(io::Error::other(format!(
                "the group WAL failed earlier: {reason}"
            )));
        }
        for frame in batch.iter().flat_map(Command::frames) {
            if let Err(e) = self.wal.append(FrameKind::ReplicatedEntry, frame) {
                return self.fail(&e);
            }
        }
        if !batch.iter().any(Command::needs_sync) {
            return Flush::Done;
        }
        let wal = &mut self.wal;
        let sync = || wal.sync().map_err(|e| io::Error::other(e.to_string()));
        match sync_through(&self.gate, sync) {
            None => Flush::Crashed,
            Some(Ok(offset)) => {
                self.record_synced(offset);
                Flush::Done
            }
            Some(Err(e)) => self.fail(&e),
        }
    }

    #[cfg(test)]
    fn record_synced(&self, offset: WalOffset) {
        *self.synced.lock().unwrap_or_else(PoisonError::into_inner) = Some(offset);
    }

    #[cfg(not(test))]
    #[allow(clippy::unused_self)] // the test build records the offset for its crash model
    fn record_synced(&self, _: WalOffset) {}

    fn fail(&mut self, e: &impl std::fmt::Display) -> Flush {
        let reason = e.to_string();
        self.failed = Some(reason.clone());
        Flush::Failed(io::Error::other(reason))
    }
}

#[cfg(test)]
pub(super) mod test_hooks {
    use std::thread::JoinHandle;

    use super::*;

    /// What the crash tests need from a running log.
    pub(in crate::replicated::raft) struct Hooks {
        gate: Arc<SyncGate>,
        synced: Arc<Mutex<Option<WalOffset>>>,
        thread: Mutex<Option<JoinHandle<()>>>,
    }

    impl Hooks {
        pub(super) fn new(
            gate: Arc<SyncGate>,
            synced: Arc<Mutex<Option<WalOffset>>>,
            thread: JoinHandle<()>,
        ) -> Self {
            Self {
                gate,
                synced,
                thread: Mutex::new(Some(thread)),
            }
        }
    }

    impl GroupLog {
        pub(in crate::replicated::raft) fn open_gated(
            wal: WalConfig,
            group: u32,
            votes: VoteLog,
            gate: Arc<SyncGate>,
        ) -> io::Result<Self> {
            Self::open_with(wal, group, votes, gate)
        }

        /// Stop the writer once every command it holds is done; no other
        /// handle on the log may be alive.
        pub(in crate::replicated::raft) fn shutdown(self) {
            let thread = self
                .hooks
                .thread
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            drop(self);
            if let Some(thread) = thread {
                let _ = thread.join();
            }
        }

        /// A handle that can crash the writer after the log itself has
        /// moved into a task.
        pub(in crate::replicated::raft) fn hooks(&self) -> Arc<Hooks> {
            Arc::clone(&self.hooks)
        }

        pub(in crate::replicated::raft) fn crash(&self) -> Option<WalOffset> {
            self.hooks.crash()
        }
    }

    impl Hooks {
        /// Stop the writer, held at the gate, before its fsync, and
        /// return the boundary the last successful sync made durable.
        pub(in crate::replicated::raft) fn crash(&self) -> Option<WalOffset> {
            self.gate.crash();
            let thread = self
                .thread
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(thread) = thread {
                let _ = thread.join();
            }
            *self.synced.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }
}
