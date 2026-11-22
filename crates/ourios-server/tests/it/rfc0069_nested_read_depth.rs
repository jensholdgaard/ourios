//! RFC 0069 — nested-value read depth, and an undecodable cell degrading
//! on its own. See `docs/rfcs/0069-nested-value-read-depth.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the RFC
//! is red; each names the slice that discharges it and moves to the test
//! home §6 gives it.

/// Scenario RFC0069.1 — every depth ingest accepts reads back.
#[test]
#[ignore = "RFC0069.1 stub — implemented with the read-depth slice"]
fn rfc0069_1_every_depth_ingest_accepts_reads_back() {
    todo!("RFC0069.1 — the #1136 depth matrix, over both transports, queries back whole");
}

/// Scenario RFC0069.2 — the read bound covers the ingest bound.
#[test]
#[ignore = "RFC0069.2 stub — implemented with the read-depth slice"]
fn rfc0069_2_the_read_bound_covers_the_ingest_bound() {
    todo!("RFC0069.2 — the deepest admitted value decodes under MAX_READ_DEPTH");
}

/// Scenario RFC0069.3 — an undecodable cell degrades alone.
#[test]
#[ignore = "RFC0069.3 stub — implemented with the per-cell degradation slice"]
fn rfc0069_3_an_undecodable_cell_degrades_alone() {
    todo!(
        "RFC0069.3 — every row returns: a bad body verbatim, or retained_lossy when not UTF-8; \
         each bad attribute cell as ourios.attribute.undecodable; one WARN per bad cell"
    );
}

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

/// Scenario RFC0069.6 — the fallback is masked for a masked reader.
#[test]
#[ignore = "RFC0069.6 stub — implemented with the per-cell degradation slice"]
fn rfc0069_6_the_fallback_is_masked_for_a_masked_reader() {
    todo!("RFC0069.6 — a masked reader gets the reserved attribute masked whole");
}

/// Scenario RFC0069.7 — a filter over an undecodable cell does not fail the query.
#[test]
#[ignore = "RFC0069.7 stub — implemented with the predicate-guard slice"]
fn rfc0069_7_a_filter_over_an_undecodable_cell_does_not_fail_the_query() {
    todo!(
        "RFC0069.7 — neither a comparison nor its negation matches the bad cell, the promoted \
         JSON fallback included; the query does not fail"
    );
}
