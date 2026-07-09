//! RFC 0030 §5 — TLS/mTLS on the receiver listeners (six of the nine
//! scenarios; .3/.7/.8 live in the `ourios-server` harness per §6 —
//! .7 asserts a startup warning of the served binary, which only the
//! server crate can spawn).
//!
//! Remaining stubs are `#[ignore]`d so the default run stays green
//! while the RFC is red; each names the green slice that discharges it.

use ourios_ingester::receiver::tls::{TlsMinVersion, TlsSettings};

/// Scenario RFC0030.1 — gRPC ingest over TLS.
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[test]
#[ignore = "RFC0030.1 stub — implemented in the acceptor green slice"]
fn rfc0030_1_grpc_ingest_over_tls() {
    todo!(
        "RFC0030.1 — grpc_tls with a test-CA pair: TLS OTLP export \
         succeeds and is ingested; a plaintext dial of the same port \
         fails at the transport layer, nothing reaching the auth \
         layer or the WAL"
    );
}

/// Scenario RFC0030.2 — HTTP ingest over TLS.
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[test]
#[ignore = "RFC0030.2 stub — implemented in the acceptor green slice"]
fn rfc0030_2_http_ingest_over_tls() {
    todo!(
        "RFC0030.2 — http_tls with a test-CA pair: OTLP/HTTP post \
         over https succeeds and is ingested; a plaintext http \
         request to the same port fails at the transport layer, \
         nothing reaching the auth layer or the WAL"
    );
}

/// Scenario RFC0030.4 — mTLS require-and-verify.
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[test]
#[ignore = "RFC0030.4 stub — implemented in the mTLS green slice"]
fn rfc0030_4_mtls_require_and_verify() {
    todo!(
        "RFC0030.4 — client_ca_file set, valid bearer held constant: \
         CA-signed client cert proceeds through bearer auth and is \
         ingested; no cert and wrong-CA cert fail the handshake, \
         nothing reaching the handler or the auth layer"
    );
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

/// Scenario RFC0030.6 — certificate reload.
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[test]
#[ignore = "RFC0030.6 stub — implemented in the reload green slice"]
fn rfc0030_6_certificate_reload() {
    todo!(
        "RFC0030.6 — reload_interval_secs set: swapped cert/key pair \
         serves new handshakes (peer-cert serial) without restart; \
         garbage files keep the last good config and log an error"
    );
}

/// Scenario RFC0030.9 — `min_version` enforcement.
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[test]
#[ignore = "RFC0030.9 stub — implemented in the acceptor green slice"]
fn rfc0030_9_min_version_enforcement() {
    todo!(
        "RFC0030.9 — min_version 1.3: a TLS 1.2-only handshake is \
         refused; a TLS 1.3 handshake succeeds"
    );
}
