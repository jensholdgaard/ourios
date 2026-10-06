//! RFC0059.6 — The bootstrap floor is provable and written once.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0059.6 — The bootstrap floor is provable and written once.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.6 stub — implemented in the bootstrap slice of #898's implementation"]
fn rfc0059_6_the_floor_is_the_max_over_data_audit_and_snapshots() {
    todo!(
        "RFC0059.6 — the object is created at max(data_max, audit_max, \
         restored_max) with no margin, the bootstrapped event is logged at \
         most once per successful creation, a failed scan step writes \
         neither the object nor the marker, and the next start redoes the \
         scan"
    );
}
