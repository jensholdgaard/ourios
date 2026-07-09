//! RFC 0030 §5 — TLS/mTLS on the receiver listeners. Every receiver
//! arm is live here — .1/.2/.4/.5/.6/.9; the .3/.7/.8 arms live in the
//! `ourios-server` harness per §6 (.7 asserts a startup warning of the
//! served binary, which only the server crate can spawn).

use std::path::{Path, PathBuf};

use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::LogsServiceServer;
use ourios_ingester::receiver::grpc::LogsReceiver;
use ourios_ingester::receiver::http::{HttpConfig, router};
use ourios_ingester::receiver::tls::{ALPN_GRPC, ALPN_HTTP, TlsMinVersion, TlsSettings};
use ourios_ingester::receiver::tls_serve::{
    LISTENER_HTTP, ReloadingAcceptor, TlsListener, reloading_acceptor, tls_incoming,
};
use prost::Message as _;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};

use crate::ingest_support::{capturing_pipeline, request, resource_logs};

/// Mint a self-signed leaf (its own CA) with `localhost` + `127.0.0.1`
/// SANs — so a tonic client can pin `domain_name("localhost")` and a
/// reqwest client can verify the `127.0.0.1` it dials — and write the
/// cert/key PEM into `dir`. Returns the paths plus the cert PEM (the
/// client's trusted root). No committed key material (RFC 0029 rule).
fn cert_pair(dir: &Path) -> (PathBuf, PathBuf, String) {
    let signed =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("mint a self-signed cert");
    let cert_pem = signed.cert.pem();
    let cert_path = dir.join("server.crt");
    let key_path = dir.join("server.key");
    std::fs::write(&cert_path, &cert_pem).expect("write cert");
    std::fs::write(&key_path, signed.signing_key.serialize_pem()).expect("write key");
    (cert_path, key_path, cert_pem)
}

/// TLS settings for a listener from a freshly minted cert pair.
fn tls_settings(prefix: &str, cert: &Path, key: &Path) -> TlsSettings {
    TlsSettings::from_parts(
        prefix,
        Some(&cert.display().to_string()),
        Some(&key.display().to_string()),
        None,
        None,
        None,
    )
    .expect("valid settings")
    .expect("configured")
}

/// Scenario RFC0030.1 — gRPC ingest over TLS: a TLS client trusting the
/// test CA exports and the batch lands; a plaintext dial of the same
/// port fails at the transport layer, nothing reaching the WAL.
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0030_1_grpc_ingest_over_tls() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (cert, key, cert_pem) = cert_pair(tmp.path());
    let acceptor = tls_settings("receiver.grpc_tls", &cert, &key)
        .acceptor(ALPN_GRPC)
        .map(ReloadingAcceptor::fixed)
        .expect("acceptor");

    let (pipeline, captured) = capturing_pipeline();
    let service = LogsServiceServer::new(LogsReceiver::new(pipeline));
    let incoming = TcpIncoming::bind("127.0.0.1:0".parse().expect("addr")).expect("bind");
    let addr = incoming.local_addr().expect("local addr");
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(tls_incoming(incoming, acceptor))
            .await
    });

    // TLS client trusting the minted cert, SNI pinned to a cert SAN.
    let channel = Endpoint::from_shared(format!("https://{addr}"))
        .expect("endpoint")
        .tls_config(
            ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(cert_pem.as_bytes()))
                .domain_name("localhost"),
        )
        .expect("tls config")
        .connect()
        .await
        .expect("TLS connect");
    let mut client = LogsServiceClient::new(channel);
    client
        .export(tonic::Request::new(request(vec![resource_logs(
            "checkout",
            &["one line"],
        )])))
        .await
        .expect("TLS export");
    assert_eq!(captured.lock().expect("captured").len(), 1);

    // A plaintext client dialling the TLS port fails: the server's
    // handshake sees the cleartext HTTP/2 preface, not a ClientHello.
    let plaintext = LogsServiceClient::connect(format!("http://{addr}")).await;
    let plaintext_export = match plaintext {
        Ok(mut c) => c
            .export(tonic::Request::new(request(vec![resource_logs(
                "checkout",
                &["nope"],
            )])))
            .await
            .map(|_| ()),
        Err(_) => Err(tonic::Status::unavailable("connect failed")),
    };
    assert!(
        plaintext_export.is_err(),
        "plaintext to the TLS port must fail"
    );
    assert_eq!(
        captured.lock().expect("captured").len(),
        1,
        "the plaintext attempt reached nothing",
    );

    server.abort();
}

