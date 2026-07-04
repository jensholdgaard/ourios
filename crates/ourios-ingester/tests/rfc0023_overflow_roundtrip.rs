//! RFC 0023 §5 — RFC0023.2: ceiling-overflow lines stay stored and
//! searchable. The miner-level scenarios (`.1/.3/.4/.5/.6`) live in
//! `crates/ourios-miner/tests/rfc0023_bounded_memory.rs`; this one
//! crosses the ingest path (miner overflow → record sink → Parquet →
//! body read-back), which needs both crates.

/// Scenario RFC0023.2 — overflow lines stay stored and searchable.
/// See `docs/rfcs/0023-bounded-template-memory.md` §5.
#[test]
#[ignore = "RFC0023.2 stub — implemented in the miner-bounds green slice (as its ingest-path integration arm)"]
fn rfc0023_2_overflow_bodies_round_trip_bit_identically() {
    todo!(
        "RFC0023.2 — ceiling-overflow lines written through the record sink \
         round-trip bit-identically through the Parquet body column"
    );
}
