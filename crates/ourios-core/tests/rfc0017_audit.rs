//! RFC 0017 — read-time template registry & query-row rendering, the
//! audit-schema arm of scenario `.1`.
//!
//! **Status: `red`.** Failing stub driving the `green` implementation: it
//! encodes the audit-contract half of RFC 0017 §5 scenario .1 (the new
//! `template_created` `event_kind`/`event_type` is an append-only addition —
//! ordinal `6`, no existing ordinal renumbered) and currently `todo!()`s. It
//! is `#[ignore]`d so the default `cargo test` (and CI) stays green until the
//! `green` slice lands `TemplateChange::Created`; `green` replaces the body
//! with the real assertions and removes the `#[ignore]`.
//!
//! See `docs/rfcs/0017-template-registry-query-rendering.md` §3.1 / §5 / §6.

/// Scenario RFC0017.1 (audit-schema arm) — the `template_created` event is an
/// append-only audit addition: a new `event_kind` ordinal `6` paired with the
/// `event_type` string `template_created`, with every existing ordinal (`0`–`5`)
/// left unchanged (RFC 0005 §3.7 append-only rule).
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[test]
#[ignore = "RFC0017.1 — red until TemplateChange::Created + event_kind 6 land (green)"]
fn rfc0017_1_template_created_is_append_only_audit_addition() {
    todo!(
        "RFC0017.1: template_created = event_kind ordinal 6 / event_type \"template_created\", existing ordinals unchanged"
    )
}
