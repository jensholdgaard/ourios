//! RFC0059.16 — Bootstrapping over existing data needs authorisation.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0059.16 — Bootstrapping over existing data needs authorisation.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.16 stub — implemented in the bootstrap slice of #898's implementation"]
fn rfc0059_16_bootstrapping_over_data_needs_authorisation() {
    todo!(
        "RFC0059.16 — a markerless root bootstraps an empty store \
         unauthorised, fails closed over a store holding data unless \
         OURIOS_TEMPLATE_IDS_ALLOW_BOOTSTRAP is set, and the setting is off \
         by default"
    );
}
