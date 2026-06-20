//! RFC 0018 — OTLP log-spec compliance acceptance scenarios (§5), the
//! receiver + ingest/schema arms (`.1`, `.2`, `.3`, `.6`).
//!
//! **Status: `red`.** Failing stubs that drive the `green` implementation:
//! each encodes one RFC 0018 §5 scenario and currently `todo!()`s. They are
//! `#[ignore]`d so the default `cargo test` (and CI) stays green while the
//! ingest/receiver changes are built — `green` replaces each body with a real
//! assertion and removes the `#[ignore]`.
//!
//! `.4` (`event_name` DSL filter) lives in `ourios-querier` and `.5`
//! (non-finite-double canonical round-trip) in `ourios-core`, next to where
//! their `green` implementations land.
//!
//! See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5/§6.

/// Scenario RFC0018.1 — scope attributes + schema URLs survive ingest→storage:
/// a batch whose `InstrumentationScope` carries `attributes`, whose `ScopeLogs`
/// carries a `schema_url`, and whose `ResourceLogs` carries a `schema_url`
/// round-trips those values through Parquet (`scope_attributes` as canonical JSON).
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[test]
#[ignore = "RFC0018.1 — red until scope_attributes / *_schema_url decode + columns land (green)"]
fn rfc0018_1_scope_fields_round_trip() {
    todo!("RFC0018.1: scope attributes + resource/scope schema_url persist and round-trip")
}

/// Scenario RFC0018.2 — the new columns are OPTIONAL / back-compatible: a
/// historical Parquet file written before this amendment (no `scope_attributes` /
/// `*_schema_url` columns) reads successfully, those fields read absent/NULL, no error.
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[test]
#[ignore = "RFC0018.2 — red until the additive OPTIONAL columns + reader tolerance land (green)"]
fn rfc0018_2_new_columns_back_compatible() {
    todo!("RFC0018.2: pre-amendment files read fine; the three new fields read absent/NULL")
}

/// Scenario RFC0018.3 — transient ingest failure is reported retryable: a WAL
/// append/fsync failure yields a retryable gRPC code (UNAVAILABLE, or
/// `RESOURCE_EXHAUSTED` + `RetryInfo`) and HTTP 503/429 — never `INTERNAL`/500; a
/// permanent failure still maps to `INVALID_ARGUMENT`/400.
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[test]
#[ignore = "RFC0018.3 — red until the transient-vs-permanent error mapping lands (green)"]
fn rfc0018_3_transient_failure_is_retryable() {
    todo!("RFC0018.3: transient -> retryable code; permanent -> INVALID_ARGUMENT/400")
}

/// Scenario RFC0018.6 — out-of-range `SeverityNumber` is preserved, not clamped:
/// `severity_number` 25 / 200 are stored verbatim (never silently clamped to 0),
/// the `ingest.severity_out_of_range` metric increments, and a value a `u8` cannot
/// hold (negative, > 255) maps to 0 + the same anomaly count.
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[test]
#[ignore = "RFC0018.6 — red until severity preserve+flag replaces the clamp-to-0 (green)"]
fn rfc0018_6_out_of_range_severity_preserved() {
    todo!("RFC0018.6: 25/200 preserved + anomaly metric; non-u8 -> 0 (storage invariant)")
}
