//! A reader's question to a tail replica's miner (RFC 0065 §6.7): the
//! definitions of the templates the rows it serves reference. The miner
//! belongs to the group's task, which answers between steps, so an answer
//! is the miner's state after some whole step, never part of one.

use std::collections::BTreeSet;

use ourios_core::tenant::TenantId;
use tokio::sync::{mpsc, oneshot};

use super::super::kept::{Definitions, TemplateKey};
use super::super::mining::GroupMiner;

/// Questions a group's task holds before a reader waits to ask.
const PENDING: usize = 16;

/// One reader's question, and where its answer goes.
pub(super) struct Lookup {
    tenant: TenantId,
    wanted: BTreeSet<TemplateKey>,
    answer: oneshot::Sender<Definitions>,
}

impl Lookup {
    /// Answer from `miner`.
    pub(super) fn answer(self, miner: &GroupMiner) {
        // A reader that gave up has dropped its end: there is no one to tell.
        self.answer
            .send(miner.definitions(&self.tenant, &self.wanted))
            .ok();
    }
}

/// Where readers ask a group's task.
#[derive(Clone)]
pub(super) struct Asker(mpsc::Sender<Lookup>);

impl Asker {
    /// `tenant`'s definitions of `wanted`; `None` once the task has ended.
    pub(super) async fn ask(
        &self,
        tenant: TenantId,
        wanted: BTreeSet<TemplateKey>,
    ) -> Option<Definitions> {
        let (answer, answered) = oneshot::channel();
        let lookup = Lookup {
            tenant,
            wanted,
            answer,
        };
        self.0.send(lookup).await.ok()?;
        answered.await.ok()
    }
}

/// A group task's questions, and where readers ask them.
pub(super) fn channel() -> (Asker, mpsc::Receiver<Lookup>) {
    let (asker, lookups) = mpsc::channel(PENDING);
    (Asker(asker), lookups)
}

/// The next question, never resolving without one to answer.
pub(super) async fn asked(lookups: &mut Option<mpsc::Receiver<Lookup>>) -> Lookup {
    match lookups {
        Some(lookups) => match lookups.recv().await {
            Some(lookup) => lookup,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}
