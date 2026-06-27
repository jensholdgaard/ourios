//! RFC 0017 — read-time template registry & query-row rendering, the
//! registry-derivation scenario (`.2`).
//!
//! `.2` is `derive_template_registry` completeness. The version-correct
//! *rendering* scenario (`.5`) lives in `rfc0017_rendering.rs` with the other
//! `render_log_body` scenarios.
//!
//! See `docs/rfcs/0017-template-registry-query-rendering.md` §3.2 / §5 / §6.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ourios_core::audit::{
    AuditEvent, AuditPayload, AuditSink, TemplateChange, hash_triggering_line,
};
use ourios_core::tenant::TenantId;
use ourios_miner::tree::OwnedToken;
use ourios_parquet::{ParquetAuditSink, Store};
use ourios_querier::derive_template_registry;
use tempfile::TempDir;

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// A template audit event for `tenant` / `template_id` at `secs`, carrying
/// `change`. Templates use the real space-joined canonical form
/// (`tree::format_template`'s output), so the registry's `parse_template`
/// recovers them faithfully.
fn template_event(tenant: &str, template_id: u64, secs: u64, change: TemplateChange) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: at(secs),
        payload: AuditPayload::Template {
            template_id,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change,
        },
    }
}

fn fixed(s: &str) -> OwnedToken {
    OwnedToken::Fixed(s.to_owned())
}

/// Scenario RFC0017.2 — `derive_template_registry` folds a tenant audit stream
/// of `template_created` / `template_widened` / `template_type_expanded` events
/// (deterministic `(timestamp, path, row)` order) into a registry containing
/// the tokens for **every** `(template_id, version)` the stream describes,
/// including version 1, with later versions not clobbering earlier ones.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[test]
fn rfc0017_2_registry_derives_completely_including_v1() {
    let bucket = TempDir::new().unwrap();
    let tenant = "acme";

    // Template 1: created at v1 ("user <*>"), then widened to v2
    // ("user <*> <*>"). Template 2: created at v1 ("GET <*>"), then
    // type-expanded to v2 (template unchanged — the §3.7 invariant for that
    // kind). A rejection contributes nothing.
    let events = vec![
        template_event(
            tenant,
            1,
            100,
            TemplateChange::Created {
                new_template: "user <*>".to_owned(),
            },
        ),
        template_event(
            tenant,
            1,
            200,
            TemplateChange::Widened {
                old_version: 1,
                new_version: 2,
                old_template: "user <*>".to_owned(),
                new_template: "user <*> <*>".to_owned(),
                positions_widened: vec![2],
            },
        ),
        template_event(
            tenant,
            2,
            150,
            TemplateChange::Created {
                new_template: "GET <*>".to_owned(),
            },
        ),
        template_event(
            tenant,
            2,
            250,
            TemplateChange::TypeExpanded {
                old_version: 1,
                new_version: 2,
                old_template: "GET <*>".to_owned(),
                new_template: "GET <*>".to_owned(),
                slots_expanded: Vec::new(),
            },
        ),
        template_event(
            tenant,
            1,
            300,
            TemplateChange::RejectedDegenerate {
                version: 2,
                current_template: "user <*> <*>".to_owned(),
                would_be_template: "<*> <*> <*>".to_owned(),
                would_be_positions: vec![0],
            },
        ),
    ];

    let mut sink = ParquetAuditSink::new(bucket.path());
    for e in &events {
        sink.emit(e.clone());
    }
    assert_eq!(sink.write_failures(), 0, "fixture events must all persist");

    // RFC 0019 — the registry derivation now reads through the `Store` seam;
    // `Some(local_root)` selects the local hybrid branch (the std::fs walk).
    let store = Store::local(bucket.path()).expect("local store");
    let registry = derive_template_registry(&store, Some(bucket.path()), &TenantId::new(tenant))
        .expect("derive");

    // Every (template_id, version) the stream described is present — including
    // version 1 (the `template_created` events), the gap this RFC closes.
    assert_eq!(
        registry.get(&(1, 1)),
        Some(&vec![fixed("user"), OwnedToken::Wildcard]),
        "template 1 v1 tokens recovered from template_created",
    );
    assert_eq!(
        registry.get(&(1, 2)),
        Some(&vec![
            fixed("user"),
            OwnedToken::Wildcard,
            OwnedToken::Wildcard
        ]),
        "template 1 v2 tokens from the widening — v1 not clobbered",
    );
    assert_eq!(
        registry.get(&(2, 1)),
        Some(&vec![fixed("GET"), OwnedToken::Wildcard]),
        "template 2 v1 tokens recovered",
    );
    assert_eq!(
        registry.get(&(2, 2)),
        Some(&vec![fixed("GET"), OwnedToken::Wildcard]),
        "template 2 v2 (type-expanded; template unchanged)",
    );
    // The rejection added no entry; exactly the four versions above.
    assert_eq!(registry.len(), 4, "no entry for the degenerate rejection");
}

// Note: the row-vs-path tenant backstop in `derive_template_registry` (RFC
// 0005 §3.9) is defense-in-depth against a hand-corrupted file. It is *not*
// reachable through the supported write path — `AuditWriter` rejects a
// tenant/partition mismatch at write time (`PartitionMismatch`) — so, like the
// identical backstop in `alias_store::derive_alias_map`, it is not exercised by
// a forged fixture here. The fold logic is unit-tested in the module.

// Scenario RFC0017.5 (rows render against their own template version) lives in
// `rfc0017_rendering.rs`, alongside the other `render_log_body` scenarios.
