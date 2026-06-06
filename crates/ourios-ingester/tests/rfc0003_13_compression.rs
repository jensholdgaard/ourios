//! RFC0003.13 — Compression over HTTP: identity and gzip MUST be supported.
//!
//! The same payload sent with `Content-Encoding: identity` and `gzip`
//! decodes to the same request (proving gzip is decompressed, not just
//! accepted); an unsupported encoding is rejected with 415.

mod ingest_support;

use axum::http::StatusCode;
use ingest_support::{capturing_pipeline, gzip, post_request, request, resource_logs, send};
use ourios_ingester::receiver::decode_protobuf;
use ourios_ingester::receiver::http::{HttpConfig, router};
use prost::Message;

const PROTOBUF: &str = "application/x-protobuf";

/// Recover the single `OtlpBatch` payload the pipeline captured.
fn only_captured(captured: &ingest_support::Captured) -> Vec<u8> {
    let frames = captured.lock().expect("captured");
    assert_eq!(frames.len(), 1, "exactly one frame captured");
    frames[0].clone()
}

/// Scenario RFC0003.13 — Compression over HTTP: identity and gzip MUST be supported.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[tokio::test]
async fn rfc0003_13_identity_and_gzip_decode_equally_unsupported_is_415() {
    let export = request(vec![resource_logs("checkout", &["hello"])]);
    let payload = export.encode_to_vec();

    // identity (explicit)
    let (pipeline, captured_identity) = capturing_pipeline();
    let (status, _) = send(
        router(pipeline, &HttpConfig::default()),
        post_request(
            "/v1/logs",
            Some(PROTOBUF),
            Some("identity"),
            payload.clone(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "identity is accepted");

    // gzip
    let (pipeline, captured_gzip) = capturing_pipeline();
    let (status, _) = send(
        router(pipeline, &HttpConfig::default()),
        post_request("/v1/logs", Some(PROTOBUF), Some("gzip"), gzip(&payload)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "gzip is accepted");

    // The two paths decoded to the same request (gzip was decompressed),
    // and that request is the original.
    let from_identity =
        decode_protobuf(&only_captured(&captured_identity)).expect("identity decodes");
    let from_gzip = decode_protobuf(&only_captured(&captured_gzip)).expect("gzip decodes");
    assert_eq!(from_identity, from_gzip, "identity and gzip decode equally");
    assert_eq!(from_identity, export, "and recover the original export");

    // An unsupported encoding → 415, nothing appended.
    let (pipeline, captured_zstd) = capturing_pipeline();
    let (status, _) = send(
        router(pipeline, &HttpConfig::default()),
        post_request("/v1/logs", Some(PROTOBUF), Some("zstd"), payload),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "an unsupported Content-Encoding → 415",
    );
    assert!(
        captured_zstd.lock().expect("captured").is_empty(),
        "a rejected request appends nothing",
    );
}
