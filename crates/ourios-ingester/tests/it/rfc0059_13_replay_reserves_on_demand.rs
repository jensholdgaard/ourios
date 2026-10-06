//! RFC0059.13 — Replay past the ready blocks reserves on demand.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0059.13 — Replay past the ready blocks reserves on demand.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.13 stub — implemented in the reservation slice of #898's implementation"]
fn rfc0059_13_replay_past_the_ready_blocks_reserves_on_demand() {
    todo!(
        "RFC0059.13 — replay that mints more templates than the three \
         blocks startup reserves never fails id_reservation_failed against \
         a healthy store, and once replay ends a drained reserver fails \
         instead of calling the store"
    );
}
