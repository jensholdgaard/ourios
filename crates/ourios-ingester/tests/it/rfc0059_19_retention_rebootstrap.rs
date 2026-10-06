//! RFC0059.19 — Retention or erasure followed by re-bootstrap never reissues a bound id.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0059.19 — Retention or erasure followed by re-bootstrap never reissues a bound id.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.19 stub — implemented in the recovery seat slice of #898's implementation"]
fn rfc0059_19_a_rebootstrap_after_retention_never_reissues_a_bound_id() {
    todo!(
        "RFC0059.19 — after retention removes one tenant's data and audit \
         and the documented re-bootstrap runs, the floor covers every \
         still-bound id and no new id equals one"
    );
}
