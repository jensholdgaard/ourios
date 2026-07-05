//! RFC 0025 §5 — the query-owned scenario: rendering distinguishes
//! absent from empty (`.3`). See
//! `crates/ourios-parquet/tests/rfc0025_absent_body.rs` for the
//! scenario placement map.
//!
//! The stub is `#[ignore]`d so the default run stays green while the
//! RFC is red.

/// Scenario RFC0025.3 — rendering distinguishes absent from empty.
/// See `docs/rfcs/0025-absent-body-representation.md` §5.
#[test]
#[ignore = "RFC0025.3 stub — implemented in the read-path green slice"]
fn rfc0025_3_rendering_distinguishes_absent_from_empty() {
    todo!(
        "RFC0025.3 — a row with body = \"\" renders with an empty string \
         body; a body_kind = Absent row renders with no body field at all"
    );
}
