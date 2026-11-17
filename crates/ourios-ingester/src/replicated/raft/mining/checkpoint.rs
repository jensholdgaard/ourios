//! A group's checkpoint as its miner holds it (RFC 0065 §6.5): every
//! tenant's state, the id allocator and the configuration in force, all
//! at the last entry it applied, which is the cut.

use std::fmt;

use ourios_miner::cluster::MinerCluster;
use ourios_miner::snapshot::{LogMark, WalHighWater, snapshot};

use super::{GroupMiner, Stage};
use crate::replicated::raft::bundle::{Allocator, Bundle, BundleError, CutId, TenantEntry};
use crate::replicated::raft::codec::wal_id;
use crate::replicated::raft::snapshot_record::{GroupMinerConfig, IdBlock};

/// A cut's bundle and its tenants' `.snap` bytes, in its tenant order:
/// what `bundle::write::write` takes.
#[derive(Debug)]
pub struct Captured {
    pub bundle: Bundle,
    pub snapshots: Vec<Vec<u8>>,
}

/// Why a miner that could cut a checkpoint did not.
#[derive(Debug)]
#[non_exhaustive]
pub enum CaptureError {
    /// A tenant's state did not encode.
    Snapshot { tenant: String, detail: String },
    /// The state it holds is no bundle a replica would restore.
    Bundle(BundleError),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshot { tenant, detail } => {
                write!(f, "tenant {tenant}'s snapshot did not encode: {detail}")
            }
            Self::Bundle(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CaptureError {}

impl GroupMiner {
    /// The checkpoint at the last entry this miner applied, for the
    /// group's log id `log`. `None` while no cut is allowed: before the
    /// first configuration (RFC 0065 §6.5 ignores such a cut), while it
    /// still holds batches to read back, once stopped, or before any entry.
    ///
    /// # Errors
    ///
    /// The [`CaptureError`] when its state makes no bundle.
    #[must_use]
    pub fn capture(&self, log: u32) -> Option<Result<Captured, CaptureError>> {
        let Stage::Mining {
            miner,
            held: None,
            config,
        } = &self.stage
        else {
            return None;
        };
        let at = wal_id(&self.last?);
        let cut = CutId {
            log,
            seq: at.index,
            term: at.term,
        };
        Some(captured(miner, cut, *config, &self.blocks.state()))
    }

    /// The index of the last entry applied that changed what a bundle
    /// holds: not a checkpoint or a flusher assignment.
    #[must_use]
    pub fn changed_through(&self) -> Option<u64> {
        self.changed
    }

    /// The last entry applied, id blocks included: a report's `mined`.
    #[must_use]
    pub fn followed(&self) -> Option<crate::replicated::raft::types::LogId> {
        self.last
    }

    /// The index of the last entry applied, if any.
    #[must_use]
    pub fn applied_through(&self) -> Option<u64> {
        self.last.map(|last| last.index)
    }

    /// The miner's cluster, once a configuration has applied.
    #[cfg(test)]
    pub(super) fn cluster(&self) -> Option<&MinerCluster> {
        match &self.stage {
            Stage::Mining { miner, .. } => Some(miner),
            Stage::Unconfigured { .. } | Stage::Stopped(_) => None,
        }
    }
}

/// Every tenant `miner` holds, its state marked with `cut`, under
/// `config`, and its allocator drawing from `blocks`.
fn captured(
    miner: &MinerCluster,
    cut: CutId,
    config: GroupMinerConfig,
    (current, pending): &(Option<IdBlock>, Vec<IdBlock>),
) -> Result<Captured, CaptureError> {
    let mark = WalHighWater::Replicated(LogMark {
        log: cut.log,
        seq: cut.seq,
    });
    let mut tenants = Vec::new();
    let mut snapshots = Vec::new();
    for tenant in miner.tenant_ids() {
        let mut state = miner.snapshot_state(&tenant);
        state.wal_high_water = Some(mark.clone());
        let bytes = snapshot(&state).map_err(|e| CaptureError::Snapshot {
            tenant: tenant.as_str().to_owned(),
            detail: e.to_string(),
        })?;
        tenants.push(TenantEntry::of(tenant, config, &bytes));
        snapshots.push(bytes);
    }
    let ids = Allocator::new(miner.highest_allocated() + 1, *current, pending.clone())
        .map_err(CaptureError::Bundle)?;
    let bundle = Bundle::new(cut, ids, tenants).map_err(CaptureError::Bundle)?;
    Ok(Captured { bundle, snapshots })
}

#[cfg(test)]
mod tests;
