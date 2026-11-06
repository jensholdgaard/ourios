//! A group's state machine (RFC 0065 §6.1): what it has applied and the
//! membership it applied last.
//!
//! It lives in memory: with purge not yet built, every restart replays
//! the group log from its first entry.

use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures_util::TryStreamExt as _;
use openraft::storage::{EntryResponder, RaftSnapshotBuilder, RaftStateMachine};
use openraft::type_config::alias::{SnapshotMetaOf, SnapshotOf, StoredMembershipOf};
use openraft::{EntryPayload, OptionalSend};

use super::types::{LogId, RaftTypes};

type StoredMembership = StoredMembershipOf<RaftTypes>;

#[derive(Default)]
struct State {
    applied: Option<LogId>,
    membership: StoredMembership,
    snapshot: Option<SnapshotMetaOf<RaftTypes>>,
}

/// Clones share one state.
#[derive(Clone, Default)]
pub struct GroupSm(Arc<Mutex<State>>);

impl GroupSm {
    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
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

// openraft's traits are async; this state machine answers from memory.
#[allow(clippy::unused_async_trait_impl)]
impl RaftSnapshotBuilder<RaftTypes> for GroupSm {
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

// openraft's traits are async; this state machine answers from memory.
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
