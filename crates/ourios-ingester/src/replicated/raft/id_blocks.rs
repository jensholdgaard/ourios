//! The template-id blocks a group's leader keeps ahead in its log (RFC
//! 0065 §6.5): control kind `0x03`, each block `(after, through]` reserved
//! in the store by RFC 0059's compare-and-swap before its entry is
//! appended.
//!
//! A block is reserved fresh for each entry and never held across passes,
//! so a leader deposed between the reservation and the commit leaves only
//! skipped ids: the next reservation, by any leader, starts above it.
//!
//! The leader counts the blocks from its own log ([`Ahead::of`]); the
//! flusher's report of the blocks it has not started replaces that count
//! once the `Propose` call carries it.

use ourios_miner::cluster::IdSpaceExhausted;
use ourios_parquet::Store;
use ourios_wal::Control;

use super::learners::Group;
use super::snapshot_record::IdBlock;
use crate::template_ids::{self, TemplateIdsError, names};

/// The blocks the leader keeps ahead of the group's miner: "the target
/// RFC 0059's refiller keeps", its current block and two ready ones.
pub const AHEAD: u32 = 3;

/// The id-block control kind.
pub const KIND: u8 = Control::IdBlock {
    after: 0,
    through: 1,
}
.kind();

/// The id blocks a group holds ahead of its miner, as the leader counts
/// them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ahead {
    /// The blocks counted as not yet started.
    pub blocks: u32,
    /// The latest block applied, which the next must start at or above.
    pub latest: Option<IdBlock>,
}

impl Ahead {
    /// The leader's own count: the snapshot's latest block and every block
    /// after the snapshot that a replica would apply, committed or not.
    #[must_use]
    pub fn of(group: Group<'_>) -> Self {
        group
            .log
            .held_controls()
            .blocks_over(group.sm.snapshot().as_ref())
    }

    /// One block on from `latest`.
    #[must_use]
    pub(super) fn starting(latest: Option<IdBlock>) -> Self {
        Self {
            blocks: u32::from(latest.is_some()),
            latest,
        }
    }

    /// This count after a block entry: a block starting below the latest
    /// one's end is ignored by every replica, so it counts for nothing.
    #[must_use]
    pub(super) fn then(self, block: Option<IdBlock>) -> Self {
        let follows = |b: &IdBlock| self.latest.is_none_or(|p| b.after() >= p.through());
        match block.filter(follows) {
            Some(block) => Self {
                blocks: self.blocks.saturating_add(1),
                latest: Some(block),
            },
            None => self,
        }
    }

    /// The floor to reserve the next block above, while fewer than
    /// [`AHEAD`] are held.
    #[must_use]
    pub fn short(self) -> Option<u64> {
        (self.blocks < AHEAD).then(|| self.latest.map_or(0, IdBlock::through))
    }
}

/// Why a group short of blocks got none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The store has no template-id high-water. Only the bootstrap creates
    /// it, never a reservation (RFC 0059 §3.2).
    Absent,
    /// The reservation failed, with the failure's `error.type`.
    Failed(&'static str),
}

/// What one pass did for a group short of blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopUp {
    /// The block was reserved, appended and committed.
    Appended(IdBlock),
    /// Nothing was reserved, so nothing was appended.
    Refused(Refusal),
}

/// The block's entry.
#[must_use]
pub fn control(block: IdBlock) -> Control {
    Control::IdBlock {
        after: block.after(),
        through: block.through(),
    }
}

/// Durably reserve a block above `floor` in `store` for `group`, off the
/// async runtime, reporting a failure.
///
/// # Errors
///
/// The [`Refusal`]; nothing is to be appended.
pub async fn reserve(store: &Store, floor: u64, group: u32) -> Result<IdBlock, Refusal> {
    let store = store.clone();
    let reserved = tokio::task::spawn_blocking(move || template_ids::reserve(&store, floor)).await;
    let failed = match reserved {
        Ok(Ok(block)) => match IdBlock::new(block.after(), block.through()) {
            Some(block) => return Ok(block),
            None => TemplateIdsError::Exhausted(IdSpaceExhausted),
        },
        Ok(Err(e)) => e,
        Err(e) => {
            report(group, OTHER, &e);
            return Err(Refusal::Failed(OTHER));
        }
    };
    report(group, failed.error_type(), &failed);
    match failed {
        TemplateIdsError::HighWaterDeleted => Err(Refusal::Absent),
        e => Err(Refusal::Failed(e.error_type())),
    }
}

/// The `error.type` of a reservation whose task did not finish.
const OTHER: &str = "_OTHER";

fn report(group: u32, error_type: &'static str, error: &dyn std::fmt::Display) {
    tracing::warn!(
        name: names::REFILL_FAILED,
        {
            { ourios_semconv::OURIOS_WAL_GROUP } = group,
            { crate::metrics::ERROR_TYPE } = error_type,
        },
        "group {group}'s leader reserved no template-id block and appends none: {error}",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(after: u64, through: u64) -> Option<IdBlock> {
        IdBlock::new(after, through)
    }

    #[test]
    fn a_fresh_group_is_short_from_zero() {
        assert_eq!(Ahead::starting(None).short(), Some(0));
    }

    #[test]
    fn the_snapshots_block_counts_and_sets_the_floor() {
        let ahead = Ahead::starting(block(0, 1_000));
        assert_eq!(ahead.blocks, 1);
        assert_eq!(ahead.short(), Some(1_000));
    }

    #[test]
    fn three_following_blocks_are_enough() {
        let ahead = Ahead::starting(None)
            .then(block(0, 1_000))
            .then(block(1_000, 2_000))
            .then(block(2_500, 3_500));
        assert_eq!(ahead.blocks, AHEAD);
        assert_eq!(ahead.short(), None);
    }

    #[test]
    fn an_ignored_block_counts_for_nothing() {
        let ahead = Ahead::starting(block(1_000, 2_000)).then(block(1_500, 2_500));
        assert_eq!(ahead, Ahead::starting(block(1_000, 2_000)));
        assert_eq!(ahead.then(None), ahead, "an invalid body counts neither");
    }

    #[test]
    fn the_kind_is_the_id_blocks() {
        assert_eq!(KIND, 0x03);
    }
}
