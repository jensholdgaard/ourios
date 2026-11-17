use std::sync::{Arc, Mutex, PoisonError};

use axum::body::Body;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsService;
use prost::Message as _;
use tower::ServiceExt as _;

use super::super::grpc::LogsReceiver;
use super::super::http::{HttpConfig, router};
use super::super::pipeline::{Export, ReceiveError, SpanError};
use super::super::selector::TENANT_HEADER;
use super::{Ingest, SharedIngest};

const CREDENTIAL: &str = "Bearer forwarded-token";

/// An [`Ingest`] that records the tenant and credential of each export.
#[derive(Default)]
struct Recording(Mutex<Vec<(String, Option<String>)>>);

#[async_trait::async_trait]
impl Ingest for Recording {
    async fn ingest_reported(
        &self,
        export: Export<'_>,
        credential: Option<&str>,
        _span_error: SpanError,
    ) -> Result<usize, ReceiveError> {
        let mut seen = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        seen.push((
            export.tenant.as_str().to_owned(),
            credential.map(str::to_owned),
        ));
        Ok(0)
    }
}

impl Recording {
    fn seen(&self) -> Vec<(String, Option<String>)> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

fn expected() -> Vec<(String, Option<String>)> {
    vec![("acme".to_owned(), Some(CREDENTIAL.to_owned()))]
}

/// RFC 0065 §6.8: an export a node does not lead is forwarded with the
/// client's credential, so the gRPC transport hands it to the ingest.
#[tokio::test]
async fn the_grpc_transport_hands_on_the_clients_credential() {
    let recording = Arc::new(Recording::default());
    let receiver = LogsReceiver::new(Arc::clone(&recording) as SharedIngest);
    let mut request = tonic::Request::new(ExportLogsServiceRequest::default());
    let metadata = request.metadata_mut();
    metadata.insert(TENANT_HEADER, "acme".parse().expect("tenant"));
    metadata.insert("authorization", CREDENTIAL.parse().expect("credential"));

    receiver.export(request).await.expect("accepted");

    assert_eq!(recording.seen(), expected());
}

/// As for gRPC, the OTLP/HTTP transport hands on the client's credential.
#[tokio::test]
async fn the_http_transport_hands_on_the_clients_credential() {
    let recording = Arc::new(Recording::default());
    let app = router(
        Arc::clone(&recording) as SharedIngest,
        &HttpConfig::default(),
    );
    let request = axum::http::Request::post("/v1/logs")
        .header("content-type", "application/x-protobuf")
        .header(TENANT_HEADER, "acme")
        .header("authorization", CREDENTIAL)
        .body(Body::from(
            ExportLogsServiceRequest::default().encode_to_vec(),
        ))
        .expect("request");

    let response = app.oneshot(request).await.expect("served");

    assert!(response.status().is_success(), "{}", response.status());
    assert_eq!(recording.seen(), expected());
}
