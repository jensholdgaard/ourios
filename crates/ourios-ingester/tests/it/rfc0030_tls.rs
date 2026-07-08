//! RFC 0030 §5 — TLS/mTLS on the receiver listeners (seven of the
//! nine scenarios; .3/.8 live in the `ourios-server` harness per §6).
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.

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

/// Scenario RFC0030.5 — config validation.
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[test]
#[ignore = "RFC0030.5 stub — implemented in the config green slice"]
fn rfc0030_5_config_validation() {
    todo!(
        "RFC0030.5 — cert_file without key_file, client_ca_file \
         without a server pair, min_version 1.1, unreadable or \
         non-PEM cert_file: startup fails naming the exact offending \
         field or path"
    );
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

/// Scenario RFC0030.7 — plaintext-auth warning.
/// See `docs/rfcs/0030-tls-mtls-listeners.md` §5.
#[test]
#[ignore = "RFC0030.7 stub — implemented in the config green slice"]
fn rfc0030_7_plaintext_auth_warning() {
    todo!(
        "RFC0030.7 — auth.tokens without a *_tls block: exactly one \
         startup warning naming the listener; with the *_tls block, \
         no warning"
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
