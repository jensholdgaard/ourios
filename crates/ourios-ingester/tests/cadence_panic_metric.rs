//! A panicked cadence-sweep step is countable, and distinguishable from a
//! store error on the same counter (#791).
//!
//! Before this, a panic in the age sweep's step retired the flush cadence
//! for the life of the process with no log, no counter, and a `JoinHandle`
//! that still joined cleanly — so the only eventual symptom was an OOM
//! kill with no trail back to the cause.
//!
//! Its own test binary, like `perf_metrics.rs`: `SinkMetrics` resolves
//! through the **global** meter, and two global-installing tests in one
//! binary would race.

use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics, SumDataPoint,
};
use ourios_core::record::{BodyKind, MinedRecord, RecordSink};
use ourios_core::tenant::TenantId;
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_parquet::store::Store;
use ourios_semconv as semconv;
use std::time::Duration;

/// Sum of the named u64 counter's datapoints, optionally restricted to
/// those carrying `attribute = value`.
fn counter_sum(rms: &[ResourceMetrics], name: &str, attribute: Option<(&str, &str)>) -> u64 {
    let metric = rms
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .find(|m| m.name() == name)
        .unwrap_or_else(|| panic!("metric {name} missing from the exported stream"));
    let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
        panic!("{name} should be a u64 sum");
    };
    sum.data_points()
        .filter(|dp| match attribute {
            None => true,
            Some((key, value)) => dp
                .attributes()
                .any(|kv| kv.key.as_str() == key && kv.value.as_str() == value),
        })
        .map(SumDataPoint::value)
        .sum()
}

/// Set a directory's Unix mode.
fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .expect("set bucket permissions");
}

/// One record, enough to give a partition something to fail to flush.
fn a_record() -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("cadence-panic"),
        template_id: 0,
        template_version: 0,
        severity_number: 9,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: 1_750_000_000_000_000_000,
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: Vec::new(),
        separators: Vec::new(),
        body: Some("line".to_string()),
        confidence: 0.0,
        lossy_flag: true,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_cadence_panic_is_counted_and_tagged_apart_from_a_store_error() {
    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");

    // A real `SharedParquetSink`, so what is asserted is the wiring the age
    // sweep calls rather than `SinkMetrics` in isolation.
    let bucket = tempfile::TempDir::new().expect("bucket dir");
    let mut sink = SharedParquetSink::new(ParquetRecordSink::new(
        Store::local(bucket.path()).expect("local store"),
        FlushConfig {
            target_bytes: usize::MAX,
            max_buffer_age: Duration::from_secs(86_400),
            ceiling_bytes: usize::MAX,
        },
    ));

    // A genuine store failure, for the *untagged* datapoint the tagged one
    // has to be distinguishable from: buffer a record, then make the bucket
    // directory read-only so the flush's write cannot land (removing it is
    // not enough — the local backend recreates missing parents). The buffer
    // is retained (the WAL is the durability of record) and the failure is
    // counted.
    sink.emit(a_record());
    set_mode(bucket.path(), 0o555);
    sink.flush_all();
    // Restore before the TempDir drops, or its own cleanup fails.
    set_mode(bucket.path(), 0o755);

    sink.record_cadence_panic();
    guard.force_flush().expect("force_flush");

    let rms = exporter.get_finished_metrics().expect("metrics exported");
    // The dimension is the whole point: a dead age sweep and a transient
    // store failure share this counter, and an operator has to be able to
    // alert on one without the other.
    assert_eq!(
        counter_sum(
            &rms,
            semconv::OURIOS_SINK_FLUSH_ERRORS,
            Some(("error.type", "cadence_panic")),
        ),
        1,
        "the cadence panic is counted as error.type=cadence_panic",
    );
    assert_eq!(
        counter_sum(&rms, semconv::OURIOS_SINK_FLUSH_ERRORS, None),
        2,
        "the store error rides the same counter, so the total is both — if \
         this is 1 the store flush silently succeeded and the distinction \
         below is untested",
    );
    assert_eq!(
        counter_sum(
            &rms,
            semconv::OURIOS_SINK_FLUSH_ERRORS,
            Some(("error.type", "cadence_panic")),
        ) + 1,
        counter_sum(&rms, semconv::OURIOS_SINK_FLUSH_ERRORS, None),
        "exactly one of the two carries the cadence_panic dimension: the \
         store error must stay untagged, or alerting on a dead sweep would \
         fire on every transient store blip",
    );
}
