//! The cluster-wide `template_id` allocator and its durable reservations
//! (RFC 0001 §6.1 / §6.9, the 2026-10-05 template-id high-water
//! amendment).
//!
//! The allocator hands out ids only from a range a [`IdReserver`] has
//! reserved. The default reserver reserves everything up front, which is
//! the in-memory behaviour tests and benchmarks rely on; the ingester
//! installs one that writes each reservation to object storage before
//! the range is used, so no id is ever issued twice, whatever a restart
//! loses.

use std::fmt;

use ourios_core::tenant::TenantId;

use super::MinerCluster;

/// The `ourios.miner.parse_failure.reason` member counted when a mint
/// found no reserved id and the reservation itself failed.
pub const ID_RESERVATION_FAILED: &str = "id_reservation_failed";

/// A reserved block of ids: every id `i` with `after < i <= through`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdBlock {
    after: u64,
    through: u64,
}

impl IdBlock {
    /// The block `(after, through]`, or `None` when it is empty.
    #[must_use]
    pub fn new(after: u64, through: u64) -> Option<Self> {
        (after < through).then_some(Self { after, through })
    }

    /// The highest id already issued below the block.
    #[must_use]
    pub fn after(self) -> u64 {
        self.after
    }

    /// The block's highest id.
    #[must_use]
    pub fn through(self) -> u64 {
        self.through
    }
}

/// Why a reservation failed.
#[derive(Debug)]
pub struct IdReservationError(Box<dyn std::error::Error + Send + Sync>);

impl IdReservationError {
    pub fn new(source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self(source.into())
    }
}

impl fmt::Display for IdReservationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "template-id reservation failed: {}", self.0)
    }
}

impl std::error::Error for IdReservationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

/// The `u64` id space has no id left above the one named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdSpaceExhausted;

impl fmt::Display for IdSpaceExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("no template id is left above u64::MAX - 1")
    }
}

impl std::error::Error for IdSpaceExhausted {}

/// Durably reserves blocks of template ids.
pub trait IdReserver: Send {
    /// Reserve a block lying wholly above `floor`, the highest id this
    /// cluster must never issue again. The block is durable before this
    /// returns.
    ///
    /// # Errors
    ///
    /// [`IdReservationError`] when the reservation could not be made
    /// durable; the miner then issues no id.
    fn reserve(&mut self, floor: u64) -> Result<IdBlock, IdReservationError>;
}

/// Reserves the whole id space at once: no durability, the in-memory
/// behaviour.
struct Unreserved;

impl IdReserver for Unreserved {
    fn reserve(&mut self, floor: u64) -> Result<IdBlock, IdReservationError> {
        IdBlock::new(floor, u64::MAX - 1).ok_or_else(|| IdReservationError::new(IdSpaceExhausted))
    }
}

/// The allocator: the next id and the end (exclusive) of the reserved
/// range it comes from.
pub(super) struct IdRange {
    next: u64,
    end: u64,
    reserver: Box<dyn IdReserver>,
}

impl IdRange {
    /// Start at 1, so 0 stays the `NO_TEMPLATE` sentinel.
    pub(super) fn new() -> Self {
        Self {
            next: 1,
            end: u64::MAX,
            reserver: Box::new(Unreserved),
        }
    }

    /// The id the next allocation takes. Meaningful only once
    /// [`Self::ensure`] has succeeded.
    pub(super) fn peek(&self) -> u64 {
        self.next
    }

    /// Take the id [`Self::peek`] named. `next < end <= u64::MAX` after a
    /// successful [`Self::ensure`], so this never overflows.
    pub(super) fn consume(&mut self) {
        self.next += 1;
    }

    /// Make at least one reserved id available.
    fn ensure(&mut self) -> Result<(), IdReservationError> {
        if self.next < self.end {
            return Ok(());
        }
        let floor = self.next - 1;
        let block = self.reserver.reserve(floor)?;
        if block.after < floor {
            return Err(IdReservationError::new(format!(
                "reserved block ({}, {}] reaches below issued id {floor}",
                block.after, block.through
            )));
        }
        self.end = block
            .through
            .checked_add(1)
            .ok_or_else(|| IdReservationError::new(IdSpaceExhausted))?;
        self.next = block.after + 1;
        Ok(())
    }

    /// Never allocate `issued` or any id below it.
    fn allocate_past(&mut self, issued: u64) -> Result<(), IdSpaceExhausted> {
        let floor = issued.checked_add(1).ok_or(IdSpaceExhausted)?;
        self.next = self.next.max(floor);
        Ok(())
    }
}

