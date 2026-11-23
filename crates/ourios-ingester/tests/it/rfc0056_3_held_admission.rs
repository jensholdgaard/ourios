//! RFC0056.3 — a held tenant's appends are refused, retryably.
//! See `docs/rfcs/0056-audit-durability.md` §5.
//!
//! Driven through a real WAL and both transports: the refusal must come
//! before the append, so replaying the WAL afterwards finds only the
//! other tenant's batches.

use std::sync::Arc;

use axum::http::StatusCode;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsService;
use ourios_config::MinerConfig;
use ourios_core::tenant::TenantId;
use ourios_ingester::held::HeldTenants;
use ourios_ingester::receiver::grpc::LogsReceiver;
use ourios_ingester::receiver::http::{HttpConfig, router};
use ourios_ingester::receiver::{Ingest, IngestPipeline};
use ourios_miner::cluster::MinerCluster;
use ourios_wal::{FrameKind, FrameSink, RecoveryError, TenantBatch, Wal, WalOffset};
use prost::Message;
use tonic::Code;

use crate::ingest_support::{coordinator, request, resource_logs, send, wal_config};

const HELD: &str = "acme";
const OTHER: &str = "globex";

fn export() -> ExportLogsServiceRequest {
    request(vec![resource_logs("checkout", &["a log line"])])
}

/// A pipeline over a real WAL in `root`, with `acme` held.
fn held_pipeline(root: &std::path::Path) -> Arc<IngestPipeline> {
    let wal = Wal::open(wal_config(root)).expect("open WAL");
    let held = HeldTenants::enabled();
    assert!(held.hold(&TenantId::new(HELD)));
    let pipeline = IngestPipeline::new(
        coordinator(Box::new(wal)),
        MinerCluster::new(MinerConfig::default()),
    )
    .with_held_tenants(held);
    Arc::new(pipeline)
}

fn grpc_export(tenant: &str) -> tonic::Request<ExportLogsServiceRequest> {
    let mut request = tonic::Request::new(export());
    request
        .metadata_mut()
        .insert("x-ourios-tenant", tenant.parse().expect("ascii"));
    request
}

fn http_export(tenant: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri("/v1/logs")
        .header("content-type", "application/x-protobuf")
        .header("x-ourios-tenant", tenant)
        .body(axum::body::Body::from(export().encode_to_vec()))
        .expect("build request")
}

/// Scenario RFC0056.3 — a held tenant's appends are refused, retryably.
/// See `docs/rfcs/0056-audit-durability.md` §5.
#[tokio::test]
async fn rfc0056_3_a_held_tenants_appends_are_refused_retryably() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let pipeline = held_pipeline(tmp.path());
    let grpc = LogsReceiver::new(Arc::clone(&pipeline) as Arc<dyn Ingest>);

    let refused = grpc
        .export(grpc_export(HELD))
        .await
        .expect_err("a held tenant's batch is refused");
    assert_eq!(refused.code(), Code::Unavailable, "{}", refused.message());
    assert!(refused.message().contains(HELD), "{}", refused.message());

    let (status, body) = send(
        router(
            Arc::clone(&pipeline) as Arc<dyn Ingest>,
            &HttpConfig::default(),
        ),
        http_export(HELD),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let message = tonic_types::Status::decode(body.as_slice())
        .expect("the 503 carries a protobuf Status")
        .message;
    assert!(message.contains(HELD), "{message}");
    assert_eq!(
        pipeline.acknowledged_durable(),
        None,
        "refused before the WAL append",
    );

    grpc.export(grpc_export(OTHER))
        .await
        .expect("the other tenant's batch is accepted over gRPC");
    let (status, _) = send(
        router(
            Arc::clone(&pipeline) as Arc<dyn Ingest>,
            &HttpConfig::default(),
        ),
        http_export(OTHER),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "and over HTTP");
    assert!(pipeline.acknowledged_durable().is_some());

    drop(grpc);
    drop(pipeline);
    assert_eq!(
        frame_tenants(tmp.path()),
        vec![OTHER, OTHER],
        "only the accepted batches reached the WAL",
    );
}

/// The tenant of every `TenantOtlpBatch` frame in the WAL at `root`, in log
/// order, read back by replay.
fn frame_tenants(root: &std::path::Path) -> Vec<String> {
    #[derive(Default)]
    struct Tenants(Vec<String>);
    impl FrameSink for Tenants {
        fn consume(
            &mut self,
            _: WalOffset,
            kind: FrameKind,
            payload: &[u8],
        ) -> Result<(), RecoveryError> {
            if kind == FrameKind::TenantOtlpBatch {
                let batch = TenantBatch::decode(payload).expect("valid tenant prefix");
                self.0.push(batch.tenant.to_owned());
            }
            Ok(())
        }
    }
    let mut tenants = Tenants::default();
    Wal::open(wal_config(root))
        .expect("reopen WAL")
        .replay(&mut tenants)
        .expect("replay");
    tenants.0
}