/// Scenario RFC0030.2 — HTTP ingest over TLS: an HTTPS client trusting
/// the test CA posts a batch and it lands; a plaintext `http://`
/// request to the TLS port fails at the transport layer.
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0030_2_http_ingest_over_tls() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (cert, key, cert_pem) = cert_pair(tmp.path());
    let acceptor = tls_settings("receiver.http_tls", &cert, &key)
        .acceptor(ALPN_HTTP)
        .map(ReloadingAcceptor::fixed)
        .expect("acceptor");

    let (pipeline, captured) = capturing_pipeline();
    let app = router(pipeline, &HttpConfig::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let server = tokio::spawn(async move {
        axum::serve(
            TlsListener::new(listener, acceptor, LISTENER_HTTP),
            app.into_make_service(),
        )
        .await
    });

    let body = request(vec![resource_logs("checkout", &["one line"])]).encode_to_vec();
    let client = reqwest::Client::builder()
        .add_root_certificate(
            reqwest::Certificate::from_pem(cert_pem.as_bytes()).expect("root cert"),
        )
        .build()
        .expect("client");
    let resp = client
        .post(format!("https://127.0.0.1:{}/v1/logs", addr.port()))
        .header("content-type", "application/x-protobuf")
        .body(body.clone())
        .send()
        .await
        .expect("HTTPS post");
    assert!(resp.status().is_success(), "status {}", resp.status());
    assert_eq!(captured.lock().expect("captured").len(), 1);

    // Plaintext `http://` to the TLS port fails the handshake.
    let plaintext = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{}/v1/logs", addr.port()))
        .header("content-type", "application/x-protobuf")
        .body(body)
        .send()
        .await;
    assert!(plaintext.is_err(), "plaintext to the TLS port must fail");
    assert_eq!(
        captured.lock().expect("captured").len(),
        1,
        "the plaintext attempt reached nothing",
    );

    server.abort();
}

/// Regression for the §3.2 stall guard (not a §5 scenario): a client
/// that opens a TCP connection and never sends its `ClientHello` must
/// not block a healthy client from being served. Proven on the HTTP
/// `TlsListener`, whose `accept` drives handshakes concurrently.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_handshake_does_not_block_the_listener() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (cert, key, cert_pem) = cert_pair(tmp.path());
    let acceptor = tls_settings("receiver.http_tls", &cert, &key)
        .acceptor(ALPN_HTTP)
        .map(ReloadingAcceptor::fixed)
        .expect("acceptor");
    let (pipeline, captured) = capturing_pipeline();
    let app = router(pipeline, &HttpConfig::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        axum::serve(
            TlsListener::new(listener, acceptor, LISTENER_HTTP),
            app.into_make_service(),
        )
        .await
    });

    // A stalled client: connect at TCP, then send nothing (no
    // ClientHello). Held open for the duration of the test.
    let _stalled = tokio::net::TcpStream::connect(addr)
        .await
        .expect("stall connect");

    // A healthy client must still be served promptly — well within the
    // 10 s handshake deadline the stalled connection is subject to.
    let body = request(vec![resource_logs("checkout", &["one line"])]).encode_to_vec();
    let client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(cert_pem.as_bytes()).expect("root"))
        .build()
        .expect("client");
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client
            .post(format!("https://127.0.0.1:{}/v1/logs", addr.port()))
            .header("content-type", "application/x-protobuf")
            .body(body)
            .send(),
    )
    .await
    .expect("the healthy client is served despite the stalled one")
    .expect("HTTPS post");
    assert!(resp.status().is_success(), "status {}", resp.status());
    assert_eq!(captured.lock().expect("captured").len(), 1);

    server.abort();
}

/// A static token store + the matching `Bearer` header, mirroring the
/// RFC 0026 ingester tests: the mTLS handshake gates admission, the
/// bearer still binds the tenant.
fn token_store(tenants: &[&str]) -> std::sync::Arc<ourios_core::auth::TokenStore> {
    use ourios_core::auth::{TokenSpec, build_token_store};
    std::sync::Arc::new(
        build_token_store(Some(&[TokenSpec {
            name: Some("edge-collector".to_string()),
            token: Some("tok-edge".to_string()),
            tenants: tenants.iter().map(|t| (*t).to_string()).collect(),
        }]))
        .expect("valid")
        .expect("enabled"),
    )
}

