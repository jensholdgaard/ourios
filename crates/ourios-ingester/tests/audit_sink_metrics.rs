//! The buffering audit sink exports its `OpenTelemetry` instruments (issue
//! #302 / `AGENTS.md` §6.3). A dedicated test binary: `init_in_memory` installs the
//! **global** meter provider, so this must not share a process with another
//! provider-installing test (the colocated `metrics.rs` compaction test).

use std::time::{Duration, UNIX_EPOCH};

use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
};
use ourios_core::audit::{
    AuditEvent, AuditPayload, AuditSink, TemplateChange, hash_triggering_line,
};
use ourios_core::tenant::TenantId;
use ourios_ingester::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use ourios_ingester::metrics::FLUSH_OUTCOME_TRANSIENT;
use ourios_parquet::Store;
use ourios_semconv as semconv;

fn created_event(tenant: &str, template_id: u64) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: UNIX_EPOCH + Duration::from_secs(1_775_127_480),
        payload: AuditPayload::Template {
            template_id,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: Some("line".to_owned()),
            change: TemplateChange::Created {
                new_template: "user <*> logged in".to_owned(),
            },
        },
    }
}

fn names(rms: &[ResourceMetrics]) -> Vec<String> {
    rms.iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .map(|m| m.name().to_string())
        .collect()
}

fn assert_exported(rms: &[ResourceMetrics]) {
    let collected = names(rms);
    for expected in [
        semconv::OURIOS_AUDIT_SINK_BUFFER_USAGE,
        semconv::OURIOS_AUDIT_SINK_FLUSHES,
        semconv::OURIOS_AUDIT_SINK_FLUSH_EVENTS,
        semconv::OURIOS_AUDIT_SINK_FLUSH_ERRORS,
        semconv::OURIOS_AUDIT_SINK_DERIVE_ERRORS,
    ] {
        assert!(
            collected.iter().any(|n| n == expected),
            "exported stream missing {expected}, got {collected:?}",
        );
    }
}

fn metric<'a>(rms: &'a [ResourceMetrics], name: &str) -> &'a AggregatedMetrics {
    rms.iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .find(|m| m.name() == name)
        .unwrap_or_else(|| panic!("metric {name} missing from the exported stream"))
        .data()
}

fn u64_sum(rms: &[ResourceMetrics], name: &str) -> u64 {
    let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric(rms, name) else {
        panic!("{name} should be a u64 sum");
    };
    sum.data_points().fold(0, |acc, dp| acc + dp.value())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn audit_sink_exports_its_instruments() {
    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");
    // Build the sink after the provider is installed so its instruments resolve
    // against it.
    let dir = tempfile::TempDir::new().expect("temp");
    let store_root = dir.path().join("store");
    std::fs::create_dir_all(&store_root).expect("create store root");
    let store = Store::local(&store_root).expect("store");
    // Holding is inert until phase 3; a second sink would skew the gauge.
    let mut sink = SharedParquetAuditSink::new(BufferingAuditSink::new(store, 1024)).holding();

    // Phase 1 — three events across two tenants flush to two files.
    sink.emit(created_event("alpha", 1));
    sink.emit(created_event("alpha", 2));
    sink.emit(created_event("bravo", 1));
    assert!(sink.flush(), "a healthy store fully drains");

    // Phase 2 — one more event, then a sabotaged store forces a transient
    // (Io) flush error: the event is retained, the error counted.
    sink.emit(created_event("alpha", 3));
    std::fs::remove_dir_all(&store_root).expect("remove store dir");
    std::fs::write(&store_root, b"not a directory").expect("sabotage");
    assert!(
        !sink.flush(),
        "a transient store error is not fully drained"
    );
    assert_eq!(
        sink.buffered_events(),
        1,
        "the transient error retained the event"
    );
    lose_a_template_event_for_good(&sink);

    guard.force_flush().expect("force_flush");
    let rms = exporter.get_finished_metrics().expect("metrics exported");

    assert_exported(&rms);

    assert_eq!(
        u64_sum(&rms, semconv::OURIOS_AUDIT_SINK_FLUSHES),
        2,
        "two partitions flushed (one file each)",
    );
    assert_eq!(
        u64_sum(&rms, semconv::OURIOS_AUDIT_SINK_FLUSH_EVENTS),
        3,
        "three events written across the successful flush",
    );

    // The flush error carries the transient disposition.
    let AggregatedMetrics::U64(MetricData::Sum(errors)) =
        metric(&rms, semconv::OURIOS_AUDIT_SINK_FLUSH_ERRORS)
    else {
        panic!("flush.errors should be a u64 sum");
    };
    assert!(
        errors.data_points().any(|dp| {
            dp.value() == 1
                && dp.attributes().any(|kv| {
                    kv.key.as_str() == semconv::OURIOS_AUDIT_SINK_FLUSH_OUTCOME
                        && kv.value.as_str() == FLUSH_OUTCOME_TRANSIENT
                })
        }),
        "the transient flush error is recorded with outcome=transient",
    );

    // The observable gauge reports the one retained event.
    let AggregatedMetrics::I64(MetricData::Sum(buffer)) =
        metric(&rms, semconv::OURIOS_AUDIT_SINK_BUFFER_USAGE)
    else {
        panic!("buffer.usage should be an i64 sum (observable UpDownCounter)");
    };
    assert_eq!(
        buffer.data_points().fold(0_i64, |acc, dp| acc + dp.value()),
        1,
        "the buffer gauge reports the retained event",
    );

    assert_one_tenant_held(&rms, "charlie");
}

/// Phase 3 — RFC 0056 §3.2: a template event lost for good (a pre-epoch
/// timestamp derives no partition, so no store I/O) holds its tenant,
/// which counts once however many of its events are lost.
fn lose_a_template_event_for_good(sink: &SharedParquetAuditSink) {
    for _ in 0..2 {
        let lost = AuditEvent {
            timestamp: UNIX_EPOCH - Duration::from_secs(1),
            ..created_event("charlie", 1)
        };
        let _ = sink.write_retaining(vec![lost]);
    }
}

/// The held tenant counts once, under its `ourios.tenant`.
fn assert_one_tenant_held(rms: &[ResourceMetrics], tenant: &str) {
    let AggregatedMetrics::I64(MetricData::Sum(held)) =
        metric(rms, semconv::OURIOS_RECEIVER_TENANT_HELD)
    else {
        panic!("tenant.held should be an i64 sum (UpDownCounter)");
    };
    let points: Vec<_> = held
        .data_points()
        .map(|dp| {
            let tenant = dp
                .attributes()
                .find(|kv| kv.key.as_str() == semconv::OURIOS_TENANT)
                .map(|kv| kv.value.as_str().into_owned());
            (tenant, dp.value())
        })
        .collect();
    assert_eq!(points, vec![(Some(tenant.to_owned()), 1)]);
}
