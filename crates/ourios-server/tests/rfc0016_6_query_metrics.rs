//! RFC0016.6 — pruning is observable.
//!
//! A selective query over a multi-row-group corpus returns non-zero
//! `row_groups_pruned` in the response, and the querier emits the OpenTelemetry
//! query metrics (`ourios.query.duration` + `ourios.query.row_groups`, the
//! latter split into the `scanned`/`pruned` states whose sum is the candidate
//! total — the B1 pruned fraction is derived in the backend, per the
//! OpenTelemetry usage/state convention; RFC 0016 §3.6).
//!
//! This test installs a process-global in-memory `MeterProvider`, so it lives
//! in its own integration binary (its own process) and runs single-threaded —
//! mirroring `ourios-ingester`'s `perf_metrics` test.

use std::collections::HashMap;
use std::path::Path;

use axum::body::{Body, to_bytes};
use axum::http::{Request, header};
use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
};
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, Writer};
use ourios_semconv as semconv;
use ourios_server::querier::router;
use tower::ServiceExt;

/// 2026-04-02T10:58:00 UTC — comfortably in the past, so the default look-back
/// window covers it (matching the querier engine's pruning fixtures).
const TS0: u64 = 1_775_127_480_000_000_000;
const HOUR_NS: u64 = 3_600_000_000_000;
/// A look-back wide enough to cover the whole corpus regardless of wall clock,
/// so only the `template_id` predicate — not time — drives pruning.
const HUGE_WINDOW: u64 = 100 * 365 * 24 * 60 * 60 * 1_000_000_000;

fn mined_at(tenant: &str, template_id: u64, ts_ns: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new(tenant),
        template_id,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("lib.cart".to_string()),
        scope_version: Some("1.0.0".to_string()),
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: ts_ns,
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ourios_core::audit::ParamType::Num,
            value: "42".to_string(),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

fn write_records(bucket: &Path, recs: &[MinedRecord]) {
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

fn metric_names(rms: &[ResourceMetrics]) -> Vec<String> {
    rms.iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .map(|m| m.name().to_string())
        .collect()
}

fn metric_data<'a>(rms: &'a [ResourceMetrics], name: &str) -> &'a AggregatedMetrics {
    rms.iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .find(|m| m.name() == name)
        .unwrap_or_else(|| panic!("metric {name} missing"))
        .data()
}

/// Sum the `ourios.query.row_groups` counter restricted to a `state` value.
fn row_groups_in_state(rms: &[ResourceMetrics], state: &str) -> u64 {
    let AggregatedMetrics::U64(MetricData::Sum(sum)) =
        metric_data(rms, semconv::OURIOS_QUERY_ROW_GROUPS)
    else {
        panic!("row_groups should be a u64 sum (counter)");
    };
    sum.data_points()
        .filter(|dp| {
            dp.attributes().any(|kv| {
                kv.key.as_str() == semconv::OURIOS_QUERY_ROW_GROUP_STATE
                    && kv.value.as_str() == state
            })
        })
        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
        .sum()
}

/// Scenario RFC0016.6 — pruning is observable.
/// See `docs/rfcs/0016-query-serving-endpoint.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rfc0016_6_pruning_is_observable() {
    // Arrange — install an in-memory global meter, THEN build the router so its
    // instruments resolve against it. A multi-hour corpus where each file holds
    // a distinct `template_id` (distinct hour ⇒ distinct partition ⇒ distinct
    // file/row group), so a `template_id` predicate prunes the others.
    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");
    let bucket = tempfile::tempdir().unwrap();
    let recs: Vec<MinedRecord> = (0..4)
        .map(|k| mined_at("acme", 1 + k, TS0 + k * HOUR_NS))
        .collect();
    write_records(bucket.path(), &recs);

    let app = router(bucket.path().to_path_buf(), HUGE_WINDOW);
    let request = Request::builder()
        .method("POST")
        .uri("/v1/query")
        .header(header::CONTENT_TYPE, "text/plain")
        .header("X-Ourios-Tenant", "acme")
        .body(Body::from("template_id == 1"))
        .expect("build request");

    // Act
    let response = app.oneshot(request).await.expect("oneshot");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");

    // Assert — the response exposes the pruning win.
    assert_eq!(status, axum::http::StatusCode::OK, "served a 200: {json}");
    assert_eq!(json["rows"], 1, "the one template-1 row matches");
    let pruned = json["stats"]["row_groups_pruned"]
        .as_u64()
        .expect("row_groups_pruned is a number");
    assert!(
        pruned > 0,
        "the other templates' files are pruned by statistics, got {json}",
    );

    // Assert — the OTel query metrics are emitted.
    guard.force_flush().expect("force_flush");
    let rms = exporter.get_finished_metrics().expect("metrics exported");
    let names = metric_names(&rms);
    for expected in [
        semconv::OURIOS_QUERY_DURATION,
        semconv::OURIOS_QUERY_ROW_GROUPS,
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "exported stream missing {expected}, got {names:?}",
        );
    }

    // The duration histogram recorded exactly the one query.
    let AggregatedMetrics::F64(MetricData::Histogram(hist)) =
        metric_data(&rms, semconv::OURIOS_QUERY_DURATION)
    else {
        panic!("query.duration should be an f64 histogram");
    };
    assert_eq!(
        hist.data_points()
            .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::count)
            .sum::<u64>(),
        1,
        "one query → one duration observation",
    );

    // The pruned-state counter matches the response's pruned count (and the
    // scanned/pruned states sum to the candidate total).
    let pruned_metric = row_groups_in_state(&rms, "pruned");
    let scanned_metric = row_groups_in_state(&rms, "scanned");
    assert_eq!(
        pruned_metric, pruned,
        "the pruned-state counter matches the response's row_groups_pruned",
    );
    assert!(scanned_metric >= 1, "at least one row group was scanned");
}
