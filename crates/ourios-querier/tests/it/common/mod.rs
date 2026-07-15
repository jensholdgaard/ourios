//! Shared querier integration-test fixtures: a real RFC 0005 store written by
//! `ourios-parquet`, the same way `tests/execution.rs` builds one, so a
//! compiled DSL runs against genuine Parquet (predicate pushdown + statistics,
//! not a mock). Used by both `rfc0002_dsl.rs` and `rfc0001_query_semantics.rs`.

// Each integration-test file (`rfc0002_dsl.rs`, `rfc0001_query_semantics.rs`)
// compiles this module into its own binary independently, so an item used by
// only one of them looks dead to the other. This is the standard idiom for a
// shared `tests/common` module.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;

use ourios_core::audit::ParamType;
use ourios_core::otlp::any_value::Value as AvValue;
use ourios_core::otlp::{AnyValue, KeyValue};
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{DEFAULT_ZSTD_LEVEL, PartitionKey, PromotedAttributes, Writer};

/// 2026-04-02T10:58:00 UTC — the same base instant the execution tests
/// use, so all fixture rows land in one `hour=` partition unless bumped.
pub const TS0: u64 = 1_775_127_480_000_000_000;
/// One hour in nanoseconds.
pub const HOUR_NS: u64 = 3_600_000_000_000;

pub fn kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(AvValue::StringValue(value.to_string())),
        }),
        ..Default::default()
    }
}

/// A fully-populated fixture record so every first-class field (§6.2) has
/// a non-trivial value to query: a `service.name` resource attribute, a
/// `scope`, an explicit severity, and optional trace/span ids.
#[allow(clippy::too_many_arguments)]
pub fn rec(
    tenant: &str,
    template_id: u64,
    ts_ns: u64,
    severity_number: u8,
    service: &str,
    scope: &str,
    trace_id: Option<[u8; 16]>,
    span_id: Option<[u8; 8]>,
) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new(tenant),
        template_id,
        template_version: 1,
        severity_number,
        severity_text: None,
        scope_name: Some(scope.to_string()),
        scope_version: Some("1.0.0".to_string()),
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: ts_ns,
        observed_time_unix_nano: Some(ts_ns + 1_000),
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: vec![kv("service.name", service)],
        trace_id,
        span_id,
        flags: 0x01,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ParamType::Num,
            value: "42".to_string(),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

/// A minimal record: template 1, INFO, service "api", scope "lib.cart".
pub fn simple(tenant: &str, template_id: u64, ts_ns: u64) -> MinedRecord {
    rec(tenant, template_id, ts_ns, 9, "api", "lib.cart", None, None)
}

/// A [`simple`] record with an explicit positional `params` list (RFC 0005
/// §3.2), for the §6.3 `param(n)` group-term scenarios — including short
/// lists (RFC0002.15's excluded rows).
pub fn rec_with_params(tenant: &str, template_id: u64, ts_ns: u64, params: &[&str]) -> MinedRecord {
    MinedRecord {
        params: params
            .iter()
            .map(|v| Param {
                type_tag: ParamType::Str,
                value: (*v).to_string(),
            })
            .collect(),
        // The writer's three-zone invariant: a string-bodied record carries
        // `params.len() + 1` separators.
        separators: vec![String::new(); params.len() + 1],
        ..simple(tenant, template_id, ts_ns)
    }
}

/// A record with explicit resource attributes (overriding the default
/// single `service.name`), so a test can give one row a key and another
/// row none.
pub fn rec_with_resource(
    tenant: &str,
    ts_ns: u64,
    resource_attributes: Vec<KeyValue>,
) -> MinedRecord {
    MinedRecord {
        resource_attributes,
        ..rec(tenant, 1, ts_ns, 9, "api", "lib.cart", None, None)
    }
}

pub fn write_all(bucket: &Path, recs: &[MinedRecord]) {
    let mut by_part: HashMap<PartitionKey, Vec<MinedRecord>> = HashMap::new();
    for r in recs {
        by_part
            .entry(PartitionKey::derive(r).expect("derive partition"))
            .or_default()
            .push(r.clone());
    }
    for (part, rs) in by_part {
        let mut w = Writer::open(bucket, part).expect("open writer");
        w.append_records(&rs).expect("append");
        w.close().expect("close");
    }
}

/// [`write_all`] with an explicit RFC 0022 promoted attribute set, so a test
/// can seed post-amendment files whose promoted columns go beyond the
/// implicit `service.name`.
pub fn write_all_with_promoted(bucket: &Path, recs: &[MinedRecord], promoted: &PromotedAttributes) {
    let store = Store::local(bucket).expect("local store");
    let mut by_part: HashMap<PartitionKey, Vec<MinedRecord>> = HashMap::new();
    for r in recs {
        by_part
            .entry(PartitionKey::derive(r).expect("derive partition"))
            .or_default()
            .push(r.clone());
    }
    for (part, rs) in by_part {
        let mut w =
            Writer::open_in_with_promoted(&store, part, DEFAULT_ZSTD_LEVEL, promoted.clone())
                .expect("open writer");
        w.append_records(&rs).expect("append");
        w.close().expect("close");
    }
}

