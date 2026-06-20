//! Record materialisation (RFC 0003 §6.1 steps 2–3).
//!
//! Maps one decoded OTLP `LogRecord` to the flat
//! [`OtlpLogRecord`] the miner consumes, inheriting the enclosing
//! `Resource` attributes and `InstrumentationScope` name/version so
//! downstream code never walks back up the OTLP hierarchy. The mapping
//! narrows proto's "empty value = absence" sentinels into a single
//! `Option`/`None` at this boundary (RFC0003.9), reflects
//! `dropped_attributes_count` verbatim (RFC0003.10), and forks the body
//! via [`Body::from_any_value`] (RFC0003.7/.8).
//!
//! Tenant derivation (RFC0003.3, per `ResourceLogs`) is *not* done here:
//! `materialize_record` takes the resolved `tenant_id` as a parameter,
//! so the fan-out slice supplies it.

use opentelemetry_proto::tonic::common::v1::{InstrumentationScope, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs};
use ourios_core::otlp::{Body, OtlpLogRecord};
use ourios_core::tenant::TenantId;

/// Materialise one decoded `LogRecord` into an [`OtlpLogRecord`] under
/// `tenant_id`, inheriting `resource_attributes` and the enclosing
/// `scope`.
///
/// Consumes `record` so its body `AnyValue` and per-record attributes
/// *move* into the result — no deep clone of structured trees, per the
/// §6.4 amendment. `resource_attributes` and the scope name/version are
/// shared across the records under a `ResourceLogs`/`ScopeLogs`, so they
/// are cloned per record.
#[must_use]
pub fn materialize_record(
    record: LogRecord,
    resource_attributes: &[KeyValue],
    resource_schema_url: &str,
    scope: Option<&InstrumentationScope>,
    scope_schema_url: &str,
    tenant_id: TenantId,
) -> OtlpLogRecord {
    OtlpLogRecord {
        tenant_id,
        // Event time: `0` = unknown per the OTLP spec, kept as `0`
        // (a `u64`, not narrowed — absence and "epoch 0" are the same
        // wire value and the schema models it as a plain `u64`).
        time_unix_nano: record.time_unix_nano,
        // Collector observation time: wire `0` = unset → `None`
        // (RFC0003.9; the `Option<u64>` typing exists for this).
        observed_time_unix_nano: nonzero(record.observed_time_unix_nano),
        // `UNSPECIFIED` (`0`) is an explicit OTLP value, kept as `0`
        // (RFC0003.9); proto's `i32` is narrowed to the schema's
        // documented `0..=24` `u8` range — see `severity_to_u8`.
        severity_number: severity_to_u8(record.severity_number),
        severity_text: nonempty(record.severity_text),
        scope_name: scope.and_then(|s| (!s.name.is_empty()).then(|| s.name.clone())),
        scope_version: scope.and_then(|s| (!s.version.is_empty()).then(|| s.version.clone())),
        // RFC 0018 §3.1 — the scope's own attributes, and the schema URLs from
        // the ScopeLogs / ResourceLogs wrappers. Empty wire string → `None`
        // (the RFC0003.9 absence rule; proto3 can't distinguish unset from "").
        scope_attributes: scope.map(|s| s.attributes.clone()).unwrap_or_default(),
        resource_schema_url: (!resource_schema_url.is_empty())
            .then(|| resource_schema_url.to_string()),
        scope_schema_url: (!scope_schema_url.is_empty()).then(|| scope_schema_url.to_string()),
        attributes: record.attributes,
        // Reflected verbatim from the wire, never recomputed (RFC0003.10).
        dropped_attributes_count: record.dropped_attributes_count,
        resource_attributes: resource_attributes.to_vec(),
        trace_id: fixed_len(&record.trace_id),
        span_id: fixed_len(&record.span_id),
        flags: record.flags,
        event_name: nonempty(record.event_name),
        // `string_value` → mining path (`Body::String`), every other
        // variant → `Body::Structured` verbatim (RFC0003.7/.8); `None`
        // when the wire delivered no body.
        body: record.body.and_then(Body::from_any_value),
    }
}

/// Materialise every `LogRecord` under one `ResourceLogs` group into
/// `OtlpLogRecord`s under `tenant_id`, in `(ScopeLogs, LogRecord)` order.
///
/// The group's `Resource` attributes are inherited by every record, and
/// each `ScopeLogs` contributes its own `InstrumentationScope`
/// name/version. Tenant derivation (RFC0003.3, per `ResourceLogs`) is the
/// caller's job — this takes the already-resolved `tenant_id`.
#[must_use]
pub fn materialize_resource_logs(
    resource_logs: ResourceLogs,
    tenant_id: &TenantId,
) -> Vec<OtlpLogRecord> {
    let resource_attributes = resource_logs
        .resource
        .map(|resource| resource.attributes)
        .unwrap_or_default();
    let resource_schema_url = resource_logs.schema_url;
    let mut records = Vec::new();
    for scope_logs in resource_logs.scope_logs {
        let scope = scope_logs.scope;
        let scope_schema_url = scope_logs.schema_url;
        for record in scope_logs.log_records {
            records.push(materialize_record(
                record,
                &resource_attributes,
                &resource_schema_url,
                scope.as_ref(),
                &scope_schema_url,
                tenant_id.clone(),
            ));
        }
    }
    records
}

/// Narrow proto's `i32` `severity_number` to the schema's `u8`, **preserving**
/// the wire value (RFC 0018 §3.5 — faithful witness, §3.0). Valid OTLP
/// severity is `0..=24` (`0` = UNSPECIFIED); out-of-named-range values
/// (`25..=255`) are kept verbatim (monotone-meaningful and surfaced via the
/// ingest counter's `error.type = severity_out_of_range`, not silently
/// clamped). Only the extremes a `u8` cannot hold — negative or `> 255` —
/// narrow to `0`, where the storage invariant wins.
fn severity_to_u8(n: i32) -> u8 {
    u8::try_from(n).unwrap_or(0)
}

/// Whether a stored `severity_number` is outside the OTLP `0..=24` range
/// (RFC 0018 §3.5). Drives the `error.type = severity_out_of_range` attribute
/// on the ingest counter. Operates on the *preserved* `u8`: `25..=255` is
/// detected here; the negative / `> 255` extremes narrowed to `0` are out of
/// range too but indistinguishable post-narrowing, so attribution is
/// best-effort over the `u8`-storable band (see §3.5).
#[must_use]
pub(crate) fn severity_is_out_of_range(severity_number: u8) -> bool {
    severity_number > 24
}

/// Proto scalar `0` → `None`, else `Some` — the RFC0003.9 narrowing of a
/// "0 = unset" wire sentinel.
fn nonzero(v: u64) -> Option<u64> {
    (v != 0).then_some(v)
}

/// Proto empty string → `None`, else `Some` — narrows the "empty = unset"
/// sentinel proto uses for optional strings.
fn nonempty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

/// A proto `bytes` id (`trace_id` / `span_id`): exactly `N` bytes →
/// `Some`; empty (absent) or any other length (malformed) → `None`.
fn fixed_len<const N: usize>(bytes: &[u8]) -> Option<[u8; N]> {
    <[u8; N]>::try_from(bytes).ok()
}
