//! HTTP transport-error arms of RFC0003.11 — controlled status codes,
//! never a panic, and no `OtlpBatch` frame appended on a rejected
//! request.
//!
//! These cover the HTTP side of RFC0003.11; the gRPC arm (client
//! cancellation) and the full `rfc0003_11` acceptance flip land with the
//! gRPC-listener slice, so `rfc0003_11` stays `#[ignore]`'d until both
//! transports' arms exist.

mod ingest_support;

use axum::http::StatusCode;
use ingest_support::{capturing_pipeline, post_request, send};
use ourios_ingester::receiver::http::{HttpConfig, router};

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
