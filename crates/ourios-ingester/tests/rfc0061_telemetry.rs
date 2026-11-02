//! RFC 0061 — the `error.type = request_too_large` arms of RFC0061.2, .3
//! and .6.
//!
//! Harness-exempt (RFC0028.2, see `tests/README.md`): the arms read
//! `ourios.ingest.batches` through the **process-global** `OTel` meter
//! provider (`init_in_memory`), which cannot share a process with another
//! installer. One test, so the provider is installed exactly once.
//!
//! See `docs/rfcs/0061-otlp-request-size-limit.md` §5.

/// Scenarios RFC0061.2, .3 and .6 — every over-limit rejection counts on
/// `ourios.ingest.batches` with `error.type = request_too_large`.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "RFC0061.2/.3/.6 stub — implemented with the rejection telemetry"]
async fn rfc0061_request_too_large_is_counted() {
    todo!(
        "RFC0061.2 — the three over-limit gRPC arms raise \
         ourios.ingest.batches{{error.type=request_too_large}} by 3; RFC0061.3 — an \
         AppendError::TooLarge through either transport records it; RFC0061.6 — \
         the re-encoding growth on either transport records it"
    );
}
