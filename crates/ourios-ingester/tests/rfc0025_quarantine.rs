//! RFC 0025 §5 — the sink-owned scenarios: the permanent-encode-error
//! quarantine (`.4`) and its telemetry (`.5`). See
//! `crates/ourios-parquet/tests/rfc0025_absent_body.rs` for the
//! scenario placement map.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.

/// Scenario RFC0025.4 — the sink no longer wedges.
/// See `docs/rfcs/0025-absent-body-representation.md` §5.
#[test]
#[ignore = "RFC0025.4 stub — implemented in the quarantine green slice"]
fn rfc0025_4_sink_quarantines_instead_of_wedging() {
    todo!(
        "RFC0025.4 — a permanently-rejected record (timestamp overflow) in \
         a partition buffer is quarantined to the audit stream with a \
         record_quarantined event; the healthy records persist; subsequent \
         flushes of the partition succeed"
    );
}

/// Scenario RFC0025.5 — quarantine telemetry.
/// See `docs/rfcs/0025-absent-body-representation.md` §5.
#[test]
#[ignore = "RFC0025.5 stub — implemented in the quarantine green slice"]
fn rfc0025_5_quarantine_telemetry() {
    todo!(
        "RFC0025.5 — a quarantine increments the existing flush-error \
         counter with error.type set to the BatchError variant; no new \
         metric name is introduced"
    );
}
