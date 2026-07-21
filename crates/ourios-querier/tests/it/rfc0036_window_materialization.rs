//! RFC 0036 §5 — window-query materialization (RFC0036.2), the
//! in-repo slice.
//!
//! The stub is `#[ignore]`d so the default run stays green while the
//! RFC is red; it names the green slice that discharges it.
//!
//! Placement note: the RFC 0016 scanned/pruned row-group counts this
//! scenario bounds are asserted from this harness (`execution.rs`), so
//! the synthetic-hour CI slice co-locates with them (RFC 0036 §6). The
//! full comparative arm — the L6-shape pair on the v8 corpus through
//! the RFC 0031 dispatch and the before/after §9 bytes diagnostic — is
//! a harness/measurement concern in `ourios-bench`, not a CI stub,
//! matching how RFC 0033 handled its `.6` comparative arm. The other
//! four scenarios live with the compaction code in
//! `ourios-parquet/tests/it/rfc0036_write_side_layout.rs`.

/// Scenario RFC0036.2 — window-query materialization (the point).
/// See `docs/rfcs/0036-write-side-layout.md` §5.
#[test]
#[ignore = "RFC0036.2 stub — implemented in the pruning green slice (synthetic-hour scanned-count bound; the comparative arm runs via the ourios-bench RFC 0031 harness)"]
fn rfc0036_2_window_materialization_bound() {
    todo!(
        "RFC0036.2 — compacted store built from a synthetic v8-shape \
         hour (many services, promoted service.name); the L6-shape \
         query (one service, k-row time window) runs: the RFC 0016 \
         scanned/pruned counts show row groups scanned \
         ≤ ceil(B_sw / T) + 2, where B_sw is the queried service's \
         bytes within the window (measurable from the compacted \
         file's footer) and T the configured row-group threshold — \
         the groups that hold the answer plus at most two boundary \
         groups, not the whole hour; the before/after materialization \
         bytes (total minus the registry acquisition) are measured on \
         the comparative corpus and published in the §9 series as the \
         storage-channel diagnostic (the gate is the scanned bound, \
         not a bytes ratio — §2.2)"
    );
}
