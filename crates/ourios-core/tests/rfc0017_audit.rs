//! RFC 0017 — read-time template registry & query-row rendering, the
//! audit-schema arm of scenario `.1`.
//!
//! Asserts the `template_created` audit event is an **append-only** addition:
//! a new `event_kind` ordinal `6` paired with the `event_type` string
//! `template_created`, with every existing ordinal (`0`–`5`) unchanged
//! (RFC 0005 §3.7), and that a `Created` payload derives the new
//! kind/type and does not count as a merge.
//!
//! See `docs/rfcs/0017-template-registry-query-rendering.md` §3.1 / §5 / §6.

use std::time::SystemTime;

use ourios_core::audit::{
    AuditEvent, AuditPayload, EVENT_KIND_ALIAS_ASSERTED, EVENT_KIND_ALIAS_RETRACTED,
    EVENT_KIND_COMPACTION, EVENT_KIND_TEMPLATE_CREATED, EVENT_KIND_TEMPLATE_TYPE_EXPANDED,
    EVENT_KIND_TEMPLATE_WIDENED, EVENT_KIND_TEMPLATE_WIDENING_REJECTED_DEGENERATE,
    EVENT_TYPE_TEMPLATE_CREATED, TemplateChange, hash_triggering_line,
};
use ourios_core::tenant::TenantId;

/// Scenario RFC0017.1 (audit-schema arm) — `template_created` is an
/// append-only audit addition: ordinal `6` / `event_type = "template_created"`,
/// existing ordinals `0`–`5` unchanged (RFC 0005 §3.7).
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[test]
fn rfc0017_1_template_created_is_append_only_audit_addition() {
    // The new ordinal is the next free value, and the existing ordinals
    // are untouched (the RFC 0005 §3.7 append-only rule — no renumber, so
    // old readers are unaffected).
    assert_eq!(EVENT_KIND_TEMPLATE_CREATED, 6);
    assert_eq!(EVENT_TYPE_TEMPLATE_CREATED, "template_created");
    assert_eq!(EVENT_KIND_TEMPLATE_WIDENED, 0);
    assert_eq!(EVENT_KIND_TEMPLATE_TYPE_EXPANDED, 1);
    assert_eq!(EVENT_KIND_TEMPLATE_WIDENING_REJECTED_DEGENERATE, 2);
    assert_eq!(EVENT_KIND_COMPACTION, 3);
    assert_eq!(EVENT_KIND_ALIAS_ASSERTED, 4);
    assert_eq!(EVENT_KIND_ALIAS_RETRACTED, 5);

    // All seven ordinals are distinct — no collision with the new one.
    let mut ordinals = [
        EVENT_KIND_TEMPLATE_WIDENED,
        EVENT_KIND_TEMPLATE_TYPE_EXPANDED,
        EVENT_KIND_TEMPLATE_WIDENING_REJECTED_DEGENERATE,
        EVENT_KIND_COMPACTION,
        EVENT_KIND_ALIAS_ASSERTED,
        EVENT_KIND_ALIAS_RETRACTED,
        EVENT_KIND_TEMPLATE_CREATED,
    ];
    let count = ordinals.len();
    ordinals.sort_unstable();
    let mut deduped = ordinals.to_vec();
    deduped.dedup();
    assert_eq!(deduped.len(), count, "event_kind ordinals must be distinct");

    // A `Created` payload derives the new kind/type and is not a merge.
    let event = AuditEvent {
        tenant_id: TenantId::new("tenant-x"),
        timestamp: SystemTime::UNIX_EPOCH,
        payload: AuditPayload::Template {
            template_id: 7,
            triggering_line_hash: hash_triggering_line(b"user 42 logged in"),
            triggering_line_sample: Some("user 42 logged in".to_owned()),
            change: TemplateChange::Created {
                new_version: 1,
                new_template: "user <*> logged in".to_owned(),
            },
        },
    };
    assert_eq!(event.payload.event_kind(), EVENT_KIND_TEMPLATE_CREATED);
    assert_eq!(event.payload.event_type(), EVENT_TYPE_TEMPLATE_CREATED);
    assert!(
        !event.payload.counts_as_merge(),
        "leaf creation is not a merge",
    );
}
