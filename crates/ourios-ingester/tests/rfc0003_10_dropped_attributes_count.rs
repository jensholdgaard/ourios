//! RFC0003.10 — `dropped_attributes_count` preserved verbatim.
//!
//! The receiver reflects the wire-level `dropped_attributes_count` onto
//! the materialised record exactly, and never recomputes it.

use opentelemetry_proto::tonic::logs::v1::LogRecord;
use ourios_core::tenant::TenantId;
use ourios_ingester::receiver::materialize_record;

/// Scenario RFC0003.10 — `dropped_attributes_count` preserved verbatim.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_10_dropped_attributes_count_is_reflected_verbatim() {
    let record = LogRecord {
        dropped_attributes_count: 42,
        ..Default::default()
    };
    let materialized = materialize_record(record, &[], None, TenantId::new("tenant-a"));
    assert_eq!(
        materialized.dropped_attributes_count, 42,
        "dropped_attributes_count is reflected from the wire, never recomputed",
    );
}