/// Scenario RFC0030.4 — mTLS require-and-verify. With `client_ca_file`
/// set (⇒ `RequireAndVerifyClientCert`) and a static bearer configured:
/// a CA-trusted client cert *plus* a valid bearer is ingested; the same
/// cert with no bearer is rejected `Unauthenticated` (the gRPC status;
/// mTLS composes with, does not replace, bearer auth); a client with no
/// cert, and one with a cert from an
/// untrusted CA, both fail the handshake — reaching neither the handler
/// nor the auth layer.
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn rfc0030_4_mtls_require_and_verify() {
    use ourios_ingester::receiver::AuthResolver;
    use ourios_ingester::receiver::grpc::AuthLayer;

    let tmp = tempfile::TempDir::new().expect("temp");
    let (server_cert, server_key, server_pem) = cert_pair(tmp.path());
    // The trusted client is its own CA: its cert PEM is the
    // `client_ca_file` root the server verifies against.
    let trusted_client =
        rcgen::generate_simple_self_signed(vec!["edge-collector".to_string()]).expect("client");
    let client_ca = tmp.path().join("client-ca.crt");
    std::fs::write(&client_ca, trusted_client.cert.pem()).expect("write client CA");

    let settings = TlsSettings::from_parts(
        "receiver.grpc_tls",
        Some(&server_cert.display().to_string()),
        Some(&server_key.display().to_string()),
        Some(&client_ca.display().to_string()),
        None,
        None,
    )
    .expect("valid")
    .expect("configured");
    let acceptor = ReloadingAcceptor::fixed(settings.acceptor(ALPN_GRPC).expect("acceptor"));

    let (pipeline, captured) = capturing_pipeline();
    let service = LogsServiceServer::new(LogsReceiver::new(pipeline));
    let auth_layer = AuthLayer::new(AuthResolver::static_only(Some(token_store(&["checkout"]))));
    let incoming = TcpIncoming::bind("127.0.0.1:0".parse().expect("addr")).expect("bind");
    let addr = incoming.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .layer(auth_layer)
            .add_service(service)
            .serve_with_incoming(tls_incoming(incoming, acceptor))
            .await
    });

    // A client config presenting `identity`, trusting the server cert.
    let tls_with_identity = |cert_pem: String, key_pem: String| {
        ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(server_pem.as_bytes()))
            .identity(tonic::transport::Identity::from_pem(cert_pem, key_pem))
            .domain_name("localhost")
    };
    let trusted_id = tls_with_identity(
        trusted_client.cert.pem(),
        trusted_client.signing_key.serialize_pem(),
    );

    // (1) Trusted client cert + valid bearer → ingested.
    let channel = Endpoint::from_shared(format!("https://{addr}"))
        .expect("endpoint")
        .tls_config(trusted_id.clone())
        .expect("tls")
        .connect()
        .await
        .expect("mTLS connect");
    let mut client = LogsServiceClient::new(channel);
    let mut req = tonic::Request::new(request(vec![resource_logs("checkout", &["one line"])]));
    req.metadata_mut()
        .insert("authorization", "Bearer tok-edge".parse().expect("md"));
    client.export(req).await.expect("authed mTLS export");
    assert_eq!(captured.lock().expect("captured").len(), 1);

    // (2) Same trusted cert, NO bearer → mTLS admits, bearer rejects.
    let channel = Endpoint::from_shared(format!("https://{addr}"))
        .expect("endpoint")
        .tls_config(trusted_id)
        .expect("tls")
        .connect()
        .await
        .expect("mTLS connect");
    let mut client = LogsServiceClient::new(channel);
    let status = client
        .export(tonic::Request::new(request(vec![resource_logs(
            "checkout",
            &["no bearer"],
        )])))
        .await
        .expect_err("no bearer is rejected even over mTLS");
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    assert_eq!(
        captured.lock().expect("captured").len(),
        1,
        "the request without a bearer reached the auth layer but not the WAL",
    );

    // Connect (tonic defers the handshake) then export, so the mTLS
    // rejection surfaces whether it lands at handshake or first RPC.
    let ingest_attempt = |config: ClientTlsConfig| async move {
        let channel = Endpoint::from_shared(format!("https://{addr}"))?
            .tls_config(config)?
            .connect()
            .await?;
        let mut client = LogsServiceClient::new(channel);
        let mut req = tonic::Request::new(request(vec![resource_logs("checkout", &["x"])]));
        req.metadata_mut()
            .insert("authorization", "Bearer tok-edge".parse().expect("md"));
        client.export(req).await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    };

    // (3) No client cert → rejected, never reaching the handler.
    let no_cert = ingest_attempt(
        ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(server_pem.as_bytes()))
            .domain_name("localhost"),
    )
    .await;
    assert!(no_cert.is_err(), "a client with no cert must be rejected");

    // (4) Cert from an untrusted CA → rejected.
    let untrusted = rcgen::generate_simple_self_signed(vec!["rogue".to_string()]).expect("rogue");
    let wrong_ca = ingest_attempt(tls_with_identity(
        untrusted.cert.pem(),
        untrusted.signing_key.serialize_pem(),
    ))
    .await;
    assert!(
        wrong_ca.is_err(),
        "an untrusted-CA client cert must be rejected"
    );

    assert_eq!(
        captured.lock().expect("captured").len(),
        1,
        "only the fully-authorized request reached the WAL",
    );
    server.abort();
}

