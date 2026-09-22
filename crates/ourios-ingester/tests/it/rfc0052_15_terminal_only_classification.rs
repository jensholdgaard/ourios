//! RFC0052.15 — Only the terminal rotation state is reported
//! server-terminal, client-retryable.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! The classifier tests held on `hold/794-wedged-classification`
//! (`a_quiesced_wal_classifies_as_wedged_not_transient`,
//! `the_append_that_wedged_the_wal_is_also_wedged_not_transient`,
//! `other_append_and_sync_failures_stay_transient`,
//! `an_oversize_batch_is_still_its_own_outcome`) come back rewritten to
//! the terminal-only rule beside `IngestFailure` itself in
//! `receiver/pipeline.rs`, which is the only place the `pub(super)`
//! classification is nameable. What lives here is the half the §5
//! criterion actually pins: both states driven through a **real** WAL
//! whose rotation fails, out to both transports, so the production path
//! cannot erase the class while those unit tests still pass.
//!
//! Every arm asserts the *narrowness* too. Without the ordinary-failure
//! leg the suite passes on a blanket removal of `Retry-After`, which
//! would contradict RFC 0018 §3.2 rather than amend it.
//! `rfc0018_retryable.rs` keeps the transient class's own assertions.

use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsService;
use ourios_config::MinerConfig;
use ourios_ingester::receiver::IngestPipeline;
use ourios_ingester::receiver::grpc::LogsReceiver;
use ourios_ingester::receiver::http::{HttpConfig, router};
use ourios_miner::cluster::MinerCluster;
use ourios_wal::{FrameKind, RotationFaults, RotationSite, Wal, WalConfig};
use prost::Message;
use tonic::Code;
use tower::ServiceExt;

use crate::ingest_support::{coordinator, grpc_request, post_request, request, resource_logs};

const PROTOBUF: &str = "application/x-protobuf";

fn valid_request() -> ExportLogsServiceRequest {
    request(vec![resource_logs("checkout", &["a log line"])])
}

/// A pipeline over a **real** WAL whose segment is full, so the next
/// append rotates, with `site` failing every attempt under `budget`.
///
/// The seam is RFC 0052 §6's (`fault-injection`): the five rotation
/// steps are `fsync`, `create` and `rename` calls a directory
/// permission cannot single out.
fn rotation_failing_pipeline(root: &Path, budget: u32, site: RotationSite) -> IngestPipeline {
    let mut wal = Wal::open(WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 20,
        segment_size_bytes: ourios_wal::MIN_SEGMENT_SIZE_BYTES,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
        rotation_retry_attempts: budget,
        macos_full_fsync: false,
    })
    .expect("open WAL");
    // Fill the segment to exactly `segment_size_bytes`, so the next
    // frame of any size crosses the §6.5 cap and rotates. `sync`
    // reports the segment's length, so the top-up is computed rather
    // than guessed at: a frame that left even a few spare bytes would
    // make the test depend on the encoded request's size.
    wal.append(FrameKind::OtlpBatch, &vec![0xAA; 16 * 1024 * 1024])
        .expect("fill the segment");
    let filled = wal.sync().expect("sync").byte;
    let remaining = ourios_wal::MIN_SEGMENT_SIZE_BYTES - filled;
    let top_up = usize::try_from(remaining).expect("fits usize") - 12;
    wal.append(FrameKind::OtlpBatch, &vec![0xBB; top_up])
        .expect("top the segment up to the cap");
    assert_eq!(
        wal.sync().expect("sync").byte,
        ourios_wal::MIN_SEGMENT_SIZE_BYTES,
        "the segment sits exactly on the rotation cap",
    );
    wal.arm_rotation_faults(RotationFaults::always(site));

    let miner = MinerCluster::new(MinerConfig::default());
    IngestPipeline::new(coordinator(Box::new(wal)), miner)
}

