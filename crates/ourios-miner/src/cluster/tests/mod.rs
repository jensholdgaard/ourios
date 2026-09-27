use super::*;
use ourios_core::audit::SharedAuditSink;
use ourios_core::otlp::{AnyValue, ArrayValue, any_value::Value as AvValue};
use ourios_core::record::SharedRecordSink;
use proptest::prelude::*;

use crate::snapshot::{
    LeafRecord, ParamTypeRecord, SnapshotState, StructuredTemplateRecord, TokenRecord,
};

/// Test helper — a `Body::String` record for `tenant` carrying
/// `text` and default severity (UNSPECIFIED) / scope (None).
/// Keeps tests focused on their assertions rather than on
/// record-construction boilerplate.
fn string_record(tenant: &TenantId, text: &str) -> OtlpLogRecord {
    OtlpLogRecord {
        tenant_id: tenant.clone(),
        body: Some(Body::String(text.to_string())),
        ..Default::default()
    }
}

/// Test helper — a `Body::Structured` record for `tenant` with
/// the given severity and scope.
fn structured_record(tenant: &TenantId, severity: u8, scope: Option<&str>) -> OtlpLogRecord {
    OtlpLogRecord {
        tenant_id: tenant.clone(),
        severity_number: severity,
        scope_name: scope.map(str::to_string),
        body: Some(Body::Structured(AnyValue {
            value: Some(AvValue::IntValue(0)),
        })),
        ..Default::default()
    }
}

/// Test helper — build a cluster wired to a [`SharedAuditSink`]
/// and return both so the test can inspect emissions.
fn cluster_with_observable_sink() -> (MinerCluster, SharedAuditSink) {
    let sink = SharedAuditSink::new();
    let cluster = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(sink.clone()));
    (cluster, sink)
}

/// Drain the sink and return only the template *changes* a widening /
/// type-expansion / rejection test asserts on, dropping the per-leaf
/// `Created` events RFC 0017 §3.1 emits on every allocation. Leaf
/// creation is audited now (so a read-time registry can recover v1
/// tokens), but its correctness is covered by
/// `fresh_leaf_emits_created_event` and the RFC0017.1 acceptance test;
/// filtering here keeps each widening test decoupled from how many
/// leaves the scenario happens to allocate rather than re-asserting the
/// creation count in every one.
fn drain_changes(sink: &SharedAuditSink) -> Vec<AuditEvent> {
    sink.drain()
        .into_iter()
        .filter(|e| {
            !matches!(
                &e.payload,
                AuditPayload::Template {
                    change: TemplateChange::Created { .. },
                    ..
                }
            )
        })
        .collect()
}

/// Test helper — a cluster whose audit and record sinks are
/// both `SharedAuditSink`/`SharedRecordSink` clones so tests
/// can inspect what was emitted on both streams.
fn cluster_with_observable_sinks() -> (MinerCluster, SharedAuditSink, SharedRecordSink) {
    let audit = SharedAuditSink::new();
    let records = SharedRecordSink::new();
    let cluster = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(audit.clone()))
        .with_record_sink(Box::new(records.clone()));
    (cluster, audit, records)
}

mod adoption;
mod confidence;
mod emission;
mod ingest;
mod restore;
mod structured;
mod type_expansion;
mod widen;
