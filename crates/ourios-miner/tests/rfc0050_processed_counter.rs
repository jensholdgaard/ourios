//! Scenario RFC0050.5 (the counting clause) — upstream-template
//! rejections are counted on
//! `ourios.miner.upstream_template.processed`, `error.type` present
//! only on rejection.
//!
//! Standalone binary (RFC0028.2 exemption pattern): the assertion
//! reads the **global** meter via `ourios_telemetry::init_in_memory`,
//! which cannot share a process with another installer.

use ourios_config::{MinerConfig, UpstreamTemplates};
use ourios_core::otlp::{AnyValue, Body, KeyValue, OtlpLogRecord, any_value};
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::MinerCluster;

fn annotated(tenant: &TenantId, body: &str, template: &str) -> OtlpLogRecord {
    OtlpLogRecord {
        tenant_id: tenant.clone(),
        body: Some(Body::String(body.to_string())),
        attributes: vec![KeyValue {
            key: "log.record.template".to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(template.to_string())),
            }),
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// One success (no `error.type`) and one rejection each for
/// `byte_limit`, `grammar` and `template_ceiling` — every path lands
/// on the dedicated processed counter with the documented attribute
/// shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rfc0050_5_rejections_ride_the_processed_counter() {
    use opentelemetry_sdk::metrics::data::{
        AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
    };

    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");
    let config = MinerConfig::default()
        .with_upstream_templates(UpstreamTemplates::Adopt)
        .with_upstream_template_byte_limit(32)
        .with_max_templates(1)
        .expect("non-zero ceiling");
    let mut cluster = MinerCluster::new(config);
    let t = TenantId::new("acme");

    // Adopted (success, no error.type) — and it owns the whole
    // ceiling of 1, arming the template_ceiling arm below.
    cluster.ingest(&annotated(&t, "job 7 finished", "job <*> finished"));
    // Over the 32-byte limit: rejected before tokenisation.
    cluster.ingest(&annotated(
        &t,
        "job 8 finished",
        "job <*> finished with an overlong template tail",
    ));
    // Foreign placeholder syntax: typed grammar refusal.
    cluster.ingest(&annotated(&t, "user alice logged in", "user %s logged in"));
    // Grammatical, aligned, distinct shape — but the ceiling is full.
    cluster.ingest(&annotated(&t, "disk sda failed", "disk <*> failed"));
    guard.force_flush().expect("force_flush succeeds");

    let rms = exporter.get_finished_metrics().expect("metrics exported");
    let AggregatedMetrics::U64(MetricData::Sum(sum)) = rms
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .find(|m| m.name() == ourios_semconv::OURIOS_MINER_UPSTREAM_TEMPLATE_PROCESSED)
        .expect("ourios.miner.upstream_template.processed missing from exported stream")
        .data()
    else {
        panic!("upstream_template.processed should be a u64 sum");
    };

    let value_for = |error_type: Option<&str>| -> u64 {
        sum.data_points()
            .filter(|dp| {
                let mut tenant_ok = false;
                let mut error_attr: Option<String> = None;
                for kv in dp.attributes() {
                    match kv.key.as_str() {
                        k if k == ourios_semconv::OURIOS_TENANT && kv.value.as_str() == "acme" => {
                            tenant_ok = true;
                        }
                        "error.type" => error_attr = Some(kv.value.as_str().into_owned()),
                        _ => {}
                    }
                }
                tenant_ok && error_attr.as_deref() == error_type
            })
            .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
            .sum()
    };

    assert_eq!(value_for(None), 1, "the adoption counted, error.type-free");
    assert_eq!(value_for(Some("byte_limit")), 1);
    assert_eq!(value_for(Some("grammar")), 1);
    assert_eq!(value_for(Some("template_ceiling")), 1);
}
