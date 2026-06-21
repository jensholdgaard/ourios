//! RFC 0017 — read-time template registry & query-row rendering, the
//! miner-emit arm of scenario `.1`.
//!
//! Asserts that allocating a new leaf emits a `template_created` audit event
//! carrying the leaf's `template_id` and the initial tokens — on the same
//! `AuditSink` (WAL-before-ack) path as the existing template events. The
//! `Created` variant carries no version field (a leaf is always born at v1,
//! made unrepresentable); the on-disk row stores `new_version = 1`.
//!
//! See `docs/rfcs/0017-template-registry-query-rendering.md` §3.1 / §5 / §6.

use ourios_core::audit::{AuditPayload, SharedAuditSink, TemplateChange};
use ourios_core::config::MinerConfig;
use ourios_core::otlp::{Body, OtlpLogRecord};
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::MinerCluster;

/// Scenario RFC0017.1 (miner-emit arm) — a new leaf's allocation emits a
/// `template_created` event with `(template_id, new_version = 1,
/// new_template = the initial tokens)`.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[test]
fn rfc0017_1_new_leaf_emits_template_created() {
    let sink = SharedAuditSink::new();
    let mut cluster = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(sink.clone()));
    let t = TenantId::new("tenant-a");

    // "user 42 logged in" masks `<NUM>` at position 1, so the leaf's
    // canonical template is "user <*> logged in".
    let id = cluster.ingest(&OtlpLogRecord {
        tenant_id: t.clone(),
        body: Some(Body::String("user 42 logged in".to_owned())),
        ..Default::default()
    });

    // The event is emitted through the same `AuditSink` as every other
    // template event (the WAL-before-ack path once the WAL sink lands).
    let events = sink.drain();
    assert_eq!(events.len(), 1, "fresh leaf emits exactly one audit event");
    let AuditPayload::Template {
        template_id,
        change: TemplateChange::Created { new_template },
        ..
    } = &events[0].payload
    else {
        panic!("expected Template/Created, got {:?}", events[0].payload);
    };
    assert_eq!(*template_id, id, "event names the allocated template_id");
    // The variant carries no version (a leaf is always born at v1, made
    // unrepresentable-if-otherwise); the on-disk row stores new_version = 1.
    assert_eq!(
        new_template, "user <*> logged in",
        "creation carries the initial tokens",
    );
}
