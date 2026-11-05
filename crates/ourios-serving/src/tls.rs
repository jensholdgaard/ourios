//! RFC 0030 §3.1/§3.2 — validated TLS listener settings.
//!
//! One seam for every listener (receiver gRPC/HTTP and the querier
//! surface): [`TlsSettings::from_parts`] is the single validation path
//! for a `*_tls` config block (the RFC 0020 §3.1 doctrine — the error
//! text these functions produce *is* the startup error, whichever
//! front-end supplied the values), and [`TlsSettings::load`] turns the
//! validated settings into a `rustls::ServerConfig` at startup, so an
//! unreadable or malformed PEM fails fast naming the path.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustls::RootCertStore;
use rustls::server::WebPkiClientVerifier;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

/// A config leaf: trimmed, with empty-after-trim treated as unset (the
/// same normalisation the other `build_*` validators apply).
fn present(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|s| !s.is_empty())
}

/// `min_version` (RFC 0030 §3.1): TLS 1.0/1.1 are not implemented
/// (rustls does not ship them; the Collector deprecates them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsMinVersion {
    #[default]
    V1_2,
    V1_3,
}

/// A validated `*_tls` block (RFC 0030 §3.1). Construction via
/// [`TlsSettings::from_parts`] enforces the §3.1 rules; the PEM files
/// themselves are read by [`TlsSettings::load`], never embedded in
/// config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsSettings {
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
    /// Present ⇒ mTLS: require-and-verify client certificates against
    /// this CA (§3.3).
    pub client_ca_file: Option<PathBuf>,
    pub min_version: TlsMinVersion,
    /// `reload_interval_secs` — `None` means never reload.
    pub reload_interval: Option<Duration>,
}

impl TlsSettings {
    /// Validate the raw string leaves of one `*_tls` block. `prefix` is
    /// the block's YAML key (e.g. `receiver.grpc_tls`), so every error
    /// names the exact offending field (RFC0030.5). All-unset resolves
    /// to `Ok(None)` — TLS is opt-in per listener.
    ///
    /// # Errors
    ///
    /// A §3.1 rule violation: a lone half of the cert/key pair, any
    /// other field without the pair, an unknown `min_version`, or a
    /// non-positive `reload_interval_secs` — each naming the exact
    /// `{prefix}.*` field.
    // @codescene(disable:"Excess Number of Function Arguments")
    // One argument per leaf of a `*_tls` block, so every front-end passes
    // the same raw strings through the one validation path.
    pub fn from_parts(
        prefix: &str,
        cert_file: Option<&str>,
        key_file: Option<&str>,
        client_ca_file: Option<&str>,
        min_version: Option<&str>,
        reload_interval_secs: Option<&str>,
    ) -> Result<Option<Self>, String> {
        let leaves = [
            cert_file,
            key_file,
            client_ca_file,
            min_version,
            reload_interval_secs,
        ];
        if leaves.into_iter().all(|leaf| present(leaf).is_none()) {
            return Ok(None);
        }
        let (cert_file, key_file) = cert_pair(prefix, present(cert_file), present(key_file))?;
        Ok(Some(Self {
            cert_file,
            key_file,
            client_ca_file: present(client_ca_file).map(PathBuf::from),
            min_version: parse_min_version(prefix, present(min_version))?,
            reload_interval: parse_reload_interval(prefix, present(reload_interval_secs))?,
        }))
    }

    /// Read the PEM material and build the listener's
    /// `rustls::ServerConfig`. Called at startup (fail fast — RFC0030.5
    /// names the unreadable/malformed path) and again on each reload
    /// tick (RFC0030.6). ALPN is left to the listener wiring — gRPC
    /// requires `h2`, the HTTP surfaces offer `http/1.1` (§3.2).
    ///
    /// # Errors
    ///
    /// An unreadable or non-PEM file (naming the path), an empty
    /// certificate chain, an unusable CA, or a cert/key mismatch.
    pub fn load(&self) -> Result<rustls::ServerConfig, String> {
        let (certs, key) = self.identity()?;
        let provider = provider();
        let builder = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_protocol_versions(self.versions())
            .map_err(|e| format!("TLS protocol-version selection failed: {e}"))?;

        let builder = match &self.client_ca_file {
            None => builder.with_no_client_auth(),
            Some(ca_path) => {
                let verifier = WebPkiClientVerifier::builder_with_provider(
                    Arc::new(roots(ca_path)?),
                    provider,
                )
                .build()
                .map_err(|e| {
                    format!(
                        "cannot build the client verifier from {}: {e}",
                        ca_path.display()
                    )
                })?;
                builder.with_client_cert_verifier(verifier)
            }
        };

        builder
            .with_single_cert(certs, key)
            .map_err(|e| format!("cannot use {}: {e}", self.cert_file.display()))
    }

    /// The client half of a mutual-TLS listener's settings, for a server
    /// that also calls its peers (RFC 0065 §6.8): it presents
    /// `cert_file`/`key_file` as its client certificate and verifies the
    /// peer's server certificate against `client_ca_file`, advertising
    /// `h2`. The server name it checks is the one each connection names.
    ///
    /// # Errors
    ///
    /// No `client_ca_file`, or whatever [`Self::load`] reports for the
    /// same files.
    pub fn client_config(&self) -> Result<rustls::ClientConfig, String> {
        let Some(ca_path) = &self.client_ca_file else {
            return Err(format!(
                "a TLS client needs a CA to verify its peers; {} has no client_ca_file",
                self.cert_file.display()
            ));
        };
        let (certs, key) = self.identity()?;
        let mut config = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(self.versions())
            .map_err(|e| format!("TLS protocol-version selection failed: {e}"))?
            .with_root_certificates(roots(ca_path)?)
            .with_client_auth_cert(certs, key)
            .map_err(|e| format!("cannot use {}: {e}", self.cert_file.display()))?;
        config.alpn_protocols = ALPN_GRPC.iter().map(|p| p.to_vec()).collect();
        Ok(config)
    }

