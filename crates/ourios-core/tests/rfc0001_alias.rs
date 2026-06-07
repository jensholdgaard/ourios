//! RFC 0001 — alias-index write path acceptance criteria (RFC0001.12–.16).
//!
//! Red gate (`specified → red`, per the 2026-06-07 alias-write-path
//! amendment to RFC 0001 §6.7): `#[ignore]`'d `unimplemented!()` stubs
//! until the operator-driven alias model lands — the `alias_asserted` /
//! `alias_retracted` audit events on the §6.4 stream, the per-tenant
//! alias-map projection folded from that log, and the operator assertion
//! API. Per `docs/verification.md` §3 the scenarios become ignored stubs
//! first, implementations second; each carries the §2.3 doc-comment form
//! so the spec↔test mapping is greppable.
//!
//! Placement rationale: the alias types live in `ourios-core` alongside
//! [`ourios_core::audit`] (the alias events are new `AuditPayload`
//! variants) and the per-tenant alias map/store is consumed by
//! `ourios-miner` (emission) and `ourios-querier` (`resolves_to` reads),
//! so the shared crate is the natural home. The querier-side
//! `resolves_to` *DSL surface* is RFC0002.9's gate
//! (`crates/ourios-querier/tests/rfc0002_dsl.rs`); these stubs own the
//! write path and the map's expansion semantics that RFC0002.9 compiles
//! against.

/// Scenario RFC0001.12 — Alias assertion is durably recorded and appears in the per-tenant map.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[ignore = "RFC 0001 alias write path pending (RFC0001.12)"]
#[test]
fn rfc0001_12_alias_assertion_is_durably_recorded_and_in_the_map() {
    unimplemented!(
        "RFC0001.12 — asserting B is an alias of A (A < B) under tenant T emits a \
         durable `alias_asserted` audit event under the §3.4 WAL-before-ack barrier \
         (naming tenant_id = T, representative_id = A, member_ids = [B], actor, \
         timestamp), and after the projection rebuilds T's alias map holds the class \
         {{A, B}} with derived canonical = A (the smallest member)."
    );
}

/// Scenario RFC0001.13 — `resolves_to(rep)` returns all members and excludes non-members.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[ignore = "RFC 0001 alias write path pending (RFC0001.13)"]
#[test]
fn rfc0001_13_resolves_to_expands_to_the_whole_set() {
    unimplemented!(
        "RFC0001.13 — for tenant T whose map records {{A, B}} and an unrelated leaf C, \
         `template_id.resolves_to(A)` expands to {{A, B}}; `resolves_to(B)` expands to \
         the same {{A, B}} (member↔representative symmetry — expansion is by the set, \
         not the assertion direction); `resolves_to(C)` expands to exactly {{C}}."
    );
}

/// Scenario RFC0001.14 — Cross-tenant isolation: an alias in tenant A never affects tenant B.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[ignore = "RFC 0001 alias write path pending (RFC0001.14)"]
#[test]
fn rfc0001_14_alias_sets_are_per_tenant_isolated() {
    unimplemented!(
        "RFC0001.14 `[§3.7]` — tenant T1's map records {{A, B}} while T2 has the same \
         template_ids A and B but no assertion; `resolves_to(A)` expands to {{A, B}} for \
         T1 and to exactly {{A}} for T2 — an assertion in one tenant is invisible to \
         every other."
    );
}

/// Scenario RFC0001.15 — Retraction removes any member, including the canonical, and is itself audited.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[ignore = "RFC 0001 alias write path pending (RFC0001.15)"]
#[test]
fn rfc0001_15_retraction_removes_a_member_and_redrives_canonical() {
    unimplemented!(
        "RFC0001.15 — retracting member A (the canonical / smallest) from class {{A, B}} \
         under tenant T emits a durable `alias_retracted` audit event (same \
         WAL-before-ack barrier and field shape as RFC0001.12: representative_id = A \
         as the operator's anchor, empty member_ids, actor); after the projection \
         rebuilds the class is {{B}} — a single member, no longer an alias set — so \
         `resolves_to(A)` expands to {{A}} and `resolves_to(B)` expands to {{B}} \
         (representative-independent retraction; canonical re-derived as min of the \
         remainder)."
    );
}

/// Scenario RFC0001.16 — A non-aliased id resolves to itself.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[ignore = "RFC 0001 alias write path pending (RFC0001.16)"]
#[test]
fn rfc0001_16_non_aliased_id_resolves_to_itself() {
    unimplemented!(
        "RFC0001.16 — for tenant T with leaf Z and no assertion naming Z, \
         `template_id.resolves_to(Z)` expands to exactly {{Z}} — identical to the \
         base-member behaviour and to bare `template_id = Z` (RFC0001.6)."
    );
}
