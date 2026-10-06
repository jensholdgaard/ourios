//! RFC0059.17 — No documented policy can delete the high-water.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0059.17 — No documented policy can delete the high-water.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.17 stub — implemented in the chart slice of #898's implementation"]
fn rfc0059_17_no_documented_policy_can_delete_the_high_water() {
    todo!(
        "RFC0059.17 — every JSON policy in the chart README keeps \
         s3:DeleteObject off miner/template_ids.v1.json, wildcards \
         included, with and without a storage.s3.prefix, and carries an \
         explicit Deny"
    );
}
