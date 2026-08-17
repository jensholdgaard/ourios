//! RFC0003.11 — Transport-level errors are controlled, not panics.
//!
//! Two transports, two homes:
//! - **HTTP** error arms (malformed 400, bad/missing Content-Type 415,
//!   corrupt gzip 400, oversize 413, wrong path 404, no WAL frame
//!   appended) — `tests/http_transport_errors.rs`.
//! - **gRPC** arms — here: a tenant-resolution failure becomes a
//!   controlled `INVALID_ARGUMENT` `Status` (not a panic), and a valid
//!   request succeeds.
//!
//! On the "gRPC client cancellation mid-decode" arm: `tonic` decodes the
//! request *before* the handler runs, so an in-process direct call can't
//! reproduce a mid-decode cancellation. What this slice guarantees is the
//! testable part — `export` is a plain panic-free `async fn`, and because
//! `ingest` is atomic under the lock (append+fsync then ack), dropping
//! the response future leaves no partial WAL state. The socket-level
//! cancellation path is exercised when a real `tonic` server is served
//! (a follow-up); flagged to the maintainer as an OTLP/tonic nuance
//! rather than faked here.

use crate::ingest_support::{capturing_pipeline, grpc_request, request, resource_logs};
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsService;
use ourios_ingester::receiver::grpc::LogsReceiver;
use tonic::{Code, Request};

/// Scenario RFC0003.11 — Transport-level errors are controlled, not panics;
/// the tenancy arm is now RFC0046.1 (missing selector → `INVALID_ARGUMENT`
/// naming the header, nothing appended).
/// See `docs/rfcs/0003-otlp-receiver.md` §5 and RFC 0046 §5.
#[tokio::test]
async fn rfc0003_11_grpc_missing_selector_is_invalid_argument() {
    // Arrange
    let (pipeline, captured) = capturing_pipeline();
    let receiver = LogsReceiver::new(pipeline);

    // Act: a well-formed export with no `x-ourios-tenant` metadata.
    let status = receiver
        .export(Request::new(request(vec![resource_logs(
            "checkout",
            &["x"],
        )])))
        .await
        .expect_err("an export without a tenant selector is rejected");

    // Assert: a controlled INVALID_ARGUMENT naming the header — not a
    // panic — and nothing appended.
    assert_eq!(status.code(), Code::InvalidArgument);
    let message = status.message();
    assert!(
        message.contains("x-ourios-tenant"),
        "the Status names the selector header, got {message:?}",
    );
    assert!(
        captured.lock().expect("captured").is_empty(),
        "a rejected batch appends no frame",
    );
}

/// Scenario RFC0003.11 — a valid gRPC request succeeds (the handler never
/// panics on either path).
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[tokio::test]
async fn rfc0003_11_grpc_valid_request_succeeds() {
    let (pipeline, _) = capturing_pipeline();
    let receiver = LogsReceiver::new(pipeline);

    let response = receiver
        .export(grpc_request(request(vec![resource_logs(
            "checkout",
            &["x"],
        )])))
        .await;

    assert!(
        response.is_ok(),
        "a valid request exports without error or panic"
    );
}