/// A record with explicit log `attributes` on top of [`rec_with_resource`]'s
/// explicit resource attributes, so promoted-column tests can drive both
/// families.
pub fn rec_with_attrs(
    tenant: &str,
    ts_ns: u64,
    resource_attributes: Vec<KeyValue>,
    attributes: Vec<KeyValue>,
) -> MinedRecord {
    MinedRecord {
        attributes,
        ..rec_with_resource(tenant, ts_ns, resource_attributes)
    }
}

/// A window wide enough that a query with no `range(...)` (which gets the
/// default look-back ending at `now`) still covers all fixture rows.
pub const DEFAULT_WINDOW_NS: u64 = 30 * 24 * HOUR_NS;
/// A `now` reference comfortably after the fixture instants.
pub const NOW: u64 = TS0 + 24 * HOUR_NS;

/// An empty alias projection: no operator has aliased anything, so every
/// `resolves_to(n)` compiles to a singleton `template_id IN (n)` list
/// (behaviorally a bare `template_id == n`), and a bare `template_id == n`
/// resolves with no alias chain to follow.
pub fn no_aliases() -> ourios_core::alias::AliasMap {
    ourios_core::alias::AliasMap::new()
}

// --- RFC 0010 audit-stream fixtures (drift query tests) ---

use ourios_core::audit::{
    AuditEvent, AuditPayload, AuditSink, TemplateChange, hash_triggering_line,
};
use ourios_parquet::{ParquetAuditSink, Store};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A `SystemTime` `ns` nanoseconds after the epoch — the audit `timestamp`
/// field's type. Pairs with the `u64` nanos the drift window is expressed in.
pub fn at(ns: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(ns)
}

/// A `template_widened` audit event for `template_id` at `ts_ns`, bumping
/// `old_version` → `old_version + 1` (a widening always bumps by one).
pub fn widened(tenant: &str, template_id: u64, old_version: u32, ts_ns: u64) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: at(ts_ns),
        payload: AuditPayload::Template {
            template_id,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change: TemplateChange::Widened {
                old_version,
                new_version: old_version + 1,
                old_template: "[\"user\",\"<*>\"]".to_string(),
                new_template: "[\"user\",\"<*>\",\"<*>\"]".to_string(),
                positions_widened: vec![1],
            },
        },
    }
}

/// A `template_type_expanded` audit event for `template_id` at `ts_ns`. The
/// template literal is unchanged (the §3.7 invariant for this kind), only the
/// slot's type-set widened.
pub fn type_expanded(tenant: &str, template_id: u64, old_version: u32, ts_ns: u64) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: at(ts_ns),
        payload: AuditPayload::Template {
            template_id,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change: TemplateChange::TypeExpanded {
                old_version,
                new_version: old_version + 1,
                old_template: "[\"user\",\"<*>\"]".to_string(),
                new_template: "[\"user\",\"<*>\"]".to_string(),
                slots_expanded: Vec::new(),
            },
        },
    }
}

/// A `template_widening_rejected_degenerate` audit event for `template_id` at
/// `ts_ns` — a *non*-change that must not count toward `widening_count`
/// (RFC0010.3).
pub fn rejected_degenerate(tenant: &str, template_id: u64, version: u32, ts_ns: u64) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: at(ts_ns),
        payload: AuditPayload::Template {
            template_id,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change: TemplateChange::RejectedDegenerate {
                version,
                current_template: "[\"<*>\"]".to_string(),
                would_be_template: "[\"<*>\"]".to_string(),
                would_be_positions: vec![0],
            },
        },
    }
}

/// A `compaction` audit event at `ts_ns` — not a template change, excluded
/// from drift by the `event_type` filter (RFC0010.3).
pub fn compaction(tenant: &str, ts_ns: u64) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: at(ts_ns),
        payload: AuditPayload::Compaction {
            partition: "year=2026/month=06/day=01/hour=00".to_string(),
            input_files: vec!["a.parquet".to_string(), "b.parquet".to_string()],
            output_file: "c.parquet".to_string(),
            generation: 1,
            rows: 10,
        },
    }
}

/// Seed audit events into the RFC 0005 `audit/` stream under `bucket` via the
/// production `ParquetAuditSink` (real Parquet, the same write path drift
/// reads). Asserts no write failures so a test never silently runs against an
/// empty stream.
pub fn write_audit(bucket: &Path, events: &[AuditEvent]) {
    let mut sink = ParquetAuditSink::new(Store::local(bucket).expect("store"));
    for e in events {
        sink.emit(e.clone());
    }
    assert_eq!(
        sink.write_failures(),
        0,
        "audit fixture events must all persist",
    );
}
