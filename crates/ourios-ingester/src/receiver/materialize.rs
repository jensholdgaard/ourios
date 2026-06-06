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
use opentelemetry_proto::tonic::logs::v1::LogRecord;
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
    scope: Option<&InstrumentationScope>,
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
        scope_name: scope.and_then(|s| nonempty(s.name.clone())),
        scope_version: scope.and_then(|s| nonempty(s.version.clone())),
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

/// Narrow proto's `i32` `severity_number` to the schema's `u8`. Valid
/// OTLP severity is `0..=24` (`0` = UNSPECIFIED); any value outside that
/// range — invalid-but-`u8`-representable (`25..=255`), negative, or
/// `> 255` — maps to `0`/UNSPECIFIED, so the `OtlpLogRecord` contract the
/// miner's template key and the Parquet schema rely on holds at this
/// boundary.
fn severity_to_u8(n: i32) -> u8 {
    u8::try_from(n).ok().filter(|v| *v <= 24).unwrap_or(0)
}

/// Proto scalar `0` → `None`, else `Some` — the §6.9-style narrowing of
/// a "0 = unset" wire sentinel.
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
