//! Scenario RFC0050.8 — the read path carries **both** template
//! identities, without inventing attributes.
//!
//! The response gains the template *string* beside the existing
//! `template_id` / `template_version` fields (no second
//! `list_templates` call), while the record's `attributes` stay
//! byte-identical to what ingest stored (RFC 0018 fidelity): a
//! producer-sent `log.record.template` survives the round trip, and
//! one is never injected into a record whose producer did not send
//! it.
//!
//! See `docs/rfcs/0050-upstream-derived-templates.md` §3.6 / §5.

use std::time::{Duration, UNIX_EPOCH};

use ourios_core::audit::{
    AuditEvent, AuditPayload, AuditSink, TemplateChange, hash_triggering_line,
};
use ourios_core::tenant::TenantId;
use ourios_parquet::{ParquetAuditSink, Store};
use ourios_querier::{Querier, QueryRequest};
use tempfile::TempDir;

use crate::common::{NOW, kv, simple, write_all};

fn created(template_id: u64, template: &str) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new("acme"),
        timestamp: UNIX_EPOCH + Duration::from_secs(100),
        payload: AuditPayload::Template {
            template_id,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change: TemplateChange::Created {
                new_template: template.to_owned(),
            },
        },
    }
}

fn rows_request() -> QueryRequest {
    QueryRequest {
        tenant: TenantId::new("acme"),
        time_range: None,
        template_id: None,
        severity_text: None,
        limit: Some(10),
    }
}

#[tokio::test]
async fn rfc0050_8_response_carries_the_string_beside_the_id() {
    let bucket = TempDir::new().unwrap();

    // Two rows: one whose producer sent a `log.record.template`
    // attribute, one bare.
    let mut annotated = simple("acme", 1, NOW - 1);
    annotated.attributes = vec![
        kv("log.record.template", "user <name> logged in"),
        kv("http.method", "GET"),
    ];
    let bare = simple("acme", 1, NOW - 2);
    write_all(bucket.path(), &[annotated.clone(), bare.clone()]);

    // The audit stream gives the read-time registry the (1, 1) tokens.
    let mut sink = ParquetAuditSink::new(Store::local(bucket.path()).expect("store"));
    sink.emit(created(1, "user <*>"));
    assert_eq!(sink.write_failures(), 0, "fixture event must persist");

    let result = Querier::new(bucket.path())
        .run(rows_request())
        .await
        .expect("query");
    assert_eq!(result.records.len(), 2);

    for row in &result.records {
        // The string beside the id — no second list_templates call.
        assert_eq!(row.template_id, 1);
        assert_eq!(row.template_version, 1);
        assert_eq!(row.template.as_deref(), Some("user <*>"));
    }

    // RFC 0018 fidelity: the attributes array is exactly what ingest
    // stored — the producer-sent string survives verbatim, and the
    // bare record gains nothing.
    let by_ts = |ts: u64| {
        result
            .records
            .iter()
            .find(|r| r.time_unix_nano == ts)
            .expect("row present")
    };
    assert_eq!(by_ts(NOW - 1).attributes, annotated.attributes);
    assert_eq!(by_ts(NOW - 2).attributes, bare.attributes);
    assert!(
        !by_ts(NOW - 2)
            .attributes
            .iter()
            .any(|a| a.key == "log.record.template"),
        "no derived template is ever injected into attributes",
    );
}

#[tokio::test]
async fn rfc0050_8_unresolvable_pair_omits_the_string() {
    let bucket = TempDir::new().unwrap();
    // No audit stream at all: the registry cannot resolve (1, 1).
    write_all(bucket.path(), &[simple("acme", 1, NOW - 1)]);

    let result = Querier::new(bucket.path())
        .run(rows_request())
        .await
        .expect("query");
    assert_eq!(result.records.len(), 1);
    assert_eq!(
        result.records[0].template, None,
        "an unresolvable (id, version) yields no string, never a guess",
    );
}
