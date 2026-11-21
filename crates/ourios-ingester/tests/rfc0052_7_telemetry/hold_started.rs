//! RFC0056.11: a tenant whose template events cannot be written is held,
//! and `ourios.receiver.tenant.hold.started` names it once, with the
//! failure's `error.type`.

use std::time::{Duration, UNIX_EPOCH};

use ourios_core::audit::{AuditEvent, AuditPayload, TemplateChange, hash_triggering_line};
use ourios_core::tenant::TenantId;
use ourios_ingester::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use ourios_parquet::Store;
use ourios_semconv as semconv;
use ourios_telemetry::live_check::{self, Checked, EventSpec};

use crate::harness::{harness, serial};

const HOLD_STARTED: EventSpec = EventSpec {
    name: semconv::EVENT_OURIOS_RECEIVER_TENANT_HOLD_STARTED,
    required: &[semconv::OURIOS_TENANT, "error.type"],
    optional: &[],
};

/// A template event no partition can take: its timestamp is before the
/// epoch, so every write of it fails the same way.
fn unwritable(tenant: &str) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: UNIX_EPOCH - Duration::from_secs(1),
        payload: AuditPayload::Template {
            template_id: 1,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: Some("line".to_owned()),
            change: TemplateChange::Created {
                new_template: "user <*> logged in".to_owned(),
            },
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hold_is_announced_once_and_is_registry_conformant() {
    let _serial = serial().await;
    let dir = tempfile::TempDir::new().expect("temp");
    let store = Store::local(dir.path()).expect("store");
    let sink = SharedParquetAuditSink::new(BufferingAuditSink::new(store, 1024)).holding();

    for _ in 0..2 {
        let _ = sink.write_retaining(vec![unwritable("acme")]);
    }
    assert!(sink.held().is_held(&TenantId::new("acme")));

    let events = harness().events();
    let started: Vec<_> = events
        .iter()
        .filter(|e| e.name == HOLD_STARTED.name)
        .map(|e| {
            (
                e.attributes.get(semconv::OURIOS_TENANT).cloned(),
                e.error_type().map(str::to_owned),
            )
        })
        .collect();
    assert_eq!(
        started,
        vec![(Some("acme".to_owned()), Some("batch".to_owned()))],
        "one announcement for the held tenant: {events:#?}"
    );

    let checked = live_check::live_check(&events, &[HOLD_STARTED])
        .expect("the hold event is registry-conformant");
    if checked == Checked::SpecOnly {
        eprintln!(
            "RFC0056.11: weaver is not configured here; checked the event against its spec only"
        );
    }
}
