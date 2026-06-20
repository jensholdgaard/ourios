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

mod ingest_support;

use std::sync::Arc;

use axum::http::StatusCode;
use ingest_support::{
    capturing_pipeline, failing_append_pipeline_transient, failing_sync_pipeline,
    oversize_append_pipeline, post_request, request, resource_logs, send,
};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsService;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_ingester::receiver::grpc::LogsReceiver;
use ourios_ingester::receiver::http::{HttpConfig, router};
use prost::Message;
use tonic::{Code, Request};

const PROTOBUF: &str = "application/x-protobuf";

/// A resolvable request (has `service.name`) — the ingest itself is what
/// fails (transient WAL), not tenant resolution.
fn valid_request() -> ExportLogsServiceRequest {
    request(vec![resource_logs("checkout", &["a log line"])])
}

/// A request whose only Resource lacks `service.name` → unresolvable tenant
/// (a *permanent* client error).
fn unresolvable_request() -> ExportLogsServiceRequest {
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![KeyValue {
                    key: "host.name".to_owned(),
                    value: Some(AnyValue {
                        value: Some(Value::StringValue("node-1".to_owned())),
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records: vec![LogRecord::default()],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

/// Scenario RFC0018.3 (gRPC) — a transient WAL failure is `UNAVAILABLE`
/// (retryable); a permanent tenant failure is `INVALID_ARGUMENT`.
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[tokio::test]
async fn rfc0018_3_grpc_transient_is_unavailable_permanent_is_invalid_argument() {
    // Transient fsync failure → UNAVAILABLE (retryable), not INTERNAL.
    let sync_fail = LogsReceiver::new(Arc::new(failing_sync_pipeline()))
        .export(Request::new(valid_request()))
        .await
        .expect_err("a WAL-sync failure is reported as an error");
    assert_eq!(
        sync_fail.code(),
        Code::Unavailable,
        "transient WAL-sync failure → retryable UNAVAILABLE, not INTERNAL (RFC 0018 §3.2)",
    );

    // Transient append I/O failure → UNAVAILABLE (retryable) too.
    let append_fail = LogsReceiver::new(Arc::new(failing_append_pipeline_transient()))
        .export(Request::new(valid_request()))
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
        .export(Request::new(valid_request()))
        .await
        .expect_err("an oversize payload is rejected");
    assert_eq!(
        oversize.code(),
        Code::InvalidArgument,
        "oversize payload → non-retryable INVALID_ARGUMENT, not UNAVAILABLE (RFC 0018 §3.2)",
    );

    // Permanent: unresolvable tenant → INVALID_ARGUMENT (unchanged).
    let permanent = LogsReceiver::new(capturing_pipeline().0)
        .export(Request::new(unresolvable_request()))
        .await
        .expect_err("an unresolvable Resource is rejected");
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

    // Permanent: unresolvable tenant → 400 (unchanged).
    let (pipeline, _) = capturing_pipeline();
    let (status, _) = send(
        router(pipeline, &HttpConfig::default()),
        post_request(
            "/v1/logs",
            Some(PROTOBUF),
            None,
            unresolvable_request().encode_to_vec(),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "permanent tenant failure stays 400",
    );
}