/// The status and `Status` message a gRPC export fails with, plus
/// whether any retry hint rode along.
async fn over_grpc(pipeline: IngestPipeline) -> (Code, String, bool) {
    let status = LogsReceiver::new(Arc::new(pipeline))
        .export(grpc_request(valid_request()))
        .await
        .expect_err("the export fails");
    let hinted = !status.details().is_empty() || status.metadata().get("retry-after").is_some();
    (status.code(), status.message().to_owned(), hinted)
}

/// The same over HTTP, decoding the `google.rpc.Status` body the OTLP
/// spec requires on every 5xx.
async fn over_http(pipeline: IngestPipeline) -> (StatusCode, String, bool) {
    let (status, headers, body) = send_full(
        router(pipeline.into(), &HttpConfig::default()),
        post_request(
            "/v1/logs",
            Some(PROTOBUF),
            None,
            valid_request().encode_to_vec(),
        ),
    )
    .await;
    let decoded =
        tonic_types::Status::decode(body.as_slice()).expect("the 5xx carries a protobuf Status");
    let hinted = headers.contains_key("retry-after") || !decoded.details.is_empty();
    (status, decoded.message, hinted)
}

async fn send_full(router: Router, request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = router.oneshot(request).await.expect("oneshot");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read response body")
        .to_vec();
    (status, headers, bytes)
}

/// Scenario RFC0052.15 — every rotation failure carries `503` / `UNAVAILABLE`.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test]
async fn rfc0052_15_within_budget_and_terminal_both_carry_503_unavailable() {
    // A budget of 3 leaves the first failure inside it; a budget of 1
    // makes the same fault terminal on its first attempt.
    for budget in [3, 1] {
        let tmp = tempfile::TempDir::new().expect("temp");
        let (code, _, _) = over_grpc(rotation_failing_pipeline(
            tmp.path(),
            budget,
            RotationSite::CloseSync,
        ))
        .await;
        assert_eq!(
            code,
            Code::Unavailable,
            "budget {budget}: a non-retryable code would tell the client to drop an unacked batch",
        );

        let tmp = tempfile::TempDir::new().expect("temp");
        let (status, _, _) = over_http(rotation_failing_pipeline(
            tmp.path(),
            budget,
            RotationSite::CloseSync,
        ))
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "budget {budget}");
    }
}

/// Scenario RFC0052.15 — within the budget: transient, message says retrying, no hint.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test]
async fn rfc0052_15_a_failure_within_the_budget_is_transient_and_says_retrying() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (code, message, hinted) = over_grpc(rotation_failing_pipeline(
        tmp.path(),
        3,
        RotationSite::CloseSync,
    ))
    .await;
    assert_eq!(code, Code::Unavailable);
    assert!(
        message.contains("retrying"),
        "a later append genuinely can succeed, and the message says so: {message}",
    );
    assert!(
        !message.contains("terminal"),
        "it must not claim the state the budget has not reached: {message}",
    );
    assert!(
        !hinted,
        "the server schedules no retry of its own: {message}"
    );

    let tmp = tempfile::TempDir::new().expect("temp");
    let (status, message, hinted) = over_http(rotation_failing_pipeline(
        tmp.path(),
        3,
        RotationSite::CloseSync,
    ))
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(message.contains("retrying"), "{message}");
    assert!(!hinted, "no Retry-After on the transient arm either");
}

