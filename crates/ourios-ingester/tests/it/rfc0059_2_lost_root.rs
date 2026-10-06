//! RFC0059.2 — A replaced local root never re-issues a published id.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0059.2 — A replaced local root never re-issues a published id.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.2 stub — implemented in the recovery seat slice of #898's implementation"]
fn rfc0059_2_a_replaced_root_never_reissues_a_published_id() {
    todo!(
        "RFC0059.2 — a receiver whose local root (snapshots and WAL) is \
         replaced seats above the store's high-water, and no id it mints \
         equals a published one"
    );
}
