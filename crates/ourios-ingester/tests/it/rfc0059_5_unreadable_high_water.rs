//! RFC0059.5 — An unreadable high-water fails startup closed.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0059.5 — An unreadable high-water fails startup closed.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.5 stub — implemented in the startup seat slice of #898's implementation"]
fn rfc0059_5_an_unreadable_high_water_fails_startup_closed() {
    todo!(
        "RFC0059.5 — a high-water that does not parse, lacks \
         reserved_through, holds a non-u64 value or one above i64::MAX \
         fails startup before any listener opens, naming the object, and is \
         not rewritten"
    );
}

/// Scenario RFC0059.11 — Any later-version high-water fails startup closed, even beside a v1.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.11 stub — implemented in the startup seat slice of #898's implementation"]
fn rfc0059_11_a_later_version_fails_startup_closed_even_beside_v1() {
    todo!(
        "RFC0059.11 — a later template_ids.v<k>.json, alone or beside a v1, \
         fails startup closed and nothing is rewritten"
    );
}
