//! RFC 0025 §5 — the storage-owned scenarios: absent-body round-trip
//! (`.1`) and old-file parity (`.2`). Rendering (`.3`) lives in
//! `crates/ourios-querier/tests/rfc0025_rendering.rs`; the sink
//! quarantine (`.4`/`.5`) in
//! `crates/ourios-ingester/tests/rfc0025_quarantine.rs`.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.

/// Scenario RFC0025.1 — absent bodies round-trip.
/// See `docs/rfcs/0025-absent-body-representation.md` §5.
#[test]
#[ignore = "RFC0025.1 stub — implemented in the schema green slice"]
fn rfc0025_1_absent_bodies_round_trip() {
    todo!(
        "RFC0025.1 — a BodyKind::Absent record writes under body_kind \
         ordinal 2 with a NULL body cell and reads back with every \
         RFC 0005 §3.2 column intact; the RFC 0024 P1 pinned-rejection \
         arm for absent bodies flips to a round-trip assertion"
    );
}

/// Scenario RFC0025.2 — old files unaffected.
/// See `docs/rfcs/0025-absent-body-representation.md` §5.
#[test]
#[ignore = "RFC0025.2 stub — implemented in the schema green slice"]
fn rfc0025_2_old_files_unaffected() {
    todo!(
        "RFC0025.2 — a pre-amendment committed fixture reads identically \
         under the amended reader (the RFC 0021 §6 committed-fixture \
         parity discipline)"
    );
}
