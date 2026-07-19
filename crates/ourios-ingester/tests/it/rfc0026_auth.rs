//! RFC 0026 §5 — the ingest-owned scenarios: authentication (`.2`),
//! whole-batch tenant binding before the WAL ack (`.3`), the ingest
//! half of wildcard binding (`.5`), and rejection telemetry/audit
//! (`.7`). The server-owned scenarios (`.1`/`.4`, the query half of
//! `.5`, `.6`) live in `crates/ourios-server/tests/rfc0026_auth.rs`.
//!
//! `.2`/`.3`/`.5` are green. The HTTP arms drive the real router
//! in-process (the `ingest_support` oneshot pattern) over a capturing
//! journal, so "nothing reaches the WAL" is asserted on the journal
//! itself. The gRPC authn arm drives the `AuthInterceptor` directly:
//! its placement (`LogsServiceServer::with_interceptor`) *is* the
//! before-decode guarantee — an interceptor rejection means the inner
//! decoding service never runs — and the binding/status arms drive
//! `LogsReceiver::export` with the extension the interceptor attaches.
//! The remaining `.7` stub is `#[ignore]`d, discharged by the
//! telemetry green slice.

use std::sync::Arc;

use crate::ingest_support::{capturing_pipeline, post_request, request, resource_logs, send};
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsService;
use ourios_core::auth::{TokenSpec, TokenStore, build_token_store};
use ourios_ingester::receiver::AuthResolver;
use ourios_ingester::receiver::grpc::{AuthLayer, LogsReceiver};
use ourios_ingester::receiver::http::{HttpConfig, router};
use ourios_ingester::receiver::{AuthBinding, ReceiveError, authenticate_bearer};
use prost::Message;

/// A store with one token bound to `tenants`.
fn store(tenants: &[&str]) -> Arc<TokenStore> {
    Arc::new(
        build_token_store(Some(&[TokenSpec {
            name: Some("edge-collector".to_string()),
            token: Some("tok-edge".to_string()),
            tenants: tenants.iter().map(|t| (*t).to_string()).collect(),
        }]))
        .expect("valid")
        .expect("enabled"),
    )
}

/// The binding the listeners attach for the `store`'s token.
fn binding(tenants: &[&str]) -> AuthBinding {
    authenticate_bearer(Some(&store(tenants)), Some("Bearer tok-edge"))
        .expect("known token")
        .expect("bound")
}

/// A protobuf-encoded export for one record under each `service`.
fn protobuf_body(services: &[&str]) -> Vec<u8> {
    request(
        services
            .iter()
            .map(|service| resource_logs(service, &["one line"]))
            .collect(),
    )
    .encode_to_vec()
}

/// A `/v1/logs` POST carrying `services`, with an optional bearer.
fn logs_post(services: &[&str], bearer: Option<&str>) -> axum::http::Request<axum::body::Body> {
    let mut request = post_request(
        "/v1/logs",
        Some("application/x-protobuf"),
        None,
        protobuf_body(services),
    );
    if let Some(value) = bearer {
        request.headers_mut().insert(
            axum::http::header::AUTHORIZATION,
            value.parse().expect("header value"),
        );
    }
    request
}

/// Scenario RFC0026.2 — ingest authentication.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0026_2_ingest_authentication() {
    // HTTP: a missing, unknown, or malformed bearer is rejected with 401
    // and nothing reaches the WAL — the journal records no append.
    let (pipeline, captured) = capturing_pipeline();
    let config = HttpConfig {
        auth: AuthResolver::static_only(Some(store(&["checkout"]))),
        ..HttpConfig::default()
    };
    for bearer in [None, Some("Bearer tok-unknown"), Some("Basic dXNlcg==")] {
        let (status, _) = send(
            router(pipeline.clone(), &config),
            logs_post(&["checkout"], bearer),
        )
        .await;
        assert_eq!(
            status,
            axum::http::StatusCode::UNAUTHORIZED,
            "{bearer:?} must be rejected",
        );
    }
    assert!(
        captured.lock().expect("captured").is_empty(),
        "an unauthenticated batch never reaches the WAL",
    );

    // ...and the same request with the configured token is accepted.
    let (status, _) = send(
        router(pipeline.clone(), &config),
        logs_post(&["checkout"], Some("Bearer tok-edge")),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        captured.lock().expect("captured").len(),
        1,
        "the authenticated batch is appended",
    );

    // gRPC: the auth layer — which the server installs on the tonic
    // stack — resolves before the message decode; the same credentials
    // are rejected as one undifferentiated failure, so the decoding
    // service (and everything behind it, WAL included) never runs. The
    // served-stack arm below asserts the wire-level UNAUTHENTICATED.
    let resolver = AuthResolver::static_only(Some(store(&["checkout"])));
    for authorization in [None, Some("Bearer tok-unknown"), Some("Basic dXNlcg==")] {
        assert!(
            resolver.authenticate(authorization).await.is_err(),
            "{authorization:?} must be rejected",
        );
    }

    // A known bearer resolves to the binding the handler enforces with.
    let passed = resolver
        .authenticate(Some("Bearer tok-edge"))
        .await
        .expect("authenticated")
        .expect("bound");
    assert_eq!(passed.token_name(), "edge-collector");
}

