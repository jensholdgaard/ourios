//! RFC0059.8 — Concurrent reservers on one store get disjoint blocks.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0059.8 — Concurrent reservers on one store get disjoint blocks.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.8 stub — implemented in the reservation slice of #898's implementation"]
fn rfc0059_8_concurrent_reservers_get_disjoint_blocks() {
    todo!(
        "RFC0059.8 — two reservers on an If-Match store reserving \
         concurrently get pairwise disjoint blocks, and the high-water \
         reads the highest block end"
    );
}
