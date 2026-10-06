//! RFC0059.18 — A high-water deleted while live is never re-created.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0059.18 — A high-water deleted while live is never re-created.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.18 stub — implemented in the reservation slice of #898's implementation"]
fn rfc0059_18_a_high_water_deleted_while_live_is_never_recreated() {
    todo!(
        "RFC0059.18 — after the object is deleted under a running receiver, \
         the held ids are issued once, no reservation re-creates it, fresh \
         mints fail once the held blocks are spent, a reappeared copy is \
         not reserved from, a restart over a stale copy fails as a \
         rollback, and the documented recovery reseats above every \
         published id"
    );
}