/// Scenario RFC0030.5 — config validation. The §3.1 rules live in
/// `TlsSettings::from_parts` / `load` — the single validation path
/// whose error text *is* the startup error (the RFC 0020 §3.1
/// doctrine), so the arms assert against the seam directly.
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[test]
// One scenario, many Given/When/Then arms — splitting it would break the
// RFC0030.5 scenario→test mapping (the rfc0029 dex-test precedent).
#[allow(clippy::too_many_lines)]
fn rfc0030_5_config_validation() {
    // A lone half of the cert/key pair names the missing field.
    let err = TlsSettings::from_parts(
        "receiver.grpc_tls",
        Some("/etc/tls/server.crt"),
        None,
        None,
        None,
        None,
    )
    .expect_err("cert without key");
    assert!(
        err.contains("receiver.grpc_tls.key_file"),
        "names the missing key field: {err}"
    );

    let err = TlsSettings::from_parts(
        "receiver.http_tls",
        None,
        Some("/etc/tls/server.key"),
        None,
        None,
        None,
    )
    .expect_err("key without cert");
    assert!(
        err.contains("receiver.http_tls.cert_file"),
        "names the missing cert field: {err}"
    );

    // client_ca_file without the server pair — mTLS presupposes TLS.
    let err = TlsSettings::from_parts(
        "querier.http_tls",
        None,
        None,
        Some("/etc/tls/ca.crt"),
        None,
        None,
    )
    .expect_err("client CA without a server pair");
    assert!(
        err.contains("querier.http_tls.cert_file") && err.contains("querier.http_tls.key_file"),
        "names the required pair: {err}"
    );

    // min_version accepts only 1.2 / 1.3.
    let err = TlsSettings::from_parts(
        "receiver.grpc_tls",
        Some("/etc/tls/server.crt"),
        Some("/etc/tls/server.key"),
        None,
        Some("1.1"),
        None,
    )
    .expect_err("TLS 1.1 is not implemented");
    assert!(
        err.contains("receiver.grpc_tls.min_version") && err.contains("1.1"),
        "names the field and echoes the value: {err}"
    );

    // reload_interval_secs must be a positive integer.
    for bad in ["0", "-5", "5m"] {
        let err = TlsSettings::from_parts(
            "receiver.grpc_tls",
            Some("/etc/tls/server.crt"),
            Some("/etc/tls/server.key"),
            None,
            None,
            Some(bad),
        )
        .expect_err("non-positive / non-integer reload interval");
        assert!(
            err.contains("receiver.grpc_tls.reload_interval_secs"),
            "names the field for {bad:?}: {err}"
        );
    }

    // All-unset is open (plaintext), not an error — TLS is opt-in.
    assert_eq!(
        TlsSettings::from_parts("receiver.grpc_tls", None, None, None, None, None)
            .expect("all-unset is valid"),
        None
    );

    // An unreadable cert path fails naming the path.
    let missing = TlsSettings::from_parts(
        "receiver.grpc_tls",
        Some("/nonexistent/rfc0030/server.crt"),
        Some("/nonexistent/rfc0030/server.key"),
        None,
        None,
        None,
    )
    .expect("shape-valid settings")
    .expect("configured");
    let err = missing.load().expect_err("unreadable cert file");
    assert!(
        err.contains("/nonexistent/rfc0030/server.crt"),
        "names the path: {err}"
    );

    // A non-PEM cert file fails naming the path.
    let tmp = tempfile::TempDir::new().expect("temp");
    let garbage = tmp.path().join("garbage.crt");
    std::fs::write(&garbage, b"this is not PEM").expect("write garbage");
    let key = tmp.path().join("garbage.key");
    std::fs::write(&key, b"also not PEM").expect("write garbage key");
    let non_pem = TlsSettings::from_parts(
        "receiver.http_tls",
        Some(&garbage.display().to_string()),
        Some(&key.display().to_string()),
        None,
        None,
        None,
    )
    .expect("shape-valid settings")
    .expect("configured");
    let err = non_pem.load().expect_err("non-PEM cert file");
    assert!(
        err.contains(&garbage.display().to_string()),
        "names the path: {err}"
    );

    // An empty client-CA file fails at load, naming the path — never a
    // silently empty trust store.
    let signed = rcgen::generate_simple_self_signed(vec!["localhost".into()])
        .expect("mint a self-signed pair");
    let cert_path = tmp.path().join("server.crt");
    let key_path = tmp.path().join("server.key");
    std::fs::write(&cert_path, signed.cert.pem()).expect("write cert");
    std::fs::write(&key_path, signed.signing_key.serialize_pem()).expect("write key");
    let empty_ca = tmp.path().join("empty-ca.crt");
    std::fs::write(&empty_ca, b"").expect("write empty CA");
    let with_empty_ca = TlsSettings::from_parts(
        "receiver.grpc_tls",
        Some(&cert_path.display().to_string()),
        Some(&key_path.display().to_string()),
        Some(&empty_ca.display().to_string()),
        None,
        None,
    )
    .expect("shape-valid settings")
    .expect("configured");
    let err = with_empty_ca.load().expect_err("empty CA file");
    assert!(
        err.contains(&empty_ca.display().to_string()),
        "names the path: {err}"
    );

    // And a valid pair loads — for both min_version selections.
    for (raw, expected) in [
        (None, TlsMinVersion::V1_2),
        (Some("1.3"), TlsMinVersion::V1_3),
    ] {
        let settings = TlsSettings::from_parts(
            "receiver.grpc_tls",
            Some(&cert_path.display().to_string()),
            Some(&key_path.display().to_string()),
            None,
            raw,
            None,
        )
        .expect("valid settings")
        .expect("configured");
        assert_eq!(settings.min_version, expected);
        settings.load().expect("a valid PEM pair builds");
    }
}

