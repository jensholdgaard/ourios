//! Scenarios RFC0045.7 (divergence detector) and RFC0045.9 (watch state
//! bound) through the ingest pipeline: the warning event and the
//! `ourios.receiver.tenant.divergences` counter.
//!
//! Harness-exempt (RFC0028.2, see `tests/README.md`): installs the
//! **process-global** `OTel` meter provider (`init_in_memory`) once, so
//! both scenarios share this binary and read the same exporter.
//!
//! See `docs/rfcs/0045-composite-tenant-derivation.md` §5.

#[path = "it/ingest_support/mod.rs"]
mod ingest_support;

use std::sync::{Arc, Mutex};

use ingest_support::{open_pipeline_with_derivation, request, resource_logs_with_attrs};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use ourios_ingester::receiver::watch::MAX_VALUE_BYTES;
use ourios_ingester::receiver::{TenantDerivation, TenantRule};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;

/// One captured event: its `name` plus stringified fields.
#[derive(Debug, Clone)]
struct Captured {
    name: String,
    fields: Vec<(String, String)>,
}

#[derive(Default)]
struct Fields(Vec<(String, String)>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push((field.name().to_owned(), format!("{value:?}")));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push((field.name().to_owned(), value.to_owned()));
    }
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<Captured>>>);

impl<S: tracing::Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.0.lock().expect("capture lock").push(Captured {
            name: event.metadata().name().to_owned(),
            fields: fields.0,
        });
    }
}

impl Capture {
    fn named(&self, name: &str) -> Vec<Captured> {
        self.0
            .lock()
            .expect("capture lock")
            .iter()
            .filter(|e| e.name == name)
            .cloned()
            .collect()
    }
}

fn field<'a>(event: &'a Captured, key: &str) -> Option<&'a str> {
    event
        .fields
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// The exported `ourios.receiver.tenant.divergences` total for `key`.
fn divergences(rms: &[ResourceMetrics], key: &str) -> u64 {
    rms.iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .filter(|m| m.name() == ourios_semconv::OURIOS_RECEIVER_TENANT_DIVERGENCES)
        .filter_map(|m| match m.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => Some(sum),
            _ => None,
        })
        .flat_map(opentelemetry_sdk::metrics::data::Sum::data_points)
        .filter(|dp| {
            dp.attributes().any(|kv| {
                kv.key.as_str() == ourios_semconv::OURIOS_TENANT_WATCH_KEY
                    && kv.value.as_str() == key
            })
        })
        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
        .sum()
}

