//! RFC0059.3 — Ids are reserved before they are used; a crash only skips.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0059.3 — Ids are reserved before they are used; a crash only skips.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.3 stub — implemented in the reservation slice of #898's implementation"]
fn rfc0059_3_the_object_reads_every_id_before_it_is_allocated() {
    todo!(
        "RFC0059.3 — the high-water already reads at least every id when it \
         is allocated, and after a SIGKILL that follows a reservation but \
         precedes any use of its block, the restarted receiver's first \
         allocation is above that block"
    );
}
