//! RFC0059.14 — A seated root that finds the high-water gone fails closed.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0059.14 — A seated root that finds the high-water gone fails closed.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.14 stub — implemented in the startup seat slice of #898's implementation"]
fn rfc0059_14_a_seated_root_never_bootstraps_a_deleted_high_water() {
    todo!(
        "RFC0059.14 — a seated root whose high-water was deleted fails \
         startup naming the object, and neither bootstraps nor writes"
    );
}

/// Scenario RFC0059.15 — A denied store call names the missing permission.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.15 stub — implemented in the startup seat slice of #898's implementation"]
fn rfc0059_15_a_denied_read_names_the_missing_permission() {
    todo!(
        "RFC0059.15 — a store that refuses the receiver's calls fails \
         startup with an error naming the missing S3 action"
    );
}
