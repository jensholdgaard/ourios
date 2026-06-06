//! RFC0003.14 — Default `/v1/logs` path with configurable override.
//!
//! A `POST` to the default `/v1/logs` is handled; any other path returns
//! 404; and an operator-configured override path replaces `/v1/logs`
//! (the default then 404s) without changing any other behaviour.

mod ingest_support;

use axum::http::StatusCode;
use ingest_support::{capturing_pipeline, post_request, send};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_ingester::receiver::http::{HttpConfig, router};
use prost::Message;

const PROTOBUF: &str = "application/x-protobuf";

/// An empty, valid OTLP/protobuf body — it takes the empty fast path to a
/// 200, so a handled route is observable without needing a tenant.
fn empty_body() -> Vec<u8> {
    ExportLogsServiceRequest::default().encode_to_vec()
}

/// Scenario RFC0003.14 — Default `/v1/logs` path with configurable override.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[tokio::test]
async fn rfc0003_14_default_path_and_configurable_override() {
    // Default config: /v1/logs is handled.
    let (pipeline, _) = capturing_pipeline();
    let (status, _) = send(
        router(pipeline, &HttpConfig::default()),
        post_request("/v1/logs", Some(PROTOBUF), None, empty_body()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the default /v1/logs is handled");

    // An unconfigured path → 404.
    let (pipeline, _) = capturing_pipeline();
    let (status, _) = send(
        router(pipeline, &HttpConfig::default()),
        post_request("/not/the/path", Some(PROTOBUF), None, empty_body()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "an unconfigured path → 404");

    // Operator override: the override path is handled...
    let override_config = HttpConfig {
        path: "/otlp/v1/logs".to_owned(),
        ..HttpConfig::default()
    };
    let (pipeline, _) = capturing_pipeline();
    let (status, _) = send(
        router(pipeline, &override_config),
        post_request("/otlp/v1/logs", Some(PROTOBUF), None, empty_body()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the operator override path is handled"
    );

    // ...and the default path no longer matches under the override.
    let (pipeline, _) = capturing_pipeline();
    let (status, _) = send(
        router(pipeline, &override_config),
        post_request("/v1/logs", Some(PROTOBUF), None, empty_body()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the default path 404s once an override is configured",
    );
}