/// Scenario RFC0052.15 — only the terminal state is server-terminal, client-retryable.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test]
async fn rfc0052_15_only_the_terminal_state_is_server_terminal_client_retryable() {
    for (transport, code_is_unavailable, message, hinted) in [
        {
            let tmp = tempfile::TempDir::new().expect("temp");
            let (code, message, hinted) = over_grpc(rotation_failing_pipeline(
                tmp.path(),
                1,
                RotationSite::CloseSync,
            ))
            .await;
            ("gRPC", code == Code::Unavailable, message, hinted)
        },
        {
            let tmp = tempfile::TempDir::new().expect("temp");
            let (status, message, hinted) = over_http(rotation_failing_pipeline(
                tmp.path(),
                1,
                RotationSite::CloseSync,
            ))
            .await;
            (
                "HTTP",
                status == StatusCode::SERVICE_UNAVAILABLE,
                message,
                hinted,
            )
        },
    ] {
        assert!(
            code_is_unavailable,
            "{transport}: RFC 0018 §3.2's third class keeps the retryable code — the client \
             keeps the batch",
        );
        assert!(
            message.contains("terminal") && message.contains("operator"),
            "{transport}: the message names the state a delay does not fix: {message}",
        );
        assert!(
            !hinted,
            "{transport}: no retry hint — the server cannot predict when the state clears",
        );
    }
}

/// Scenario RFC0052.15 — the terminal state survives the group commit.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §3.3.
///
/// The post-rename site reaches the terminal state through `sync`, so
/// the class has to cross `CommitCoordinator::flush` and be rebuilt by
/// a waiter. That path used to collapse every sync error into a detail
/// string, which erased the class before the classifier ever saw it.
#[tokio::test]
async fn rfc0052_15_a_terminal_sync_failure_keeps_its_class_through_the_group_commit() {
    let tmp = tempfile::TempDir::new().expect("temp");
    // Budget 2: the rotation's own parent fsync spends the first unit
    // and the next `sync`'s discharge spends the last, so the terminal
    // state is raised by `sync`, inside the flush.
    let pipeline = rotation_failing_pipeline(tmp.path(), 2, RotationSite::ParentFsync);

    let (code, message, hinted) = over_grpc_twice(pipeline).await;
    assert_eq!(code, Code::Unavailable);
    assert!(
        message.contains("terminal") && message.contains("operator"),
        "the sync-raised terminal state reaches the client as itself, \
         not as a generic WAL sync failure: {message}",
    );
    assert!(!hinted);
}

/// Export twice against one pipeline, returning the second failure: the
/// first export spends the rotation's own attempt.
async fn over_grpc_twice(pipeline: IngestPipeline) -> (Code, String, bool) {
    let receiver = LogsReceiver::new(Arc::new(pipeline));
    let _ = receiver.export(grpc_request(valid_request())).await;
    let status = receiver
        .export(grpc_request(valid_request()))
        .await
        .expect_err("the second export fails on the discharge");
    let hinted = !status.details().is_empty() || status.metadata().get("retry-after").is_some();
    (status.code(), status.message().to_owned(), hinted)
}

/// Scenario RFC0052.15 — the reclassification is narrow.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test]
async fn rfc0052_15_ordinary_append_and_fsync_failures_stay_transient() {
    use crate::ingest_support::{
        failing_append_pipeline_transient, failing_sync_pipeline, oversize_append_pipeline,
    };

    // An ordinary append I/O failure and an ordinary fsync failure keep
    // the transient class: they never mention the terminal state, and
    // they carry no retry hint either, so the suite cannot be satisfied
    // by removing `Retry-After` everywhere.
    for (label, pipeline) in [
        ("append", failing_append_pipeline_transient()),
        ("sync", failing_sync_pipeline()),
    ] {
        let (code, message, hinted) = over_grpc(pipeline).await;
        assert_eq!(code, Code::Unavailable, "{label}");
        assert!(
            !message.contains("terminal"),
            "{label}: an ordinary I/O failure is not the terminal rotation state: {message}",
        );
        assert!(!hinted, "{label}");
    }

    // And the one append failure a client can fix itself keeps its own
    // non-retryable outcome.
    let (code, _, _) = over_grpc(oversize_append_pipeline()).await;
    assert_eq!(
        code,
        Code::InvalidArgument,
        "an oversize batch is still its own outcome, not a durability class",
    );
    let (status, _, _) = over_http(oversize_append_pipeline()).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}
