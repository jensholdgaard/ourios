//! RFC 0069 — nested-value read depth, and an undecodable cell degrading
//! on its own. See `docs/rfcs/0069-nested-value-read-depth.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the RFC
//! is red; each names the slice that discharges it and moves to the test
//! home §6 gives it. RFC0069.1 is in `rfc0069_1_depth_matrix.rs`, and
//! RFC0069.2 in `ourios-core`'s `otlp/canonical/depth.rs`. RFC0069.3 is in
//! `ourios-parquet`'s `tests/it/rfc0069_3_undecodable_cells.rs` (the reader)
//! and `ourios-querier`'s `tests/it/rfc0069_3_degraded_query.rs` (the
//! query), with its event live-checked by `tests/rfc0069_reader_events.rs`;
//! RFC0069.6 is in `ourios-querier`'s `visibility.rs`, and RFC0069.7 in
//! its `tests/it/rfc0069_7_predicate_guard.rs`.

/// Scenario RFC0069.4 — compaction carries an undecodable cell byte for byte.
#[test]
#[ignore = "RFC0069.4 stub — implemented with the compaction raw-form slice"]
fn rfc0069_4_compaction_carries_an_undecodable_cell_byte_for_byte() {
    todo!(
        "RFC0069.4 — raw cells unchanged through a spill; promoted columns re-projected from \
         what can be read, a re-typed or newly promoted key NULL on an unreadable source cell"
    );
}

/// Scenario RFC0069.5 — erasure stays pending past an undecodable identity.
#[test]
#[ignore = "RFC0069.5 stub — implemented with the compaction raw-form slice"]
fn rfc0069_5_erasure_stays_pending_past_an_undecodable_identity() {
    todo!("RFC0069.5 — the row is carried forward and the erasure's marker does not advance");
}
