//! RFC 0017 — read-time template registry & query-row rendering, the
//! registry-derivation scenarios (`.2`, `.5`).
//!
//! **Status: `red`.** Failing stubs driving the `green` implementation: they
//! encode RFC 0017 §5 scenarios .2 (the registry derives completely from the
//! audit stream, including version 1) and .5 (rows render against their own
//! template version) and currently `todo!()`. They are `#[ignore]`d so the
//! default `cargo test` (and CI) stays green until `derive_template_registry`
//! lands; `green` replaces the bodies with the real assertions and removes the
//! `#[ignore]`s.
//!
//! See `docs/rfcs/0017-template-registry-query-rendering.md` §3.2 / §3.5 / §5 / §6.

/// Scenario RFC0017.2 — `derive_template_registry` folds a tenant audit stream
/// of `template_created` / `template_widened` / `template_type_expanded` events
/// (deterministic `(timestamp, path, row)` order) into a registry containing
/// the tokens for **every** `(template_id, version)` the stream describes,
/// including version 1, with later versions not clobbering earlier ones.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[test]
#[ignore = "RFC0017.2 — red until derive_template_registry folds the audit stream completely (green)"]
fn rfc0017_2_registry_derives_completely_including_v1() {
    todo!(
        "RFC0017.2: registry holds tokens for every (template_id, version), v1 included, later versions don't clobber"
    )
}

/// Scenario RFC0017.5 — a row carrying `template_version = N` renders against
/// the N-version tokens (the event whose `new_version = N`), not the latest:
/// a line ingested before a widening reconstructs as it was then.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[test]
#[ignore = "RFC0017.5 — red until rendering keys the registry by (template_id, version) (green)"]
fn rfc0017_5_rows_render_against_their_own_version() {
    todo!(
        "RFC0017.5: a version-1 row renders against version-1 tokens, not the widened version-2 tokens"
    )
}