fn cluster_group(
    service: &str,
    cluster: &str,
    body: &str,
) -> opentelemetry_proto::tonic::logs::v1::ResourceLogs {
    resource_logs_with_attrs(
        &[("service.name", service), ("k8s.cluster.name", cluster)],
        &[body],
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0045_7_and_9_divergence_detector() {
    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test-rfc0045");
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    scenario_7_detector(&capture).await;
    scenario_9_capacity(&capture).await;

    // The counter: fluxcd (1) + payments (1) on k8s.cluster.name; a (1) on
    // cloud.region.
    guard.force_flush().expect("flush");
    let rms = exporter.get_finished_metrics().expect("collect");
    assert_eq!(divergences(&rms, "k8s.cluster.name"), 2);
    assert_eq!(divergences(&rms, "cloud.region"), 1);
}

/// RFC0045.7 — default rule + default watch: two exports sharing
/// service.name but differing in k8s.cluster.name.
async fn scenario_7_detector(capture: &Capture) {
    let tmp = tempfile::TempDir::new().expect("temp");
    let pipeline = open_pipeline_with_derivation(tmp.path(), &TenantDerivation::default());
    for cluster in ["cluster1", "cluster1", "cluster2"] {
        pipeline
            .ingest_bound(
                request(vec![cluster_group("fluxcd", cluster, "line")]),
                None,
                false,
            )
            .await
            .expect("accepted — the detector never rejects");
    }
    let warnings = capture.named(ourios_semconv::EVENT_OURIOS_RECEIVER_TENANT_DIVERGENCE);
    assert_eq!(
        warnings.len(),
        1,
        "one warning for the first divergent batch: {warnings:?}"
    );
    let w = &warnings[0];
    assert_eq!(field(w, ourios_semconv::OURIOS_TENANT), Some("fluxcd"));
    assert_eq!(
        field(w, ourios_semconv::OURIOS_TENANT_WATCH_KEY),
        Some("k8s.cluster.name")
    );
    assert_eq!(
        field(w, ourios_semconv::OURIOS_TENANT_WATCH_FIRST_VALUE),
        Some("cluster1")
    );
    assert_eq!(
        field(w, ourios_semconv::OURIOS_TENANT_WATCH_VALUE),
        Some("cluster2")
    );

    // Uniform values: a second tenant that never diverges → nothing more.
    for _ in 0..2 {
        pipeline
            .ingest_bound(
                request(vec![cluster_group("checkout", "cluster1", "line")]),
                None,
                false,
            )
            .await
            .expect("accepted");
    }
    // A group lacking the watch key (or empty / non-string) is accepted and
    // not observed.
    pipeline
        .ingest_bound(
            request(vec![resource_logs_with_attrs(
                &[("service.name", "fluxcd")],
                &["line"],
            )]),
            None,
            false,
        )
        .await
        .expect("accepted without the watch key");
    pipeline
        .ingest_bound(
            request(vec![resource_logs_with_attrs(
                &[("service.name", "fluxcd"), ("k8s.cluster.name", "")],
                &["line"],
            )]),
            None,
            false,
        )
        .await
        .expect("accepted with an empty watch value");
    assert_eq!(
        capture
            .named(ourios_semconv::EVENT_OURIOS_RECEIVER_TENANT_DIVERGENCE)
            .len(),
        1,
        "uniform / absent / empty values add no warning"
    );

    // A value longer than 128 bytes is truncated at a UTF-8 boundary with `…`.
    let long = format!("{}é{}", "x".repeat(MAX_VALUE_BYTES - 4), "tail");
    pipeline
        .ingest_bound(
            request(vec![cluster_group("payments", "c1", "line")]),
            None,
            false,
        )
        .await
        .expect("accepted");
    pipeline
        .ingest_bound(
            request(vec![cluster_group("payments", &long, "line")]),
            None,
            false,
        )
        .await
        .expect("accepted");
    let warnings = capture.named(ourios_semconv::EVENT_OURIOS_RECEIVER_TENANT_DIVERGENCE);
    let payments = warnings
        .iter()
        .find(|w| field(w, ourios_semconv::OURIOS_TENANT) == Some("payments"))
        .expect("payments diverged");
    assert_eq!(
        field(payments, ourios_semconv::OURIOS_TENANT_WATCH_VALUE),
        Some(format!("{}…", "x".repeat(MAX_VALUE_BYTES - 4)).as_str())
    );
}

/// RFC0045.9 — `watch_capacity: 1`; two tenants each later diverge.
async fn scenario_9_capacity(capture: &Capture) {
    let tmp2 = tempfile::TempDir::new().expect("temp");
    let bounded = open_pipeline_with_derivation(
        tmp2.path(),
        &TenantDerivation {
            rule: TenantRule::service_name(),
            watch: vec!["cloud.region".to_owned()],
            watch_capacity: 1,
        },
    );
    let region = |service: &str, region: &str| {
        request(vec![resource_logs_with_attrs(
            &[("service.name", service), ("cloud.region", region)],
            &["line"],
        )])
    };
    for (service, r) in [("a", "eu"), ("b", "eu"), ("a", "us"), ("b", "us")] {
        bounded
            .ingest_bound(region(service, r), None, false)
            .await
            .expect("every export is accepted");
    }
    let saturated = capture.named(ourios_semconv::EVENT_OURIOS_RECEIVER_TENANT_WATCH_SATURATED);
    assert_eq!(saturated.len(), 1, "saturation announced exactly once");
    let region_warnings: Vec<_> = capture
        .named(ourios_semconv::EVENT_OURIOS_RECEIVER_TENANT_DIVERGENCE)
        .into_iter()
        .filter(|w| field(w, ourios_semconv::OURIOS_TENANT_WATCH_KEY) == Some("cloud.region"))
        .collect();
    assert_eq!(
        region_warnings.len(),
        1,
        "only the admitted tenant is watched"
    );
    assert_eq!(
        field(&region_warnings[0], ourios_semconv::OURIOS_TENANT),
        Some("a")
    );
}
