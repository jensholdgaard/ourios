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

mod ingest_support;

use ingest_support::{capturing_pipeline, request, resource_logs};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsService;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_ingester::receiver::grpc::LogsReceiver;
use tonic::{Code, Request};

/// A request whose single Resource lacks `service.name` (only `host.name`).
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

/// Scenario RFC0003.11 — Transport-level errors are controlled, not panics.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[tokio::test]
async fn rfc0003_11_grpc_tenant_failure_is_invalid_argument() {
    // Arrange
    let (pipeline, captured) = capturing_pipeline();
    let receiver = LogsReceiver::new(pipeline);

    // Act
    let status = receiver
        .export(Request::new(unresolvable_request()))
        .await
        .expect_err("an unresolvable Resource is rejected");

    // Assert: a controlled INVALID_ARGUMENT naming the attribute — not a
    // panic — and nothing appended.
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("service.name"),
        "the Status names the missing attribute, got {:?}",
        status.message(),
    );
    assert!(
        captured.lock().expect("captured").is_empty(),
        "a rejected batch appends no OtlpBatch frame",
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
        .export(Request::new(request(vec![resource_logs(
            "checkout",
            &["x"],
        )])))
        .await;

    assert!(
        response.is_ok(),
        "a valid request exports without error or panic"
    );
}
