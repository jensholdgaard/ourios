//! RFC0059.10 — Ids increase per allocator across restarts.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0059.10 — Ids increase per allocator across restarts.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.10 stub — implemented in the reservation slice of #898's implementation"]
fn rfc0059_10_an_unaligned_high_water_still_issues_i64_max_last() {
    todo!(
        "RFC0059.10 — from a high-water whose distance to i64::MAX is not a \
         multiple of BLOCK, a shortened final block is reserved, every id \
         through i64::MAX is issued, and only the next fresh mint fails"
    );
}
