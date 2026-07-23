//! RFC 0037 §3.2 acceptance — **RFC0037.3** (unbounded fidelity +
//! observability). A structured (non-string) log body is retained whole,
//! never truncated (`lossy_flag = false`), and its canonical-JSON byte
//! length is observed on the `ourios.miner.structured_body.size` histogram,
//! dimensioned by service. This is the hazard-#2 guard: no cap, observation
//! instead.

use ourios_config::MinerConfig;
use ourios_core::otlp::{
    AnyValue, ArrayValue, Body, KeyValue as OtlpKeyValue, OtlpLogRecord, any_value,
};
use ourios_core::record::SharedRecordSink;
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::MinerCluster;

fn string_av(s: &str) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::StringValue(s.to_string())),
    }
}

fn service_attrs(service: &str) -> Vec<OtlpKeyValue> {
    vec![OtlpKeyValue {
        key: "service.name".to_string(),
        value: Some(string_av(service)),
        ..Default::default()
    }]
}

/// A large structured body: an array of `n` string elements, canonically
/// encoding to many kilobytes — the `gen_ai.input.messages` shape at scale.
fn big_structured_body(n: usize) -> AnyValue {
    let values: Vec<AnyValue> = (0..n)
        .map(|i| {
            string_av(&format!(
                "chat message part number {i} carrying several words of content"
            ))
        })
        .collect();
    AnyValue {
        value: Some(any_value::Value::ArrayValue(ArrayValue { values })),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn rfc0037_3_structured_body_unbounded_fidelity_and_observability() {
    use opentelemetry_sdk::metrics::data::{
        AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
    };

    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");

    let sink = SharedRecordSink::new();
    let mut cluster =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));

    let tenant = TenantId::new("genai-tenant");
    let body_av = big_structured_body(200);
    // The exact canonical JSON the record must retain, and its byte length —
    // what the metric must observe.
    let expected_body = String::from_utf8(
        ourios_core::otlp::canonical::encode_any_value(&body_av)
            .expect("canonical encode is infallible"),
    )
    .expect("canonical JSON is UTF-8");
    let expected_bytes = expected_body.len() as u64;

    let record = OtlpLogRecord {
        tenant_id: tenant.clone(),
        severity_number: 9,
        scope_name: Some("lib.agent".to_string()),
        event_name: Some("gen_ai.client.inference.operation.details".to_string()),
        resource_attributes: service_attrs("checkout"),
        body: Some(Body::Structured(body_av)),
        ..Default::default()
    };
    cluster.ingest(&record);
    guard.force_flush().expect("force_flush succeeds");

    // Fidelity — the structured body is retained whole (byte-for-byte), never
    // truncated, and never flagged lossy.
    let mined = sink.drain();
    assert_eq!(mined.len(), 1, "one record emitted");
    let rec = &mined[0];
    assert_eq!(
        rec.body.as_deref(),
        Some(expected_body.as_str()),
        "the structured body must be retained byte-for-byte, never truncated"
    );
    assert!(
        !rec.lossy_flag,
        "a structured body is never lossy (RFC 0001 §6.1)"
    );
    assert!(
        expected_bytes > 8_000,
        "sanity: the fixture body is genuinely large ({expected_bytes} B)"
    );

    // Observability — the histogram recorded that byte length under
    // ourios.service = checkout.
    let rms = exporter.get_finished_metrics().expect("metrics exported");
    let data = rms
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(ScopeMetrics::metrics)
        .find(|m| m.name() == ourios_semconv::OURIOS_MINER_STRUCTURED_BODY_SIZE)
        .expect("structured_body.size missing from the exported stream")
        .data();
    let AggregatedMetrics::U64(MetricData::Histogram(hist)) = data else {
        panic!("structured_body.size should be a u64 histogram");
    };
    let point = hist
        .data_points()
        .find(|dp| {
            dp.attributes().any(|kv| {
                kv.key.as_str() == ourios_semconv::OURIOS_SERVICE && kv.value.as_str() == "checkout"
            })
        })
        .expect("a data point carrying ourios.service = checkout");
    // Validate the full required attribute set: the registry marks
    // ourios.tenant `required` and ourios.service `recommended`, so both must
    // ride the data point — asserting only service would still pass if tenant
    // were accidentally dropped.
    let has_attr = |key: &str, value: &str| {
        point
            .attributes()
            .any(|kv| kv.key.as_str() == key && kv.value.as_str() == value)
    };
    assert!(
        has_attr(ourios_semconv::OURIOS_TENANT, "genai-tenant"),
        "the required ourios.tenant attribute must ride the data point"
    );
    assert!(
        has_attr(ourios_semconv::OURIOS_SERVICE, "checkout"),
        "the ourios.service attribute must ride the data point"
    );
    assert_eq!(point.count(), 1, "exactly one structured body observed");
    assert_eq!(
        point.sum(),
        expected_bytes,
        "histogram sum equals the canonical-JSON byte length"
    );

    // Byte-scale bucket boundaries (RFC 0037 §3.2): the histogram must use the
    // explicit byte buckets, not the SDK's default ~10 000-max boundaries —
    // otherwise every structured body over ~10 KiB collapses into one +Inf
    // bucket and the size distribution is unreadable. A 16 MiB top boundary is
    // the distinguishing marker.
    let bounds: Vec<f64> = point.bounds().collect();
    assert_eq!(
        bounds.last().copied(),
        Some(16_777_216.0),
        "byte-scale buckets (16 MiB top boundary), not the SDK duration-scale defaults; got {bounds:?}"
    );
    assert!(
        bounds.contains(&1_048_576.0),
        "buckets resolve the MiB range; got {bounds:?}"
    );
}
