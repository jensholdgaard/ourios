//! RFC 0001 — alias-index write path acceptance criteria (RFC0001.12–.16).
//!
//! Green gate (`red → green`, per the 2026-06-07 alias-write-path
//! amendment to RFC 0001 §6.7): the operator-driven alias model has
//! landed — the `alias_asserted` / `alias_retracted` audit events on the
//! §6.4 stream, the per-tenant [`AliasMap`] projection folded from that
//! log, and the operator assertion API. Each scenario keeps the §2.3
//! doc-comment form so the spec↔test mapping stays greppable.
//!
//! Placement rationale: the alias types live in `ourios-core` alongside
//! [`ourios_core::audit`] (the alias events are new `AuditPayload`
//! variants) and the per-tenant alias map/store is consumed by
//! `ourios-miner` (emission) and `ourios-querier` (`resolves_to` reads),
//! so the shared crate is the natural home. The querier-side
//! `resolves_to` *DSL surface* is RFC0002.9's gate
//! (`crates/ourios-querier/tests/rfc0002_dsl.rs`); these tests own the
//! write path and the map's expansion semantics that RFC0002.9 compiles
//! against.

use std::collections::BTreeSet;

use ourios_core::alias::{ActorId, AliasMap, Operator};
use ourios_core::audit::{AuditPayload, InMemoryAuditSink};
use ourios_core::tenant::TenantId;

fn actor() -> ActorId {
    ActorId::new("op-alice").expect("non-empty actor")
}

fn op(reason: &str) -> Operator {
    Operator::now(actor(), reason)
}

fn set(ids: impl IntoIterator<Item = u64>) -> BTreeSet<u64> {
    ids.into_iter().collect()
}

/// Scenario RFC0001.12 — Alias assertion is durably recorded and appears in the per-tenant map.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn rfc0001_12_alias_assertion_is_durably_recorded_and_in_the_map() {
    // Arrange — tenant T, an audit sink standing in for the §3.4
    // WAL-durable stream, and the per-tenant projection. A < B.
    let mut map = AliasMap::new();
    let mut sink = InMemoryAuditSink::new();
    let t = TenantId::new("T");
    let (a, b) = (10_u64, 20_u64);

    // Act — assert B is an alias of A under T.
    map.assert(
        &mut sink,
        &t,
        a,
        vec![b],
        op("deploy 2026-06 re-split login"),
    )
    .expect("assertion succeeds");

    // Assert — a durable `alias_asserted` event was emitted naming the
    // asserted set, and the map holds the class {A, B} with derived
    // canonical = A (the smallest member).
    let events = sink.drain();
    assert_eq!(events.len(), 1, "exactly one event durably recorded");
    assert_eq!(events[0].tenant_id, t);
    let AuditPayload::AliasAsserted {
        representative_id,
        ref member_ids,
        ..
    } = events[0].payload
    else {
        panic!(
            "expected an AliasAsserted payload, got {:?}",
            events[0].payload
        );
    };
    assert_eq!(representative_id, a);
    assert_eq!(member_ids, &vec![b]);

    assert_eq!(map.resolves(&t, a), set([a, b]));
    assert_eq!(map.canonical(&t, b), a, "canonical = min(members)");
}

/// Scenario RFC0001.13 — `resolves_to(rep)` returns all members and excludes non-members.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn rfc0001_13_resolves_to_expands_to_the_whole_set() {
    // Arrange — T's map records {A, B}; C is an unrelated leaf.
    let mut map = AliasMap::new();
    let mut sink = InMemoryAuditSink::new();
    let t = TenantId::new("T");
    let (a, b, c) = (10_u64, 20_u64, 30_u64);
    map.assert(&mut sink, &t, a, vec![b], op(""))
        .expect("assertion succeeds");

    // Act / Assert — expansion is by the set, not the assertion
    // direction: representative and member both expand to {A, B}; the
    // unrelated C expands to exactly {C}.
    assert_eq!(map.resolves(&t, a), set([a, b]));
    assert_eq!(
        map.resolves(&t, b),
        set([a, b]),
        "member↔representative symmetry"
    );
    assert_eq!(map.resolves(&t, c), set([c]));
}

