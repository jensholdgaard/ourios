//! RFC0065.33's query arm. Its compaction and erasure arms run in
//! `ourios-parquet`'s `compaction::tests::positions`; this one needs the
//! querier to count a repeated position once (§6.6, §6.7 step 4), which it
//! does not yet: before compaction it counts both flushers' copies.

/// Scenario RFC0065.33 — the query arm. A query over the partition returns
/// the same rows before and after the compaction, and after an erasure
/// that matches at least one row, exactly those rows less the ones it
/// matched.
#[test]
#[ignore = "RFC0065.33 stub — implemented with the querier's dedup by position (§6.7 step 4)"]
fn rfc0065_33_a_query_returns_the_same_rows_across_compaction_and_erasure() {
    todo!(
        "RFC0065.33 — the same rows before and after compaction, and after an erasure \
         exactly those less the ones it matched"
    );
}
