//! RFC 0061 — OTLP request-size limit, the transport arms.
//! See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it. RFC0061.1 is in
//! `ourios-server/src/config/resolve.rs`, RFC0061.3 in
//! `src/receiver/grpc.rs`, and the `error.type` counter arms of .2, .3
//! and .6 in `tests/rfc0061_telemetry.rs`, which installs the global
//! meter provider and so cannot share this binary (RFC0028.2).

/// Scenario RFC0061.2 (a) — an identity-encoded gRPC Export of `L + 1`
/// bytes is non-retryable `RESOURCE_EXHAUSTED`.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test]
#[ignore = "RFC0061.2 stub — implemented with the gRPC limit and status rewrite"]
async fn rfc0061_2a_an_identity_export_over_the_limit_is_resource_exhausted() {
    todo!(
        "RFC0061.2 (a) — a served gRPC listener with limit L answers an identity \
         Export of L + 1 bytes with RESOURCE_EXHAUSTED (never OUT_OF_RANGE) and no \
         RetryInfo, appends no WAL frame, and keeps serving"
    );
}

/// Scenario RFC0061.2 (b) — a gzip gRPC Export whose compressed frame is
/// over `L` is non-retryable `RESOURCE_EXHAUSTED`.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test]
#[ignore = "RFC0061.2 stub — implemented with the gRPC limit and status rewrite"]
async fn rfc0061_2b_a_gzip_frame_over_the_limit_is_resource_exhausted() {
    todo!(
        "RFC0061.2 (b) — a gzip Export whose compressed frame is over L gets \
         RESOURCE_EXHAUSTED (never OUT_OF_RANGE) and no RetryInfo, appends no WAL \
         frame, and the listener keeps serving"
    );
}

/// Scenario RFC0061.2 (c) — a gzip gRPC Export under `L` compressed that
/// decompresses past `L` is non-retryable `RESOURCE_EXHAUSTED`.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test]
#[ignore = "RFC0061.2 stub — implemented with the gRPC limit and status rewrite"]
async fn rfc0061_2c_a_gzip_export_inflating_past_the_limit_is_resource_exhausted() {
    todo!(
        "RFC0061.2 (c) — a gzip Export under L compressed that decompresses past L \
         gets RESOURCE_EXHAUSTED and no RetryInfo, appends no WAL frame, and the \
         listener keeps serving"
    );
}

/// Scenario RFC0061.4 — an at-limit gRPC Export is accepted.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test]
#[ignore = "RFC0061.4 stub — implemented with the gRPC limit"]
async fn rfc0061_4_an_at_limit_grpc_export_is_accepted() {
    todo!(
        "RFC0061.4 — a minimally encoded gRPC Export of exactly the default limit \
         under a 128-byte tenant is acked without AppendError::TooLarge and its \
         records are readable after flush"
    );
}

/// Scenario RFC0061.4 — an at-limit OTLP/HTTP Export is accepted.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test]
#[ignore = "RFC0061.4 stub — implemented with the shared HTTP default"]
async fn rfc0061_4_an_at_limit_http_export_is_accepted() {
    todo!(
        "RFC0061.4 — a minimally encoded OTLP/HTTP Export of exactly the default \
         limit under a 128-byte tenant is acked without AppendError::TooLarge and \
         its records are readable after flush"
    );
}

/// Scenario RFC0061.5 — HTTP uses the shared limit.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test]
#[ignore = "RFC0061.5 stub — implemented with the shared HTTP limit"]
async fn rfc0061_5_http_uses_the_shared_limit() {
    todo!(
        "RFC0061.5 — with the limit L from receiver.max_request_bytes, an HTTP \
         Export over L gets 413 plain and gzip"
    );
}

/// Scenario RFC0061.6 — a gRPC request that grows on re-encoding is
/// rejected, not failed.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test]
#[ignore = "RFC0061.6 stub — implemented with the gRPC status remap"]
async fn rfc0061_6_a_grpc_export_that_grows_on_reencoding_is_resource_exhausted() {
    todo!(
        "RFC0061.6 — a gRPC Export just under the default limit whose 1 000 records \
         carry a five-byte severity_number = -1 gets RESOURCE_EXHAUSTED without \
         RetryInfo, and the WAL holds no frame from it"
    );
}

/// Scenario RFC0061.6 — an OTLP/HTTP protobuf request that grows on
/// re-encoding is rejected, not failed.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test]
#[ignore = "RFC0061.6 stub — implemented with the shared HTTP default"]
async fn rfc0061_6_an_http_export_that_grows_on_reencoding_is_413() {
    todo!(
        "RFC0061.6 — an OTLP/HTTP protobuf Export just under the default limit \
         whose 1 000 records carry a five-byte severity_number = -1 gets 413, and \
         the WAL holds no frame from it"
    );
}
