//! A group miner restored from a checkpoint's bundle (RFC 0065 §6.5):
//! the configuration in force, every tenant's state, the id allocator's
//! cursor and its current and pending blocks, all at the cut, so it mints
//! the same next id and the same rows as a miner that mined through it.

use std::fmt;

use ourios_miner::cluster::IdCursor;
use ourios_miner::snapshot::SnapshotState;

use super::{GroupLogRead, GroupMiner, Stage};
use crate::replicated::raft::bundle::Bundle;
use crate::replicated::raft::snapshot_record::GroupMinerConfig;
use crate::replicated::raft::types::LogId;

/// Why a bundle's state does not restore.
#[derive(Debug)]
#[non_exhaustive]
pub enum RestoreError {
    /// One snapshot per tenant the bundle names, and another number came.
    Snapshots { tenants: usize, snapshots: usize },
    /// The allocator's cursor does not seat.
    Cursor(String),
    /// A tenant's state does not restore.
    Tenant { tenant: String, detail: String },
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshots { tenants, snapshots } => {
                write!(f, "{snapshots} snapshots for {tenants} tenants")
            }
            Self::Cursor(detail) => write!(f, "the id allocator does not seat: {detail}"),
            Self::Tenant { tenant, detail } => {
                write!(f, "tenant {tenant}'s state does not restore: {detail}")
            }
        }
    }
}

impl std::error::Error for RestoreError {}

impl GroupMiner {
    /// A miner over `log` restored from `bundle` and `states`, its
    /// tenants' snapshots in its tenant order, at `cut` under `config`,
    /// the configuration in force there; it mines from the entry after.
    ///
    /// # Errors
    ///
    /// The [`RestoreError`] naming what does not restore.
    pub fn restored(
        log: impl GroupLogRead + 'static,
        (bundle, states): (&Bundle, &[SnapshotState]),
        (cut, config): (LogId, GroupMinerConfig),
    ) -> Result<Self, RestoreError> {
        let tenants = bundle.tenants();
        if tenants.len() != states.len() {
            return Err(RestoreError::Snapshots {
                tenants: tenants.len(),
                snapshots: states.len(),
            });
        }
        let mut miner = Self::new(log);
        let ids = bundle.ids();
        miner.blocks.seat(ids.current(), ids.pending());
        let mut cluster = miner.cluster_under(config);
        // The range in hand is the current block's, through its end.
        let end = ids
            .current()
            .map_or(ids.next(), |block| block.through() + 1);
        let cursor = IdCursor::new(ids.next(), end)
            .ok_or_else(|| RestoreError::Cursor(format!("next {} before {end}", ids.next())))?;
        cluster
            .restore_id_cursor(cursor)
            .map_err(|e| RestoreError::Cursor(e.to_string()))?;
        for (entry, state) in tenants.iter().zip(states) {
            cluster
                .restore_tenant(&entry.tenant, state)
                .map_err(|e| RestoreError::Tenant {
                    tenant: entry.tenant.as_str().to_owned(),
                    detail: e.to_string(),
                })?;
        }
        miner.stage = Stage::Mining {
            miner: Box::new(cluster),
            held: None,
            config,
        };
        miner.last = Some(cut);
        miner.changed = Some(cut.index);
        Ok(miner)
    }
}

#[cfg(test)]
mod tests;
