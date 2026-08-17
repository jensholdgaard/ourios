//! RFC 0018 §5 — RFC0018.3: transient ingest failures are reported retryable.
//!
//! A transient WAL failure — append I/O, post-rotation quiesce, or fsync —
//! is server-side (the batch was not acked, §3.4), so the client SHOULD
//! retry: gRPC `UNAVAILABLE` / HTTP `503` (both retryable per the OTLP
//! failures table), never non-retryable `INTERNAL`/`500` (which would make
//! compliant clients drop data). A *permanent* failure stays non-retryable:
//! tenant resolution → `INVALID_ARGUMENT` / `400`, and an oversize payload
//! (`AppendError::TooLarge`, > 16 MiB) → `INVALID_ARGUMENT` / `413` — retrying
//! the same oversized batch can never succeed.
//!
//! See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5 / §3.2.

use std::sync::Arc;

use crate::ingest_support::{
    capturing_pipeline, failing_append_pipeline_transient, failing_sync_pipeline, grpc_request,
    oversize_append_pipeline, post_request, request, resource_logs, send,
};
use axum::http::StatusCode;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsService;
use ourios_ingester::receiver::grpc::LogsReceiver;
use ourios_ingester::receiver::http::{HttpConfig, router};
use prost::Message;
use tonic::Code;

const PROTOBUF: &str = "application/x-protobuf";

/// A resolvable request (has `service.name`) — the ingest itself is what
/// fails (transient WAL), not tenant resolution.
fn valid_request() -> ExportLogsServiceRequest {
    request(vec![resource_logs("checkout", &["a log line"])])
}

/// A request whose only Resource lacks `service.name` → unresolvable tenant
/// (a *permanent* client error).
/// Scenario RFC0018.3 (gRPC) — a transient WAL failure is `UNAVAILABLE`
/// (retryable); a permanent tenant failure is `INVALID_ARGUMENT`.
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[tokio::test]
async fn rfc0018_3_grpc_transient_is_unavailable_permanent_is_invalid_argument() {
    // Transient fsync failure → UNAVAILABLE (retryable), not INTERNAL.
    let sync_fail = LogsReceiver::new(Arc::new(failing_sync_pipeline()))
        .export(grpc_request(valid_request()))
        .await
        .expect_err("a WAL-sync failure is reported as an error");
    assert_eq!(
        sync_fail.code(),
        Code::Unavailable,
        "transient WAL-sync failure → retryable UNAVAILABLE, not INTERNAL (RFC 0018 §3.2)",
    );

    // Transient append I/O failure → UNAVAILABLE (retryable) too.
    let append_fail = LogsReceiver::new(Arc::new(failing_append_pipeline_transient()))
        .export(grpc_request(valid_request()))
        .await
        .expect_err("a WAL-append I/O failure is reported as an error");
    assert_eq!(
        append_fail.code(),
        Code::Unavailable,
        "transient WAL-append failure → retryable UNAVAILABLE (RFC 0018 §3.2)",
    );

    // Permanent oversize payload (AppendError::TooLarge) → INVALID_ARGUMENT,
    // never retryable: retrying the same oversized batch can't succeed.
    let oversize = LogsReceiver::new(Arc::new(oversize_append_pipeline()))
        .export(grpc_request(valid_request()))
        .await
        .expect_err("an oversize payload is rejected");
    assert_eq!(
        oversize.code(),
        Code::InvalidArgument,
        "oversize payload → non-retryable INVALID_ARGUMENT, not UNAVAILABLE (RFC 0018 §3.2)",
    );

    // Permanent: no tenant selector → INVALID_ARGUMENT (RFC0046.1).
    let permanent = LogsReceiver::new(capturing_pipeline().0)
        .export(tonic::Request::new(request(vec![resource_logs(
            "checkout",
            &["x"],
        )])))
        .await
        .expect_err("an export without a tenant selector is rejected");
    assert_eq!(
        permanent.code(),
        Code::InvalidArgument,
        "permanent tenant failure stays INVALID_ARGUMENT",
    );
}

/// Scenario RFC0018.3 (HTTP) — a transient WAL failure is `503`
/// (retryable); a permanent tenant failure is `400`.
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[tokio::test]
async fn rfc0018_3_http_transient_is_503_permanent_is_400() {
    // Transient fsync failure → 503 (retryable), not 500.
    let (status, _) = send(
        router(failing_sync_pipeline().into(), &HttpConfig::default()),
        post_request(
            "/v1/logs",
            Some(PROTOBUF),
            None,
            valid_request().encode_to_vec(),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "transient WAL-sync failure → retryable 503, not 500 (RFC 0018 §3.2)",
    );

    // Transient append I/O failure → 503 (retryable) too.
    let (status, _) = send(
        router(
            failing_append_pipeline_transient().into(),
            &HttpConfig::default(),
        ),
        post_request(
            "/v1/logs",
            Some(PROTOBUF),
            None,
            valid_request().encode_to_vec(),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "transient WAL-append failure → retryable 503 (RFC 0018 §3.2)",
    );

    // Permanent oversize payload (AppendError::TooLarge) → 413, never the
    // retryable 503: retrying the same oversized body can't succeed.
    let (status, _) = send(
        router(oversize_append_pipeline().into(), &HttpConfig::default()),
        post_request(
            "/v1/logs",
            Some(PROTOBUF),
            None,
            valid_request().encode_to_vec(),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "oversize payload → non-retryable 413, not 503 (RFC 0018 §3.2)",
    );

    // Permanent: no tenant selector → 400 (RFC0046.1).
    let (pipeline, _) = capturing_pipeline();
    let mut no_selector = post_request(
        "/v1/logs",
        Some(PROTOBUF),
        None,
        request(vec![resource_logs("checkout", &["x"])]).encode_to_vec(),
    );
    no_selector.headers_mut().remove("x-ourios-tenant");
    let (status, _) = send(router(pipeline, &HttpConfig::default()), no_selector).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "permanent tenant failure stays 400",
    );
}
