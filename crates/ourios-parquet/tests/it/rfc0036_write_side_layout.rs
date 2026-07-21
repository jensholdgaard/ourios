//! RFC 0036 §5 — write-side layout, the four compaction-side scenarios.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! Placement note: RFC0036.1/.3/.4/.5 live here because the machinery
//! they gate — the §3.2 sort-run merge, the §3.3 compacted row-group
//! threshold, and the §3.4 `sorting_columns` declaration — is
//! `compaction.rs`/`writer.rs` code (RFC 0036 §6). RFC0036.2's
//! in-repo slice (the synthetic-hour scanned-count bound via the
//! RFC 0016 counters) lives with the querier counter assertions in
//! `ourios-querier/tests/it/rfc0036_window_materialization.rs`.

/// Scenario RFC0036.1 — compacted layout (clustering + sizing +
/// declaration). See `docs/rfcs/0036-write-side-layout.md` §5.
#[test]
#[ignore = "RFC0036.1 stub — implemented in the sorted-compaction green slice (run formation + merge + writer properties)"]
fn rfc0036_1_compacted_layout() {
    todo!(
        "RFC0036.1 — partition with ≥ 2 input files whose rows span \
         multiple promoted service.name values and interleaved times, \
         compacted: footer inspection of the consolidated file shows \
         row groups rotated at the configured compacted threshold \
         (each uncompressed size ≤ threshold + one sub-batch's bounded \
         overshoot), sorting_columns declared as the §3.1 keys 1–2 on \
         every row group, and per-row-group service.name min/max \
         spanning at most a boundary pair of services; decoding yields \
         rows in §3.1 key order with the row multiset equal to the \
         inputs' union — plus the §6 merge proptest: arbitrary \
         service/time/duplicate-key mixes ⇒ output multiset equals \
         input union, §3.1-sorted, equal-key rows in tie-break order"
    );
}

/// Scenario RFC0036.3 — compaction properties preserved (D2 / D3 /
/// memory). See `docs/rfcs/0036-write-side-layout.md` §5.
#[test]
#[ignore = "RFC0036.3 stub — implemented in the compaction-properties green slice (D2 band + memory bound)"]
fn rfc0036_3_compaction_properties_preserved() {
    todo!(
        "RFC0036.3 — §9.7-scale compaction workload (band-scale \
         partition, tens of input files) run through the sorted \
         compaction: D3 holds unchanged (one output file per \
         partition, inside the 256 MiB – 2 GiB band, < 5% of live \
         files below 128 MiB), D2 throughput stays within the band \
         set from a first measurement and still ≫ the per-partition \
         seal rate; a memory-bound test shows peak decoded-row \
         residency of the order of one input file (phase 1) and \
         F × batch (phase 2) — never whole-partition residency"
    );
}

/// Scenario RFC0036.4 — determinism (the harness's contract).
/// See `docs/rfcs/0036-write-side-layout.md` §5.
#[test]
#[ignore = "RFC0036.4 stub — implemented in the determinism green slice (rebuild differential)"]
fn rfc0036_4_rebuild_byte_identity() {
    todo!(
        "RFC0036.4 — the same set of input files (same bytes, same \
         names) compacted twice, the second run with the store fake \
         returning listings in a shuffled order: the two consolidated \
         outputs are byte-identical (a file hash is the correct \
         assertion here — byte identity is exactly the property \
         claimed), preserving the §9.13 determinism property the \
         comparative ledger depends on"
    );
}

/// Scenario RFC0036.5 — no read-path or schema regression.
/// See `docs/rfcs/0036-write-side-layout.md` §5.
#[test]
#[ignore = "RFC0036.5 stub — implemented in the compat green slice (pre-RFC fixture reads + B1/B2 + frozen-gate rerun)"]
fn rfc0036_5_no_read_path_or_schema_regression() {
    todo!(
        "RFC0036.5 — stores built before and after the change: B1/B2 \
         and the frozen RFC 0031 comparative gates run against the \
         post-change store and every frozen gate still passes, with \
         the L1/L3/L4 pairs not degraded beyond the documented \
         Loki-wobble band and query results identical row-sets; a \
         pre-RFC-0036 fixture file (no sorting_columns, 128 MiB row \
         groups) reads without error or special-casing — no migration \
         exists because none is needed (CLAUDE.md §3.5)"
    );
}
