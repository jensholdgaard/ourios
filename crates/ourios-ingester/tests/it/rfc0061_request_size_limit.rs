//! RFC 0061 — OTLP request-size limit, the transport arms.
//! See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
//!
//! RFC0061.1 is in `ourios-server/src/config/resolve.rs` (the Helm arm in
//! `deploy/helm/render-tests.sh`), RFC0061.3 in `src/receiver/grpc.rs`,
//! and the `error.type` counter arms of .2, .3 and .6 in
//! `tests/rfc0061_telemetry.rs`, which installs the global meter provider
//! and so cannot share this binary (RFC0028.2).

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use ourios_core::tenant::MAX_TENANT_BYTES;
use ourios_ingester::receiver::MAX_REQUEST_BYTES;
use ourios_ingester::receiver::http::{HttpConfig, router};
use ourios_parquet::Reader;
use prost::Message;

use crate::ingest_support::{
    capturing_pipeline, gzip, pooled_wal_pipeline, post_request, replay_frames, send,
    shared_wal_pipeline,
};
use crate::rfc0061_support::{
    assert_too_large, export_of_size, export_raw, export_that_grows, incompressible, serve,
};

const PROTOBUF: &str = "application/x-protobuf";
const TENANT: &str = "checkout";
/// A small configured limit, so the over-limit arms stay cheap.
const L: usize = 64 * 1024;

/// After a rejection: nothing reached the WAL, and the listener still acks
/// a small Export.
async fn assert_nothing_appended_and_still_serving(
    addr: std::net::SocketAddr,
    captured: &crate::ingest_support::Captured,
) {
    assert!(
        captured.lock().expect("captured").is_empty(),
        "an over-limit request appends no WAL frame"
    );
    export_raw(addr, TENANT, export_of_size(1024, 1), false)
        .await
        .expect("the listener keeps serving");
    assert_eq!(captured.lock().expect("captured").len(), 1);
}

/// Scenario RFC0061.2 (a) — an identity-encoded gRPC Export of `L + 1`
/// bytes is non-retryable `RESOURCE_EXHAUSTED`. tonic's frame-length check
/// answers `OUT_OF_RANGE`; this arm pins the rewrite.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0061_2a_an_identity_export_over_the_limit_is_resource_exhausted() {
    let (pipeline, captured) = capturing_pipeline();
    let (addr, server) = serve(pipeline, L);

    let result = export_raw(addr, TENANT, export_of_size(L + 1, 4), false).await;

    assert_too_large(result, "identity L + 1");
    assert_nothing_appended_and_still_serving(addr, &captured).await;
    server.abort();
}

/// Scenario RFC0061.2 (b) — a gzip gRPC Export whose compressed frame is
/// over `L` is non-retryable `RESOURCE_EXHAUSTED`.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0061_2b_a_gzip_frame_over_the_limit_is_resource_exhausted() {
    let (pipeline, captured) = capturing_pipeline();
    let (addr, server) = serve(pipeline, L);
    // Arbitrary bytes, not an Export: the frame-length check rejects the
    // frame before anything decodes it.
    let body = incompressible(2 * L);
    assert!(gzip(&body).len() > L, "the compressed frame is over L");

    let result = export_raw(addr, TENANT, body, true).await;

    assert_too_large(result, "gzip frame over L");
    assert_nothing_appended_and_still_serving(addr, &captured).await;
    server.abort();
}

/// Scenario RFC0061.2 (c) — a gzip gRPC Export under `L` compressed that
/// decompresses past `L` is non-retryable `RESOURCE_EXHAUSTED`.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0061_2c_a_gzip_export_inflating_past_the_limit_is_resource_exhausted() {
    let (pipeline, captured) = capturing_pipeline();
    let (addr, server) = serve(pipeline, L);
    let body = export_of_size(4 * L, 4);
    assert!(gzip(&body).len() < L, "the compressed frame is under L");

    let result = export_raw(addr, TENANT, body, true).await;

    assert_too_large(result, "gzip inflating past L");
    assert_nothing_appended_and_still_serving(addr, &captured).await;
    server.abort();
}

/// A tenant of the maximum length, so the WAL frame's prefix is as long as
/// it can be.
fn longest_tenant() -> String {
    "t".repeat(MAX_TENANT_BYTES)
}

/// Every `*.parquet` file under `root`.
fn parquet_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "parquet") {
                files.push(path);
            }
        }
    }
    files
}

fn rows_under(root: &Path) -> usize {
    parquet_files(root)
        .iter()
        .map(|path| {
            Reader::open_file(path)
                .expect("open_file")
                .read_all()
                .expect("read_all")
                .len()
        })
        .sum()
}

const AT_LIMIT_RECORDS: usize = 16;

