//! A group's state machine (RFC 0065 §6.1): what it has applied and the
//! membership it applied last.
//!
//! It lives in memory, except for its snapshot: `openraft` purges the log
//! up to a snapshot it installs or builds, so the snapshot is recorded
//! in `wal_root/groups/<group>/snapshot` before the call returns,
//! and a restart opens applied through it and replays the log after it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures_util::TryStreamExt as _;
use openraft::storage::{EntryResponder, RaftSnapshotBuilder, RaftStateMachine};
use openraft::type_config::alias::{SnapshotOf, StoredMembershipOf};
use openraft::{EntryPayload, OptionalSend};

use super::gate::SyncGate;
use super::snapshot_record::{self, SnapshotMeta};
use super::types::{LogId, RaftTypes};

type StoredMembership = StoredMembershipOf<RaftTypes>;

struct State {
    applied: Option<LogId>,
    membership: StoredMembership,
    snapshot: Option<SnapshotMeta>,
}

/// Where the snapshot is recorded, and the gate its fsyncs pass.
struct Record {
    dir: PathBuf,
    gate: Arc<SyncGate>,
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
            applied: snapshot.as_ref().and_then(|meta| meta.last_log_id),
            membership: snapshot
                .as_ref()
                .map(|meta| meta.last_membership.clone())
                .unwrap_or_default(),
            snapshot,
        };
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
            record: Arc::new(Record {
                dir: dir.to_path_buf(),
                gate,
            }),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Make `meta` the recorded snapshot, durably, off the runtime's
    /// workers.
    async fn record(&self, meta: SnapshotMeta) -> io::Result<()> {
        let record = Arc::clone(&self.record);
        tokio::task::spawn_blocking(move || {
            snapshot_record::write(&record.dir, &meta, &record.gate)
        })
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)))
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
}

impl RaftSnapshotBuilder<RaftTypes> for GroupSm {
    type SnapshotData = ();

    async fn build_snapshot(&mut self) -> Result<SnapshotOf<RaftTypes, ()>, io::Error> {
        let meta = {
            let state = self.state();
            SnapshotMeta {
                last_log_id: state.applied,
                last_membership: state.membership.clone(),
            }
        };
        self.record(meta.clone()).await?;
        self.state().snapshot = Some(meta.clone());
        Ok(SnapshotOf::<RaftTypes, ()> { meta, snapshot: () })
    }
}

// openraft's traits are async; only the snapshot calls wait, on the
// record's fsyncs.
#[allow(clippy::unused_async_trait_impl)]
impl RaftStateMachine<RaftTypes> for GroupSm {
    type SnapshotData = ();
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
                if let EntryPayload::Membership(m) = entry.payload {
                    state.membership = StoredMembership::new(Some(entry.log_id), m);
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

    async fn install_snapshot(&mut self, meta: &SnapshotMeta, (): ()) -> Result<(), io::Error> {
        self.record(meta.clone()).await?;
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
