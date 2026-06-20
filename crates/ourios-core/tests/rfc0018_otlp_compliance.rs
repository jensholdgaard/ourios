//! RFC 0018 — OTLP log-spec compliance acceptance scenario (§5), the
//! canonical-encoding arm (`.5`).
//!
//! **Status: `red`.** Failing stub driving the `green` implementation: it
//! encodes RFC 0018 §5 scenario .5 and currently `todo!()`s. It is `#[ignore]`d
//! so the default `cargo test` (and CI) stays green while non-finite-double
//! handling is fixed — `green` replaces the body with a round-trip assertion
//! (and overturns the existing "encodes to null" test) and removes the
//! `#[ignore]`.
//!
//! See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5/§6.

/// Scenario RFC0018.5 — non-finite doubles round-trip through canonical JSON:
/// an `AnyValue` containing `NaN`, `Infinity`, and `-Infinity`, canonical-encoded
/// (proto3-JSON string forms "NaN"/"Infinity"/"-Infinity") and decoded, equals
/// the original — no `null` collapse.
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[test]
#[ignore = "RFC0018.5 — red until non-finite doubles use the proto3-JSON string forms (green)"]
fn rfc0018_5_non_finite_doubles_round_trip() {
    todo!("RFC0018.5: NaN/Infinity/-Infinity round-trip via proto3-JSON string forms")
}
