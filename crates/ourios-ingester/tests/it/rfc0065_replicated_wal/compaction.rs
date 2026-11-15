//! RFC0065.33: compaction and the erasure rewrite keep the ingest position
//! columns, keep one row per `(tenant, ingest position)`, and keep every
//! row of a file without the columns (§6.6).

/// Scenario RFC0065.33 — compaction keeps one row per position and every
/// row without one (B13, H8). Given two flushers' files with the same
/// positions, a later repeat and a local-backend file, when the partition
/// is compacted and then erased from, then each position is kept once,
/// the local rows are kept with null positions, and the repeats are
/// counted per tenant.
#[test]
#[ignore = "RFC0065.33 stub — implemented with compaction by position (#1245)"]
fn rfc0065_33_compaction_keeps_one_row_per_position_and_every_row_without_one() {
    todo!(
        "RFC0065.33 — the compacted file carries the columns, each position once and \
         the local rows with null positions, the repeats counted on \
         ourios.compaction.row.deduplicated; erasure keeps every surviving position"
    );
}

/// Scenario RFC0065.33 — the unpositioned arm. A partition whose files all
/// lack the columns compacts to a file without them, with nothing counted.
#[test]
#[ignore = "RFC0065.33 stub — implemented with compaction by position (#1245)"]
fn rfc0065_33_a_partition_without_positions_compacts_without_them() {
    todo!("RFC0065.33 — no input has the columns, so the output has none and nothing is counted");
}
