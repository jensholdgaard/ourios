//! RFC 0017 — read-time template registry & query-row rendering, the
//! miner-emit arm of scenario `.1`.
//!
//! **Status: `red`.** Failing stub driving the `green` implementation: it
//! encodes the miner half of RFC 0017 §5 scenario .1 (a new leaf's allocation
//! emits a `template_created` audit event carrying its initial tokens, on the
//! WAL-before-ack path) and currently `todo!()`s. It is `#[ignore]`d so the
//! default `cargo test` (and CI) stays green until the `green` slice lands the
//! emit; `green` replaces the body with the real assertion and removes the
//! `#[ignore]`.
//!
//! See `docs/rfcs/0017-template-registry-query-rendering.md` §3.1 / §5 / §6.

/// Scenario RFC0017.1 (miner-emit arm) — allocating a new leaf emits a
/// `template_created` audit event carrying `(template_id, new_version = 1,
/// new_template = the initial tokens)`, with `old_template`/`old_version` left
/// `NULL`, on the same WAL-before-ack path as the existing template events.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[test]
#[ignore = "RFC0017.1 — red until the miner emits template_created on leaf creation (green)"]
fn rfc0017_1_new_leaf_emits_template_created() {
    todo!(
        "RFC0017.1: first leaf allocation emits template_created with new_version=1 + initial tokens"
    )
}
