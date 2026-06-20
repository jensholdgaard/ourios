//! RFC 0018 — OTLP log-spec compliance acceptance scenario (§5), the DSL arm
//! (`.4`).
//!
//! **Status: `red`.** Failing stub driving the `green` implementation: it
//! encodes RFC 0018 §5 scenario .4 and currently `todo!()`s. It is `#[ignore]`d
//! so the default `cargo test` (and CI) stays green while `event_name` is added
//! as a first-class DSL field — `green` replaces the body with a real assertion
//! and removes the `#[ignore]`.
//!
//! See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5/§6.

/// Scenario RFC0018.4 — `event_name` is filterable in the DSL: a query filtering
/// on `event_name` compiles to the `event_name` column and returns exactly the
/// matching rows, with no DataFusion/SQL surface leaking to the user (H6).
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[test]
#[ignore = "RFC0018.4 — red until event_name becomes a first-class DSL field (green)"]
fn rfc0018_4_event_name_is_filterable() {
    todo!("RFC0018.4: event_name DSL filter compiles + matches; no engine leak (H6)")
}
