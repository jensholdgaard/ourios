//! RFC 0030 §3.1/§3.2 — validated TLS listener settings.
//!
//! One seam for every listener (receiver gRPC/HTTP and the querier
//! surface): [`TlsSettings::from_parts`] is the single validation path
//! for a `*_tls` config block (the RFC 0020 §3.1 doctrine — the error
//! text these functions produce *is* the startup error, whichever
//! front-end supplied the values), and [`TlsSettings::load`] turns the
//! validated settings into a `rustls::ServerConfig` at startup, so an
//! unreadable or malformed PEM fails fast naming the path.

use std::path::PathBuf;
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
    pub fn from_parts(
        prefix: &str,
        cert_file: Option<&str>,
        key_file: Option<&str>,
        client_ca_file: Option<&str>,
        min_version: Option<&str>,
        reload_interval_secs: Option<&str>,
    ) -> Result<Option<Self>, String> {
        if present(cert_file).is_none()
            && present(key_file).is_none()
            && present(client_ca_file).is_none()
            && present(min_version).is_none()
            && present(reload_interval_secs).is_none()
        {
            return Ok(None);
        }
        let (cert_file, key_file) = match (present(cert_file), present(key_file)) {
            (Some(c), Some(k)) => (PathBuf::from(c), PathBuf::from(k)),
            (Some(_), None) => {
                return Err(format!(
                    "{prefix}.key_file must be set alongside {prefix}.cert_file"
                ));
            }
            (None, Some(_)) => {
                return Err(format!(
                    "{prefix}.cert_file must be set alongside {prefix}.key_file"
                ));
            }
            (None, None) => {
                // Some other field (client_ca_file / min_version /
                // reload_interval_secs) is set without the server pair.
                return Err(format!(
                    "{prefix}.cert_file and {prefix}.key_file are required to enable TLS \
                     (the other {prefix}.* fields presuppose server TLS)"
                ));
            }
        };
        let min_version = match present(min_version) {
            None | Some("1.2") => TlsMinVersion::V1_2,
            Some("1.3") => TlsMinVersion::V1_3,
            Some(other) => {
                return Err(format!(
                    "{prefix}.min_version must be \"1.2\" or \"1.3\", got {other:?}"
                ));
            }
        };
        let reload_interval = match present(reload_interval_secs) {
            None => None,
            Some(raw) => match raw.parse::<u64>() {
                Ok(secs) if secs > 0 => Some(Duration::from_secs(secs)),
                _ => {
                    return Err(format!(
                        "{prefix}.reload_interval_secs must be a positive integer number \
                         of seconds, got {raw:?}"
                    ));
                }
            },
        };
        Ok(Some(Self {
            cert_file,
            key_file,
            client_ca_file: present(client_ca_file).map(PathBuf::from),
            min_version,
            reload_interval,
        }))
    }

    /// Read the PEM material and build the listener's
    /// `rustls::ServerConfig`. Called at startup (fail fast — RFC0030.5
    /// names the unreadable/malformed path) and again on each reload
    /// tick (RFC0030.6). ALPN is left to the listener wiring — gRPC
    /// requires `h2`, the HTTP surfaces offer both (§3.2).
    ///
    /// The crypto provider is pinned to ring explicitly: the workspace
    /// dependency tree is what decides which providers are compiled in,
    /// and relying on the process default would make this seam's
    /// behaviour depend on unrelated crates' feature flags.
    ///
    /// # Errors
    ///
    /// An unreadable or non-PEM file (naming the path), an empty
    /// certificate chain, an unusable CA, or a cert/key mismatch.
    pub fn load(&self) -> Result<rustls::ServerConfig, String> {
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

        let versions: &[&rustls::SupportedProtocolVersion] = match self.min_version {
            TlsMinVersion::V1_2 => &[&rustls::version::TLS12, &rustls::version::TLS13],
            TlsMinVersion::V1_3 => &[&rustls::version::TLS13],
        };
        let builder = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(versions)
        .map_err(|e| format!("TLS protocol-version selection failed: {e}"))?;

        let builder = match &self.client_ca_file {
            None => builder.with_no_client_auth(),
            Some(ca_path) => {
                let mut roots = RootCertStore::empty();
                for ca in CertificateDer::pem_file_iter(ca_path)
                    .map_err(|e| format!("cannot read {}: {e}", ca_path.display()))?
                {
                    let ca = ca.map_err(|e| format!("cannot parse {}: {e}", ca_path.display()))?;
                    roots
                        .add(ca)
                        .map_err(|e| format!("cannot use a CA from {}: {e}", ca_path.display()))?;
                }
                let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
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
}