/// Scenario RFC0001.14 — Cross-tenant isolation: an alias in tenant A never affects tenant B.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn rfc0001_14_alias_sets_are_per_tenant_isolated() {
    // Arrange `[§3.7]` — T1's map records {A, B}; T2 has the same ids
    // A and B but no assertion.
    let mut map = AliasMap::new();
    let mut sink = InMemoryAuditSink::new();
    let (t1, t2) = (TenantId::new("T1"), TenantId::new("T2"));
    let (a, b) = (10_u64, 20_u64);
    map.assert(&mut sink, &t1, a, vec![b], op(""))
        .expect("assertion succeeds");

    // Act / Assert — the assertion is visible only in T1.
    assert_eq!(map.resolves(&t1, a), set([a, b]));
    assert_eq!(
        map.resolves(&t2, a),
        set([a]),
        "invisible to every other tenant"
    );
    assert_eq!(map.resolves(&t2, b), set([b]));
}

/// Scenario RFC0001.15 — Retraction removes any member, including the canonical, and is itself audited.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn rfc0001_15_retraction_removes_a_member_and_rederives_canonical() {
    // Arrange — class {A, B} under T, A the canonical (smallest).
    let mut map = AliasMap::new();
    let mut sink = InMemoryAuditSink::new();
    let t = TenantId::new("T");
    let (a, b) = (10_u64, 20_u64);
    map.assert(&mut sink, &t, a, vec![b], op(""))
        .expect("assertion succeeds");
    let _ = sink.drain();

    // Act — retract member A (the canonical / smallest).
    map.retract(&mut sink, &t, a, op(""))
        .expect("retraction succeeds");

    // Assert — a durable `alias_retracted` event was emitted
    // (representative_id = A as the operator's anchor, empty member_ids);
    // the class drops to {B} (a single member, no longer an alias set),
    // so both A and B now resolve only to themselves
    // (representative-independent retraction; canonical re-derived as min
    // of the remainder).
    let events = sink.drain();
    assert_eq!(events.len(), 1, "the retraction is itself audited");
    let AuditPayload::AliasRetracted {
        representative_id,
        ref member_ids,
        ..
    } = events[0].payload
    else {
        panic!(
            "expected an AliasRetracted payload, got {:?}",
            events[0].payload
        );
    };
    assert_eq!(representative_id, a);
    assert!(member_ids.is_empty());

    assert_eq!(map.resolves(&t, a), set([a]));
    assert_eq!(
        map.resolves(&t, b),
        set([b]),
        "remainder is no longer an alias set"
    );
    assert_eq!(
        map.canonical(&t, b),
        b,
        "canonical re-derived over the remainder"
    );

    // And retraction is representative-independent: retracting the
    // *non-canonical* member from a fresh {A, B} drops it the same way.
    let mut map2 = AliasMap::new();
    let mut sink2 = InMemoryAuditSink::new();
    map2.assert(&mut sink2, &t, a, vec![b], op(""))
        .expect("assertion succeeds");
    map2.retract(&mut sink2, &t, b, op(""))
        .expect("retraction succeeds");
    assert_eq!(map2.resolves(&t, a), set([a]));
    assert_eq!(map2.resolves(&t, b), set([b]));
}

/// Scenario RFC0001.16 — A non-aliased id resolves to itself.
/// See `docs/rfcs/0001-template-miner.md` §5.
#[test]
fn rfc0001_16_non_aliased_id_resolves_to_itself() {
    // Arrange — tenant T with leaf Z and no assertion naming Z.
    let map = AliasMap::new();
    let t = TenantId::new("T");
    let z = 42_u64;

    // Act / Assert — Z expands to exactly {Z}, identical to the
    // base-member behaviour and to bare `template_id = Z` (RFC0001.6).
    assert_eq!(map.resolves(&t, z), set([z]));
    assert_eq!(map.canonical(&t, z), z);
}
