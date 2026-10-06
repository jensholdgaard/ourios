//! RFC0059.1 — A discarded snapshot never re-issues a published id.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0059.1 — A discarded snapshot never re-issues a published id.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.1 stub — implemented in the recovery seat slice of #898's implementation"]
fn rfc0059_1_no_new_id_equals_a_published_one_for_each_discard_class() {
    todo!(
        "RFC0059.1 — for every discard class, over string and structured \
         templates, no id minted after the restart equals a published one \
         unless a kept snapshot restored it under that id; an old shape \
         first seen in a reclaimed frame re-mints above the high-water"
    );
}