    /// The certificate chain and its private key.
    fn identity(&self) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), String> {
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&self.cert_file)
            .map_err(|e| format!("cannot read {}: {e}", self.cert_file.display()))?
            .collect::<Result<_, _>>()
            .map_err(|e| format!("cannot parse {}: {e}", self.cert_file.display()))?;
        if certs.is_empty() {
            return Err(format!(
                "no PEM certificates found in {}",
                self.cert_file.display()
            ));
        }
        let key = PrivateKeyDer::from_pem_file(&self.key_file)
            .map_err(|e| format!("cannot read {}: {e}", self.key_file.display()))?;
        Ok((certs, key))
    }

    fn versions(&self) -> &'static [&'static rustls::SupportedProtocolVersion] {
        static FROM_1_2: [&rustls::SupportedProtocolVersion; 2] =
            [&rustls::version::TLS12, &rustls::version::TLS13];
        static FROM_1_3: [&rustls::SupportedProtocolVersion; 1] = [&rustls::version::TLS13];
        match self.min_version {
            TlsMinVersion::V1_2 => &FROM_1_2,
            TlsMinVersion::V1_3 => &FROM_1_3,
        }
    }

    /// Build the listener's [`tokio_rustls::TlsAcceptor`] from freshly
    /// read PEM material, advertising `alpn` (gRPC passes `[b"h2"]`, the
    /// HTTP surfaces pass `[b"http/1.1"]`). One acceptor per
    /// listener; the caller wraps each accepted `TcpStream`.
    ///
    /// # Errors
    ///
    /// Whatever [`Self::load`] returns (unreadable/malformed PEM, empty
    /// chain, unusable CA, cert/key mismatch), naming the path.
    pub fn acceptor(&self, alpn: &[&[u8]]) -> Result<tokio_rustls::TlsAcceptor, String> {
        let mut config = self.load()?;
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
    }
}

/// The crypto provider is pinned to ring explicitly: the workspace
/// dependency tree is what decides which providers are compiled in, and
/// relying on the process default would make this seam's behaviour
/// depend on unrelated crates' feature flags.
fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Every CA certificate in `ca_path`.
fn roots(ca_path: &Path) -> Result<RootCertStore, String> {
    let mut roots = RootCertStore::empty();
    for ca in CertificateDer::pem_file_iter(ca_path)
        .map_err(|e| format!("cannot read {}: {e}", ca_path.display()))?
    {
        let ca = ca.map_err(|e| format!("cannot parse {}: {e}", ca_path.display()))?;
        roots
            .add(ca)
            .map_err(|e| format!("cannot use a CA from {}: {e}", ca_path.display()))?;
    }
    if roots.is_empty() {
        return Err(format!(
            "no PEM certificates found in {}",
            ca_path.display()
        ));
    }
    Ok(roots)
}

/// The server cert/key pair, both halves or an error naming the missing
/// one.
fn cert_pair(
    prefix: &str,
    cert_file: Option<&str>,
    key_file: Option<&str>,
) -> Result<(PathBuf, PathBuf), String> {
    match (cert_file, key_file) {
        (Some(c), Some(k)) => Ok((PathBuf::from(c), PathBuf::from(k))),
        (Some(_), None) => Err(format!(
            "{prefix}.key_file must be set alongside {prefix}.cert_file"
        )),
        (None, Some(_)) => Err(format!(
            "{prefix}.cert_file must be set alongside {prefix}.key_file"
        )),
        // Some other field (client_ca_file / min_version /
        // reload_interval_secs) is set without the server pair.
        (None, None) => Err(format!(
            "{prefix}.cert_file and {prefix}.key_file are required to enable TLS \
             (the other {prefix}.* fields presuppose server TLS)"
        )),
    }
}

fn parse_min_version(prefix: &str, raw: Option<&str>) -> Result<TlsMinVersion, String> {
    match raw {
        None | Some("1.2") => Ok(TlsMinVersion::V1_2),
        Some("1.3") => Ok(TlsMinVersion::V1_3),
        Some(other) => Err(format!(
            "{prefix}.min_version must be \"1.2\" or \"1.3\", got {other:?}"
        )),
    }
}

fn parse_reload_interval(prefix: &str, raw: Option<&str>) -> Result<Option<Duration>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    match raw.parse::<u64>() {
        Ok(secs) if secs > 0 => Ok(Some(Duration::from_secs(secs))),
        _ => Err(format!(
            "{prefix}.reload_interval_secs must be a positive integer number \
             of seconds, got {raw:?}"
        )),
    }
}

/// ALPN for a gRPC (`tonic`/HTTP/2-only) listener.
pub const ALPN_GRPC: &[&[u8]] = &[b"h2"];

/// ALPN for the OTLP/HTTP listener. **HTTP/1.1 only** — the receiver's
/// `axum` is built with just the `http1` feature (OTLP/HTTP is a 1.1
/// protocol), so advertising `h2` would let a dual-protocol client
/// negotiate a version the server cannot actually serve.
pub const ALPN_HTTP: &[&[u8]] = &[b"http/1.1"];
