//! One mined group as a tail read holds it (RFC 0065 §6.7): its raft and
//! state machine for the read point, how far its miner has followed, what
//! it keeps, where its miner answers for template definitions, and the
//! group's log id, which every served row's position carries.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use ourios_core::tenant::TenantId;
use tokio::sync::watch;

use super::super::flushing::{Source, Unread, formed_log};
use super::super::kept::{Definitions, Kept, TemplateKey};
use super::super::state_machine::GroupSm;
use super::GroupRaft;
use super::lookup::Asker;

/// A mined group's handles; clones share them.
#[derive(Clone)]
pub struct Tailed {
    pub(in crate::replicated::raft) raft: GroupRaft,
    pub(in crate::replicated::raft) sm: GroupSm,
    pub(in crate::replicated::raft) mined: watch::Receiver<Option<u64>>,
    pub(super) kept: Weak<Mutex<Kept>>,
    pub(super) asker: Asker,
    pub(super) formed: Arc<FormedLog>,
}

impl Tailed {
    /// `read` of what the group keeps, under its lock; `None` once its
    /// miner's task has ended. The reader holds none of it past the call.
    pub fn read_kept<T>(&self, read: impl FnOnce(&Kept) -> T) -> Option<T> {
        read_kept(&self.kept, &self.mined, read)
    }

    /// The definitions of `wanted` for `tenant` this replica holds: its
    /// miner's and the older versions its kept audit events bind, both read
    /// by its task in one step. `None` once its task has ended.
    pub async fn definitions(
        &self,
        tenant: &TenantId,
        wanted: BTreeSet<TemplateKey>,
    ) -> Option<Definitions> {
        self.asker.ask(tenant.clone(), wanted).await
    }

    /// The group's log id, read once from `formed.json`.
    ///
    /// # Errors
    ///
    /// Why it is not known yet: this replica reads no store, the group has
    /// not formed, or the record does not read.
    pub async fn log(&self) -> Result<u32, String> {
        self.formed.read().await
    }
}

/// `read` of what `kept` holds, under its lock, while the miner's task that
/// owns it runs. The reader holds none of it past the call, so only the
/// task keeps it alive and an ended task's output is neither served nor
/// reported.
pub(super) fn read_kept<T>(
    kept: &Weak<Mutex<Kept>>,
    followed: &watch::Receiver<Option<u64>>,
    read: impl FnOnce(&Kept) -> T,
) -> Option<T> {
    let kept = kept.upgrade()?;
    let kept = kept.lock().unwrap_or_else(PoisonError::into_inner);
    // A read still running as the task ended can hold `Kept` past the end;
    // the task's `followed` closes when it ends.
    if followed.has_changed().is_err() {
        return None;
    }
    Some(read(&kept))
}

/// A group's log id, as its `formed.json` records it, once read.
pub(super) struct FormedLog {
    source: Option<(Source, u32)>,
    read: OnceLock<u32>,
}

impl FormedLog {
    /// `group`'s, read from `source`'s store; with none, never known.
    pub(super) fn new(source: Option<Source>, group: u32) -> Self {
        Self {
            source: source.map(|source| (source, group)),
            read: OnceLock::new(),
        }
    }

    async fn read(&self) -> Result<u32, String> {
        if let Some(log) = self.read.get() {
            return Ok(*log);
        }
        let Some((source, group)) = self.source.clone() else {
            return Err("this replica reads no store".to_owned());
        };
        let read = tokio::task::spawn_blocking(move || formed_log(&source, group))
            .await
            .map_err(|e| e.to_string())?;
        match read {
            Ok(log) => Ok(*self.read.get_or_init(|| log)),
            Err(Unread::Absent) => Err("the group has not formed".to_owned()),
            Err(Unread::Failed { error, .. }) => Err(error),
        }
    }
}
