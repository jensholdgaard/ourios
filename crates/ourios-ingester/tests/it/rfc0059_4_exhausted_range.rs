//! RFC0059.4 — An exhausted range fails fresh mints without blocking ingest.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0059.4 — An exhausted range fails fresh mints without blocking ingest.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.4 stub — implemented in the reservation slice of #898's implementation"]
fn rfc0059_4_an_exhausted_range_fails_fresh_mints_and_keeps_matches_flowing() {
    todo!(
        "RFC0059.4 — with the current block and both ready blocks used up \
         and the reserver down, fresh templates fail parse as \
         id_reservation_failed with their body kept, known ones attach, no \
         store call runs under the miner lock, and allocation resumes once \
         the reserver recovers"
    );
}
