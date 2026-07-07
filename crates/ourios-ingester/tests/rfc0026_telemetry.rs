//! Scenario RFC0026.7 — rejection telemetry and audit.
//!
//! Harness-exempt (RFC0028.2, see `tests/README.md`): the telemetry arm
//! installs the **process-global** `OTel` meter provider
//! (`init_in_memory`), which cannot share a process with another
//! installer. One test, so the provider is installed exactly once.
//!
//! See `docs/rfcs/0026-authentication-tenant-binding.md` §5.

#[path = "it/ingest_support/mod.rs"]
mod ingest_support;

use std::sync::Arc;

use ingest_support::{request, resource_logs};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use ourios_core::audit::{AuditPayload, SharedAuditSink};
use ourios_core::auth::{TokenSpec, build_token_store};
use ourios_ingester::receiver::AuthResolver;
use ourios_ingester::receiver::grpc::AuthLayer;
use ourios_ingester::receiver::{ReceiveError, authenticate_bearer};

/// The exported `ourios.ingest.batches` datapoint value for `error.type ==
/// wanted`, across all resource metrics.
fn rejected_batches(rms: &[ResourceMetrics], wanted: &str) -> u64 {
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
            dp.attributes()
                .any(|kv| kv.key.as_str() == "error.type" && kv.value.as_str() == wanted)
        })
        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
        .sum()
}

/// RFC0026.7 — an authn rejection and an authz rejection each increment
/// the existing `ourios.ingest.batches` counter with the matching
/// `error.type`; the authz rejection emits an `ingest_denied` audit event
/// carrying the token's audit label and the offending tenant; and no
/// token value appears on any surface (metric attributes, audit event,
/// error text).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0026_7_rejection_telemetry_and_audit() {
    let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test-rfc0026-7");

    let store = build_token_store(Some(&[TokenSpec {
        name: Some("edge-collector".to_string()),
        token: Some("tok-secret-edge".to_string()),
        tenants: vec!["tenant-a".to_string()],
    }]))
    .expect("valid")
    .expect("enabled");

    // Authn rejection through the gRPC auth layer (the transport's own
    // rejection surface — UNAUTHENTICATED / grpc-status 16; HTTP's is the
    // 401): error.type = unauthenticated. The layer
    // answers a bearer-less request itself with a trailers-only
    // UNAUTHENTICATED response — the inner service never runs.
    let layer = AuthLayer::new(AuthResolver::static_only(Some(Arc::new(store.clone()))));
    let service = tower::Layer::layer(
        &layer,
        tower::service_fn(|_req: http::Request<()>| async {
            panic!("the inner service must not run for a rejected request");
            #[allow(unreachable_code)]
            Ok::<http::Response<tonic::body::Body>, std::convert::Infallible>(http::Response::new(
                tonic::body::Body::default(),
            ))
        }),
    );
    let response = tower::ServiceExt::oneshot(service, http::Request::new(()))
        .await
        .expect("infallible");
    assert_eq!(
        response
            .headers()
            .get("grpc-status")
            .and_then(|v| v.to_str().ok()),
        Some((tonic::Code::Unauthenticated as i32).to_string().as_str()),
        "trailers-only grpc-status 16"
    );

    // Authz rejection through the pipeline: error.type = permission_denied
    // + the ingest_denied audit event.
    let audit = SharedAuditSink::new();
    let (pipeline, captured) =
        ingest_support::capturing_pipeline_with_denial_audit(Box::new(audit.clone()));
    let binding = authenticate_bearer(Some(&store), Some("Bearer tok-secret-edge"))
        .expect("known token")
        .expect("bound");
    let denied = pipeline
        .ingest_bound(
            request(vec![resource_logs("tenant-b", &["intruding line"])]),
            Some(&binding),
        )
        .await
        .expect_err("out-of-set");
    assert!(matches!(&denied, ReceiveError::TenantDenied { .. }));
    assert!(
        captured.lock().expect("captured").is_empty(),
        "nothing reached the WAL",
    );

    // The audit event: offending tenant on the envelope, token label in
    // the payload, never the token value.
    let events = audit.drain();
    assert_eq!(events.len(), 1, "exactly one ingest_denied event");
    assert_eq!(events[0].tenant_id.as_str(), "tenant-b");
    match &events[0].payload {
        AuditPayload::IngestDenied { token_name } => {
            assert_eq!(token_name, "edge-collector");
        }
        other => panic!("expected IngestDenied, got {other:?}"),
    }
    let rendered = format!("{events:?}{denied}");
    assert!(
        !rendered.contains("tok-secret-edge"),
        "no token value on any surface: {rendered}",
    );

    // The metric stream: both rejections on the existing request counter,
    // attributed by error.type.
    guard.force_flush().expect("flush");
    let rms = exporter.get_finished_metrics().expect("collect");
    assert_eq!(
        rejected_batches(&rms, "unauthenticated"),
        1,
        "authn rejection counted",
    );
    assert_eq!(
        rejected_batches(&rms, "permission_denied"),
        1,
        "authz rejection counted",
    );
}