/// Raw TLS handshake to `addr` trusting `roots` (DER), returning the
/// leaf certificate the server presented — the observable that tells
/// which cert generation the listener is serving.
async fn served_leaf(addr: std::net::SocketAddr, roots: &[Vec<u8>]) -> Vec<u8> {
    use tokio_rustls::TlsConnector;
    use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
    use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};

    let mut store = RootCertStore::empty();
    for der in roots {
        store
            .add(CertificateDer::from(der.clone()))
            .expect("trust root");
    }
    let config = ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
    .expect("versions")
    .with_root_certificates(store)
    .with_no_client_auth();
    let connector = TlsConnector::from(std::sync::Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(addr).await.expect("connect");
    // Bound the handshake so a listener regression (stalled handshake)
    // fails the test instead of hanging it.
    let tls = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        connector.connect(ServerName::try_from("localhost").expect("name"), tcp),
    )
    .await
    .expect("handshake completes within the deadline")
    .expect("handshake");
    let (_, session) = tls.get_ref();
    session.peer_certificates().expect("peer certs")[0]
        .as_ref()
        .to_vec()
}

/// Poll `served_leaf` until it returns `expected` (bounded), so the test
/// tracks the reload's own cadence rather than a fixed sleep.
async fn wait_for_leaf(addr: std::net::SocketAddr, roots: &[Vec<u8>], expected: &[u8], why: &str) {
    let deadline = std::time::Duration::from_secs(15);
    let poll = std::time::Duration::from_millis(200);
    let ok = tokio::time::timeout(deadline, async {
        loop {
            if served_leaf(addr, roots).await == expected {
                return;
            }
            tokio::time::sleep(poll).await;
        }
    })
    .await;
    assert!(ok.is_ok(), "{why}");
}