impl MinerCluster {
    /// Draw template ids only from blocks `reserver` reserves. The range
    /// in hand is dropped, so the next allocation reserves first.
    #[must_use]
    pub fn with_id_reserver(mut self, reserver: Box<dyn IdReserver>) -> Self {
        self.ids.reserver = reserver;
        self.ids.end = self.ids.next;
        self
    }

    /// Never allocate `issued` or any id below it again. Startup recovery
    /// seats the allocator above the durable template-id high-water with
    /// this, before replay mints anything.
    ///
    /// # Errors
    ///
    /// [`IdSpaceExhausted`] when `issued` is `u64::MAX`.
    pub fn allocate_past_issued(&mut self, issued: u64) -> Result<(), IdSpaceExhausted> {
        self.ids.allocate_past(issued)
    }

    /// The highest id this cluster has allocated or restored, or 0.
    #[must_use]
    pub fn highest_allocated(&self) -> u64 {
        self.ids.next - 1
    }

    /// Whether a fresh id can be allocated now, reserving a block first
    /// when the range in hand is used up.
    pub(super) fn ids_ready(&mut self) -> bool {
        self.ids.ensure().is_ok()
    }

    /// Why a fresh template cannot be minted for `tenant`, if it cannot:
    /// the RFC 0023 §3.1 per-tenant ceiling, or no reservable id.
    pub(super) fn mint_blocked(
        &mut self,
        tenant: &TenantId,
        max_templates: u32,
    ) -> Option<&'static str> {
        let at_ceiling = self
            .tenants
            .get(tenant)
            .is_some_and(|s| s.leaf_count + s.owned_adopted_count >= max_templates as usize);
        match at_ceiling {
            true => Some("template_ceiling"),
            false if !self.ids_ready() => Some(ID_RESERVATION_FAILED),
            false => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Hands out the queued results in order, recording each floor.
    struct Scripted {
        results: Vec<Result<IdBlock, IdReservationError>>,
        floors: Arc<Mutex<Vec<u64>>>,
    }

    impl IdReserver for Scripted {
        fn reserve(&mut self, floor: u64) -> Result<IdBlock, IdReservationError> {
            self.floors.lock().expect("floors").push(floor);
            self.results.remove(0)
        }
    }

    fn block(after: u64, through: u64) -> IdBlock {
        IdBlock::new(after, through).expect("non-empty block")
    }

    fn range(results: Vec<Result<IdBlock, IdReservationError>>) -> (IdRange, Arc<Mutex<Vec<u64>>>) {
        let floors = Arc::new(Mutex::new(Vec::new()));
        let mut range = IdRange::new();
        range.reserver = Box::new(Scripted {
            results,
            floors: Arc::clone(&floors),
        });
        range.end = range.next;
        (range, floors)
    }

    fn take(range: &mut IdRange) -> Result<u64, IdReservationError> {
        range.ensure()?;
        let id = range.peek();
        range.consume();
        Ok(id)
    }

    #[test]
    fn ids_come_only_from_reserved_blocks_and_skip_the_gap_between_them() {
        let (mut range, floors) = range(vec![Ok(block(0, 2)), Ok(block(10, 11))]);
        let ids: Vec<u64> = (0..3).map(|_| take(&mut range).expect("id")).collect();
        assert_eq!(ids, [1, 2, 11]);
        assert_eq!(*floors.lock().expect("floors"), [0, 2]);
    }

    #[test]
    fn a_failed_reservation_issues_nothing_and_the_next_attempt_retries() {
        let (mut range, _) = range(vec![
            Err(IdReservationError::new("store unreachable")),
            Ok(block(0, 5)),
        ]);
        assert!(take(&mut range).is_err());
        assert_eq!(take(&mut range).expect("id"), 1);
    }

    #[test]
    fn a_block_below_an_issued_id_is_refused() {
        let (mut range, _) = range(vec![Ok(block(0, 9))]);
        range.allocate_past(4).expect("seat");
        let err = take(&mut range).expect_err("block reaches below 4");
        assert!(err.to_string().contains("below issued id 4"), "{err}");
    }

    #[test]
    fn seating_past_u64_max_is_a_controlled_error() {
        let mut range = IdRange::new();
        assert_eq!(range.allocate_past(u64::MAX), Err(IdSpaceExhausted));
        assert_eq!(range.allocate_past(u64::MAX - 1), Ok(()));
        assert!(take(&mut range).is_err(), "u64::MAX is never issued");
    }

    #[test]
    fn a_block_ending_at_u64_max_is_refused() {
        let (mut range, _) = range(vec![Ok(block(0, u64::MAX))]);
        assert!(take(&mut range).is_err());
    }
}
