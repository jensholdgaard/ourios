//! RFC0059.12 — Snapshots written before a root's first seat are never restored.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0059.12 — Snapshots written before a root's first seat are never restored.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.12 stub — implemented in the startup seat slice of #898's implementation"]
fn rfc0059_12_a_markerless_root_over_a_seated_store_discards_its_snapshots() {
    todo!(
        "RFC0059.12 — a markerless root over a seated store discards every \
         artefact as predates_high_water without restoring it, an unusable \
         marker fails startup closed, and the loser of a bootstrap race \
         fails startup"
    );
}