/// Scenario RFC0061.4 — an at-limit gRPC Export is accepted.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0061_4_an_at_limit_grpc_export_is_accepted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = tmp.path().join("store");
    let (pipeline, sink) = pooled_wal_pipeline(&tmp.path().join("wal"), &store, 2);
    let (addr, server) = serve(pipeline.clone(), MAX_REQUEST_BYTES);

    export_raw(
        addr,
        &longest_tenant(),
        export_of_size(MAX_REQUEST_BYTES, AT_LIMIT_RECORDS),
        false,
    )
    .await
    .expect("an Export of exactly the limit is acked");

    server.abort();
    pipeline.quiesce_encodes();
    sink.flush_all();
    assert_eq!(rows_under(&store), AT_LIMIT_RECORDS);
}

/// Scenario RFC0061.4 — an at-limit OTLP/HTTP Export is accepted.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0061_4_an_at_limit_http_export_is_accepted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = tmp.path().join("store");
    let (pipeline, sink) = pooled_wal_pipeline(&tmp.path().join("wal"), &store, 2);
    let mut request = post_request(
        "/v1/logs",
        Some(PROTOBUF),
        None,
        export_of_size(MAX_REQUEST_BYTES, AT_LIMIT_RECORDS),
    );
    request.headers_mut().insert(
        "x-ourios-tenant",
        longest_tenant().parse().expect("ascii tenant"),
    );

    let (status, _) = send(router(pipeline.clone(), &HttpConfig::default()), request).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "an Export of exactly the limit is acked"
    );
    pipeline.quiesce_encodes();
    sink.flush_all();
    assert_eq!(rows_under(&store), AT_LIMIT_RECORDS);
}

/// Scenario RFC0061.5 — HTTP uses the shared limit.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test]
async fn rfc0061_5_http_uses_the_shared_limit() {
    assert_eq!(
        HttpConfig::default().max_body_bytes,
        MAX_REQUEST_BYTES,
        "the HTTP default is the shared limit's default"
    );
    let config = HttpConfig {
        max_body_bytes: L,
        ..HttpConfig::default()
    };
    let over = export_of_size(L + 1, 4);
    let incompressible_over = incompressible(2 * L);
    assert!(gzip(&over).len() <= L, "inflates past L");
    assert!(gzip(&incompressible_over).len() > L, "over L on the wire");
    for (arm, encoding, body) in [
        ("plain", None, over.clone()),
        ("gzip, over after decompression", Some("gzip"), gzip(&over)),
        (
            "gzip, over on the wire",
            Some("gzip"),
            gzip(&incompressible_over),
        ),
    ] {
        let (pipeline, captured) = capturing_pipeline();
        let (status, _) = send(
            router(pipeline, &config),
            post_request("/v1/logs", Some(PROTOBUF), encoding, body),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{arm}");
        assert!(captured.lock().expect("captured").is_empty(), "{arm}");
    }
}

/// RFC0061.6's Export: just under the default limit, and over the WAL
/// frame once re-encoded.
fn grown_export() -> Vec<u8> {
    let body = export_that_grows(MAX_REQUEST_BYTES - 16);
    let reencoded =
        opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest::decode(
            body.as_slice(),
        )
        .expect("a valid Export")
        .encode_to_vec();
    assert!(
        reencoded.len() > ourios_wal::MAX_FRAME_BYTES,
        "re-encoding grows it past the frame ceiling: {} B",
        reencoded.len()
    );
    body
}

/// Scenario RFC0061.6 — a gRPC request that grows on re-encoding is
/// rejected, not failed.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0061_6_a_grpc_export_that_grows_on_reencoding_is_resource_exhausted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let pipeline = shared_wal_pipeline(tmp.path());
    let (addr, server) = serve(pipeline, MAX_REQUEST_BYTES);

    let result = export_raw(addr, TENANT, grown_export(), false).await;

    assert_too_large(result, "grown on re-encoding");
    server.abort();
    let _ = server.await;
    assert!(
        replay_frames(tmp.path()).is_empty(),
        "the WAL holds no frame"
    );
}

/// Scenario RFC0061.6 — an OTLP/HTTP protobuf request that grows on
/// re-encoding is rejected, not failed.
/// See `docs/rfcs/0061-otlp-request-size-limit.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0061_6_an_http_export_that_grows_on_reencoding_is_413() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let pipeline = shared_wal_pipeline(tmp.path());

    let (status, _) = send(
        router(pipeline, &HttpConfig::default()),
        post_request("/v1/logs", Some(PROTOBUF), None, grown_export()),
    )
    .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        replay_frames(tmp.path()).is_empty(),
        "the WAL holds no frame"
    );
}
