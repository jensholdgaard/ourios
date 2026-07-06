//! HTTP transport-error arms of RFC0003.11 — controlled status codes,
//! never a panic, and no `OtlpBatch` frame appended on a rejected
//! request.
//!
//! These cover the HTTP side of RFC0003.11; the gRPC arm (client
//! cancellation) and the full `rfc0003_11` acceptance flip land with the
//! gRPC-listener slice, so `rfc0003_11` stays `#[ignore]`'d until both
//! transports' arms exist.

use crate::ingest_support::{capturing_pipeline, gzip, post_request, send};
use axum::http::StatusCode;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_ingester::receiver::http::{HttpConfig, router};
use prost::Message;

const PROTOBUF: &str = "application/x-protobuf";

/// Send `request` and assert the controlled `expected` status and that
/// nothing was appended to the WAL.
async fn assert_rejected(request: axum::http::Request<axum::body::Body>, expected: StatusCode) {
    let (pipeline, captured) = capturing_pipeline();
    let (status, _) = send(router(pipeline, &HttpConfig::default()), request).await;
    assert_eq!(status, expected);
    assert!(
        captured.lock().expect("captured").is_empty(),
        "a rejected request appends no OtlpBatch frame",
    );
}

#[tokio::test]
async fn malformed_protobuf_is_400() {
    // 0xFF leads with field 31 / wire-type 7 (invalid) — prost rejects it.
    assert_rejected(
        post_request(
            "/v1/logs",
            Some(PROTOBUF),
            None,
            vec![0xff, 0xff, 0xff, 0xff],
        ),
        StatusCode::BAD_REQUEST,
    )
    .await;
}

#[tokio::test]
async fn unrecognised_content_type_is_415() {
    assert_rejected(
        post_request("/v1/logs", Some("text/plain"), None, b"hello".to_vec()),
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
    )
    .await;
}

#[tokio::test]
async fn missing_content_type_is_415() {
    assert_rejected(
        post_request("/v1/logs", None, None, b"hello".to_vec()),
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
    )
    .await;
}

#[tokio::test]
async fn corrupt_gzip_is_400() {
    assert_rejected(
        post_request(
            "/v1/logs",
            Some(PROTOBUF),
            Some("gzip"),
            b"not a gzip stream".to_vec(),
        ),
        StatusCode::BAD_REQUEST,
    )
    .await;
}

#[tokio::test]
async fn wrong_path_is_404() {
    assert_rejected(
        post_request("/not/the/path", Some(PROTOBUF), None, Vec::new()),
        StatusCode::NOT_FOUND,
    )
    .await;
}

#[tokio::test]
async fn gzip_decompression_bomb_is_413() {
    // Compresses small (under the body limit) but inflates well past it —
    // DefaultBodyLimit bounds only the compressed bytes, so the
    // decompressed cap must reject this.
    let config = HttpConfig {
        max_body_bytes: 4096,
        ..HttpConfig::default()
    };
    let bomb = gzip(&vec![0u8; 1_000_000]);
    assert!(
        bomb.len() < config.max_body_bytes,
        "the compressed bomb is under the body limit"
    );
    let (pipeline, captured) = capturing_pipeline();
    let (status, _) = send(
        router(pipeline, &config),
        post_request("/v1/logs", Some(PROTOBUF), Some("gzip"), bomb),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "a gzip body inflating past the cap → 413",
    );
    assert!(captured.lock().expect("captured").is_empty());
}

#[tokio::test]
async fn content_type_and_encoding_are_case_insensitive() {
    // Uppercased media type + encoding token must still be accepted (HTTP
    // media types / Content-Encoding tokens are case-insensitive). An
    // empty request takes the fast path to 200.
    let payload = ExportLogsServiceRequest::default().encode_to_vec();
    let (pipeline, _) = capturing_pipeline();
    let (status, _) = send(
        router(pipeline, &HttpConfig::default()),
        post_request(
            "/v1/logs",
            Some("Application/X-Protobuf"),
            Some("GZIP"),
            gzip(&payload),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "uppercased Content-Type and Content-Encoding are accepted",
    );
}

#[tokio::test]
async fn oversize_body_is_413() {
    let config = HttpConfig {
        max_body_bytes: 16,
        ..HttpConfig::default()
    };
    let (pipeline, captured) = capturing_pipeline();
    let (status, _) = send(
        router(pipeline, &config),
        post_request("/v1/logs", Some(PROTOBUF), None, vec![0u8; 1024]),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        captured.lock().expect("captured").is_empty(),
        "an oversize request appends no OtlpBatch frame",
    );
}

#[tokio::test]
async fn non_post_to_logs_path_is_405() {
    // A wrong *method* on the logs path is 405 (axum's MethodRouter
    // default) — ecosystem-consistent with the Collector, vs the 404 a
    // wrong *path* gets. Confirmed against the OTLP review.
    let (pipeline, captured) = capturing_pipeline();
    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/v1/logs")
        .body(axum::body::Body::empty())
        .expect("build GET request");
    let (status, _) = send(router(pipeline, &HttpConfig::default()), request).await;
    assert_eq!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "a non-POST to /v1/logs is 405, not 404",
    );
    assert!(
        captured.lock().expect("captured").is_empty(),
        "a rejected-method request appends no OtlpBatch frame",
    );
}
