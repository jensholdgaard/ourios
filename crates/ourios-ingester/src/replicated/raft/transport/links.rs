//! Peer links the cluster harness can cut: every RPC and stream message
//! between a cut pair fails as an unreachable peer. The production pool
//! has none.

#[cfg(test)]
use std::collections::HashSet;
#[cfg(test)]
use std::sync::{Arc, Mutex, PoisonError};

use tonic::Status;

use super::super::types::NodeId;
#[cfg(test)]
use super::Peers;

#[cfg(test)]
impl Peers {
    /// Every RPC and stream message between `a` and `b`, either way,
    /// fails as an unreachable peer until [`Peers::heal`].
    pub(crate) fn cut(&self, a: NodeId, b: NodeId) {
        self.links.set(a, b, true);
    }

    pub(crate) fn heal(&self, a: NodeId, b: NodeId) {
        self.links.set(a, b, false);
    }
}

/// The links the cluster harness has cut, as unordered node pairs.
#[cfg(test)]
#[derive(Clone, Default)]
pub(super) struct Links(Arc<Mutex<HashSet<(NodeId, NodeId)>>>);

#[cfg(test)]
impl Links {
    pub(super) fn new() -> Self {
        Self::default()
    }

    fn set(&self, a: NodeId, b: NodeId, cut: bool) {
        let mut links = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if cut {
            links.insert((a.min(b), a.max(b)));
        } else {
            links.remove(&(a.min(b), a.max(b)));
        }
    }

    pub(super) fn is_cut(&self, a: NodeId, b: NodeId) -> bool {
        let links = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        links.contains(&(a.min(b), a.max(b)))
    }
}

/// The production pool has no cut links.
#[cfg(not(test))]
#[derive(Clone)]
pub(super) struct Links;

#[cfg(not(test))]
impl Links {
    pub(super) fn new() -> Self {
        Self
    }

    #[allow(clippy::unused_self)] // the test build's links have state
    pub(super) fn is_cut(&self, _: NodeId, _: NodeId) -> bool {
        false
    }
}

/// One direction of a link: this node and the peer it calls.
#[derive(Clone)]
pub(super) struct Link {
    pub(super) links: Links,
    pub(super) from: NodeId,
    pub(super) to: NodeId,
}

impl Link {
    pub(super) fn check(&self) -> Result<(), Status> {
        if self.links.is_cut(self.from, self.to) {
            return Err(Status::unavailable("the link to the peer is cut"));
        }
        Ok(())
    }
}
