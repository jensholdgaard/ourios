//! RFC0059.10 — Ids increase per allocator across restarts.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0059.10 — Ids increase per allocator across restarts.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.10 stub — implemented in the reservation slice of #898's implementation"]
fn rfc0059_10_ids_increase_per_allocator_across_restarts() {
    todo!(
        "RFC0059.10 — an allocator's ids, listed in issue order across \
         restarts with no re-bootstrap between, strictly increase, and no \
         id above i64::MAX is issued"
    );
}
