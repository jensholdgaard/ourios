//! RFC 0061 — the `error.type = request_too_large` arms of RFC0061.2, .3
//! and .6.
//!
//! Harness-exempt (RFC0028.2, see `tests/README.md`): the arms read
//! `ourios.ingest.batches` through the **process-global** `OTel` meter
//! provider (`init_in_memory`), which cannot share a process with another
//! installer. One test, so the provider is installed exactly once.
//!
//! See `docs/rfcs/0061-otlp-request-size-limit.md` §5.

#[path = "it/ingest_support/mod.rs"]
mod ingest_support;
#[path = "it/rfc0061_support.rs"]
mod rfc0061_support;

use std::sync::Arc;

use axum::http::StatusCode;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use ourios_ingester::receiver::MAX_REQUEST_BYTES;
use ourios_ingester::receiver::http::{HttpConfig, router};

use ingest_support::{
    capturing_pipeline, gzip, oversize_append_pipeline, post_request, send, shared_wal_pipeline,
};
use rfc0061_support::{
    assert_too_large, export_of_size, export_raw, export_that_grows, incompressible, serve,
};

const PROTOBUF: &str = "application/x-protobuf";
const TENANT: &str = "checkout";
const L: usize = 64 * 1024;

/// The exported `ourios.ingest.batches` value for `error.type =
/// request_too_large`.
fn too_large(rms: &[ResourceMetrics]) -> u64 {
    rms.iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .filter(|m| m.name() == ourios_semconv::OURIOS_INGEST_BATCHES)
        .filter_map(|m| match m.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => Some(sum),
            _ => None,
        })
        .flat_map(opentelemetry_sdk::metrics::data::Sum::data_points)
        .filter(|dp| {
            dp.attributes().any(|kv| {
                kv.key.as_str() == "error.type" && kv.value.as_str() == "request_too_large"
            })
        })
        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
        .sum()
}

/// Scenarios RFC0061.2, .3 and .6 — every over-limit rejection counts on
/// `ourios.ingest.batches` with `error.type = request_too_large`.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0061_request_too_large_is_counted() {
    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test-rfc0061");
    let counted = || {
        guard.force_flush().expect("flush");
        let rms = exporter.get_finished_metrics().expect("collect");
        // The exporter keeps every flush; dropping them leaves one
        // cumulative snapshot per read.
        exporter.reset();
        too_large(&rms)
    };
    // Cumulative temporality: each read is the running total.
    assert_eq!(counted(), 0);

    // RFC0061.2 — the three over-limit gRPC arms.
    let (pipeline, _captured) = capturing_pipeline();
    let (addr, server) = serve(pipeline, L);
    let noise = incompressible(2 * L);
    assert!(gzip(&noise).len() > L);
    for (arm, body, compressed) in [
        ("identity L + 1", export_of_size(L + 1, 4), false),
        ("gzip frame over L", noise, true),
        ("gzip inflating past L", export_of_size(4 * L, 4), true),
    ] {
        assert_too_large(export_raw(addr, TENANT, body, compressed).await, arm);
    }
    server.abort();
    assert_eq!(counted(), 3, "RFC0061.2: one count per over-limit arm");

    // RFC0061.3 — `AppendError::TooLarge` through either transport.
    let (addr, server) = serve(Arc::new(oversize_append_pipeline()), L);
    assert_too_large(
        export_raw(addr, TENANT, export_of_size(1024, 1), false).await,
        "TooLarge over gRPC",
    );
    server.abort();
    let (status, _) = send(
        router(Arc::new(oversize_append_pipeline()), &HttpConfig::default()),
        post_request("/v1/logs", Some(PROTOBUF), None, export_of_size(1024, 1)),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(counted(), 5, "RFC0061.3: one count per transport");

    // RFC0061.6 — the re-encoding growth on either transport.
    let grown = export_that_grows(MAX_REQUEST_BYTES - 16);
    let tmp = tempfile::tempdir().expect("tempdir");
    let pipeline = shared_wal_pipeline(tmp.path());
    let (addr, server) = serve(Arc::clone(&pipeline), MAX_REQUEST_BYTES);
    assert_too_large(
        export_raw(addr, TENANT, grown.clone(), false).await,
        "grown over gRPC",
    );
    server.abort();
    let (status, _) = send(
        router(pipeline, &HttpConfig::default()),
        post_request("/v1/logs", Some(PROTOBUF), None, grown),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(counted(), 7, "RFC0061.6: one count per transport");
}