/// Scenario RFC0030.6 — certificate reload: with `reload_interval_secs`
/// set, replacing the cert/key on disk makes new handshakes serve the
/// new certificate without a restart; replacing them with garbage keeps
/// the last good certificate (and logs) rather than taking the listener
/// down.
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0030_6_certificate_reload() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let cert_path = tmp.path().join("server.crt");
    let key_path = tmp.path().join("server.key");
    let mint = || {
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("mint")
    };

    // Generation A on disk; B minted up front so the client can trust
    // both leaves across the swap.
    let gen_a = mint();
    std::fs::write(&cert_path, gen_a.cert.pem()).expect("write A cert");
    std::fs::write(&key_path, gen_a.signing_key.serialize_pem()).expect("write A key");
    let a_der = gen_a.cert.der().as_ref().to_vec();
    let gen_b = mint();
    let b_der = gen_b.cert.der().as_ref().to_vec();
    let roots = [a_der.clone(), b_der.clone()];

    // Reload every second.
    let settings = TlsSettings::from_parts(
        "receiver.http_tls",
        Some(&cert_path.display().to_string()),
        Some(&key_path.display().to_string()),
        None,
        None,
        Some("1"),
    )
    .expect("valid")
    .expect("configured");
    let acceptor = reloading_acceptor(&settings, ALPN_HTTP, LISTENER_HTTP).expect("acceptor");

    let (pipeline, _captured) = capturing_pipeline();
    let app = router(pipeline, &HttpConfig::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        axum::serve(
            TlsListener::new(listener, acceptor, LISTENER_HTTP),
            app.into_make_service(),
        )
        .await
    });

    // Before any reload: generation A.
    assert_eq!(served_leaf(addr, &roots).await, a_der, "serves A initially");

    // Swap to generation B on disk; poll until new handshakes serve B —
    // no restart. Polling tracks the reload cadence rather than a fixed
    // sleep (robust under CI load).
    std::fs::write(&cert_path, gen_b.cert.pem()).expect("write B cert");
    std::fs::write(&key_path, gen_b.signing_key.serialize_pem()).expect("write B key");
    wait_for_leaf(addr, &roots, &b_der, "reloaded to B within the deadline").await;

    // Garbage on disk: the listener keeps serving B. Assert repeatedly
    // over a window that spans several reload ticks, so a race with a
    // reload attempt can't produce a false pass.
    std::fs::write(&cert_path, b"not a certificate").expect("write garbage");
    for _ in 0..8 {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            served_leaf(addr, &roots).await,
            b_der,
            "garbage keeps the last good certificate",
        );
    }

    server.abort();
}

/// Scenario RFC0030.9 — `min_version` enforcement: against a server
/// pinned to TLS 1.3, a client offering only TLS 1.2 is refused and a
/// TLS 1.3 client succeeds. Exercised at the raw `tokio-rustls`
/// handshake layer — a tonic/reqwest client can't be pinned to a single
/// version, and the handshake is exactly what `min_version` governs.
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0030_9_min_version_enforcement() {
    use tokio::io::AsyncWriteExt as _;
    use tokio_rustls::TlsConnector;
    use tokio_rustls::rustls::pki_types::pem::PemObject as _;
    use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
    use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};

    let tmp = tempfile::TempDir::new().expect("temp");
    let (cert, key, cert_pem) = cert_pair(tmp.path());
    // Server accepts TLS 1.3 only.
    let settings = TlsSettings::from_parts(
        "receiver.grpc_tls",
        Some(&cert.display().to_string()),
        Some(&key.display().to_string()),
        None,
        Some("1.3"),
        None,
    )
    .expect("valid")
    .expect("configured");
    // Raw acceptor (this test drives handshakes directly, not through
    // an adapter).
    let acceptor = settings.acceptor(ALPN_GRPC).expect("acceptor");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    // Accept a few connections; each handshake either completes or
    // errors — the server just drives them so the client observes the
    // outcome.
    let server = tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let _ = acceptor.accept(tcp).await;
            });
        }
    });

    // A client trusting the cert, restricted to a single TLS version.
    let connector = |versions: &[&'static rustls::SupportedProtocolVersion]| {
        let mut roots = RootCertStore::empty();
        for c in CertificateDer::pem_slice_iter(cert_pem.as_bytes()).map(|c| c.expect("pem cert")) {
            roots.add(c).expect("add root");
        }
        let config = ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(versions)
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        TlsConnector::from(std::sync::Arc::new(config))
    };
    let domain = ServerName::try_from("localhost").expect("server name");

    // TLS 1.2-only client: refused by the 1.3-only server.
    let tcp = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let refused = connector(&[&rustls::version::TLS12])
        .connect(domain.clone(), tcp)
        .await;
    assert!(refused.is_err(), "a TLS 1.2-only client must be refused");

    // TLS 1.3 client: succeeds.
    let tcp = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let mut accepted = connector(&[&rustls::version::TLS13])
        .connect(domain, tcp)
        .await
        .expect("a TLS 1.3 client handshakes");
    accepted.flush().await.ok();

    server.abort();
}
