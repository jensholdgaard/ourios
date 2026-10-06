//! RFC0059.7 — The bootstrap reads footers in bounded memory.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0059.7 — The bootstrap reads footers in bounded memory.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
#[ignore = "RFC0059.7 stub — implemented in the bootstrap slice of #898's implementation"]
fn rfc0059_7_the_bootstrap_reads_footers_in_bounded_memory() {
    todo!(
        "RFC0059.7 — the bootstrap scan decodes no row of a file whose id \
         columns have usable statistics, its peak heap stays below an \
         eighth of the history's body and template bytes, and grows less \
         than 1.5x when the history grows 4x by adding directories"
    );
}