/// Scenario RFC0026.2 (served gRPC stack) — the metadata → interceptor →
/// extension → handler handoff over a real socket: the interceptor is
/// installed exactly as the server role installs it
/// (`LogsServiceServer::with_interceptor`), a missing/unknown bearer is
/// rejected before the handler, and a known bearer's batch lands.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0026_2_served_grpc_stack_authenticates() {
    use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;
    use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsServiceServer;

    let (pipeline, captured) = capturing_pipeline();
    let service = LogsServiceServer::new(LogsReceiver::new(pipeline));
    let layer = AuthLayer::new(AuthResolver::static_only(Some(store(&["checkout"]))));
    let incoming =
        tonic::transport::server::TcpIncoming::bind("127.0.0.1:0".parse().expect("addr"))
            .expect("bind");
    let addr = incoming.local_addr().expect("local addr");
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .layer(layer)
            .add_service(service)
            .serve_with_incoming(incoming)
            .await
    });

    let mut client = LogsServiceClient::connect(format!("http://{addr}"))
        .await
        .expect("connect");

    // No bearer → the interceptor rejects before the handler.
    let status = client
        .export(tonic::Request::new(request(vec![resource_logs(
            "checkout",
            &["one line"],
        )])))
        .await
        .expect_err("unauthenticated");
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    assert!(
        captured.lock().expect("captured").is_empty(),
        "nothing reached the WAL",
    );

    // A known bearer authenticates through the metadata and the batch lands.
    let mut authed = tonic::Request::new(request(vec![resource_logs("checkout", &["one line"])]));
    authed
        .metadata_mut()
        .insert("authorization", "Bearer tok-edge".parse().expect("md"));
    client.export(authed).await.expect("authenticated export");
    assert_eq!(captured.lock().expect("captured").len(), 1);

    server.abort();
}

/// Scenario RFC0026.3 — ingest tenant binding.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0026_3_ingest_tenant_binding() {
    let (pipeline, captured) = capturing_pipeline();
    let bound = binding(&["tenant-a", "tenant-b"]);

    // A batch whose derived tenants all fall inside {a, b} acks normally.
    let accepted = pipeline
        .ingest_bound(
            request(vec![
                resource_logs("tenant-a", &["a line"]),
                resource_logs("tenant-b", &["b line"]),
            ]),
            Some(&bound),
            false,
        )
        .await
        .expect("in-set batch acks");
    assert_eq!(accepted, 2);
    assert_eq!(captured.lock().expect("captured").len(), 1);

    // One out-of-set group rejects the WHOLE batch — in-set siblings
    // included — with no WAL append and no partial success.
    let denied = pipeline
        .ingest_bound(
            request(vec![
                resource_logs("tenant-a", &["a line"]),
                resource_logs("tenant-c", &["intruding line"]),
            ]),
            Some(&bound),
            false,
        )
        .await
        .expect_err("out-of-set batch is denied");
    assert!(
        matches!(
            &denied,
            ReceiveError::TenantDenied { token_name, tenant }
                if token_name == "edge-collector" && tenant.as_str() == "tenant-c"
        ),
        "got {denied:?}",
    );
    assert!(
        !denied.to_string().contains("tok-edge"),
        "no token value on the error surface: {denied}",
    );
    assert_eq!(
        captured.lock().expect("captured").len(),
        1,
        "the denied batch appended nothing — no partial acceptance",
    );

    // The transport mappings: 403 over HTTP, PERMISSION_DENIED over gRPC.
    let (status, _) = send(
        router(
            pipeline.clone(),
            &HttpConfig {
                auth: AuthResolver::static_only(Some(store(&["tenant-a", "tenant-b"]))),
                ..HttpConfig::default()
            },
        ),
        logs_post(&["tenant-a", "tenant-c"], Some("Bearer tok-edge")),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);

    let receiver = LogsReceiver::new(pipeline.clone());
    let mut grpc_request = tonic::Request::new(request(vec![resource_logs(
        "tenant-c",
        &["intruding line"],
    )]));
    grpc_request
        .extensions_mut()
        .insert(binding(&["tenant-a", "tenant-b"]));
    let status = receiver.export(grpc_request).await.expect_err("denied");
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
    assert_eq!(
        captured.lock().expect("captured").len(),
        1,
        "neither transport's denial reached the WAL",
    );
}

/// Scenario RFC0026.5 (ingest half) — wildcard binding.
/// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0026_5_wildcard_binding_ingest() {
    let (pipeline, captured) = capturing_pipeline();
    let bound = binding(&["*"]);

    // Arbitrary tenants — including ones no config lists — all ack, as if
    // every tenant were listed.
    for service in ["alpha", "beta", "entirely-new-tenant"] {
        let accepted = pipeline
            .ingest_bound(
                request(vec![resource_logs(service, &["a line"])]),
                Some(&bound),
                false,
            )
            .await
            .unwrap_or_else(|e| panic!("wildcard ingests to {service}: {e}"));
        assert_eq!(accepted, 1);
    }
    assert_eq!(captured.lock().expect("captured").len(), 3);
}

// Scenario RFC0026.7 (rejection telemetry + audit) lives in the dedicated
// `tests/rfc0026_telemetry.rs` binary — it installs the process-global
// OTel meter provider (the RFC0028.2 harness-exemption class).
