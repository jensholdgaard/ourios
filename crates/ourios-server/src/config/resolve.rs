//! Configuration resolution for the server binary (epic #745
//! wave 1; moved verbatim from `main.rs`, where 63% of the file had
//! become this layer): the resolved [`ServerConfig`] types, the env
//! front-end, the file front-end, and every per-subsystem `build_*`
//! validator — the RFC 0020 §3.1 single validation path, now a lib
//! module so it stops re-growing inside the binary.

use crate::config::file::{FileConfig, PromotedEntry, TlsSection};
use ourios_config::{MinerConfig, UpstreamTemplates};
use ourios_parquet::{PromotedAttributes, S3Config, StoreConfig};
use ourios_serving::tls::TlsSettings;
use ourios_wal::WalConfig;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default compaction sweep cadence when `OURIOS_COMPACTION_INTERVAL_SECS`
/// is unset.
pub const DEFAULT_COMPACTION_INTERVAL_SECS: u64 = 300;

/// Default OTLP/gRPC bind address (port 4317, the OTLP default).
pub const DEFAULT_GRPC_ADDR: &str = "0.0.0.0:4317";
/// Default OTLP/HTTP bind address (port 4318, the OTLP default).
pub const DEFAULT_HTTP_ADDR: &str = "0.0.0.0:4318";
/// Default querier HTTP bind address (port 4319, adjacent to the OTLP
/// receiver ports).
pub const DEFAULT_QUERIER_HTTP_ADDR: &str = "0.0.0.0:4319";
/// Default look-back window for a query with no `range(...)` stage — one
/// hour (RFC 0002 §4 P5; RFC 0016 §7).
pub const DEFAULT_QUERIER_WINDOW_SECS: u64 = 3600;
/// Nanoseconds per second — the unit the DSL compiler's window is in.
pub const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Resolved server configuration. `PartialEq` only — the receiver
/// params carry the miner's `f32` thresholds.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    /// The data + audit store backend (local or S3, RFC 0019).
    pub store: StoreConfig,
    /// Whether this process runs the background compaction sweep. Default on;
    /// `OURIOS_COMPACTION_ENABLED=0` disables it so a multi-pod deployment can
    /// run a single dedicated compactor rather than every pod sweeping (RFC 0009
    /// §3.2 — `publish_cas` keeps concurrent sweeps correct, but one sweeper
    /// avoids the redundant per-interval object listing).
    pub compaction_enabled: bool,
    /// How often the compaction daemon sweeps (when enabled).
    pub compaction_interval: Duration,
    /// The OTLP receiver role, if enabled (RFC 0003 §9).
    pub receiver: Option<ReceiverParams>,
    /// The querier role, if enabled (RFC 0016).
    pub querier: Option<QuerierParams>,
    /// The effective RFC 0022 promoted attribute set
    /// (`storage.promoted_attributes`, §3.2) — applied by every write path
    /// (receiver flushes and compaction rewrites; §3.4).
    pub promoted: PromotedAttributes,
    /// The resolved `auth` section (RFC 0026 static tokens + RFC 0029 OIDC),
    /// or `None` for open mode. Config-file only (§3.1 — tokens ride the
    /// `${env:…}` indirection); the env-only path always resolves open. The
    /// listeners consume the config's enforcement store: with OIDC configured
    /// and no static tokens that store is empty — enforced, not open — until
    /// the RFC 0029 verifier slice teaches the gates the full config.
    pub auth: Option<crate::auth::AuthConfig>,
}

/// Resolved querier-role configuration (RFC 0016 §3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuerierParams {
    pub http_addr: SocketAddr,
    /// RFC 0030 §3.1 — TLS on the querier listener (config-file only;
    /// `None` = plaintext). Carried here from this slice on; the
    /// acceptor wiring consumes it in the RFC0030.3 slice.
    pub http_tls: Option<TlsSettings>,
    pub default_window_nanos: u64,
    /// Serve the RFC 0027 MCP surface at `/mcp` (`querier.mcp.enabled` /
    /// `OURIOS_QUERIER_MCP_ENABLED`; default off).
    pub mcp_enabled: bool,
}

/// Resolved OTLP-receiver-role configuration (RFC 0003 §6.2).
/// `PartialEq` only: [`MinerConfig`] carries `f32` thresholds, which
/// have no total equality.
#[derive(Debug, Clone, PartialEq)]
pub struct ReceiverParams {
    pub grpc_addr: SocketAddr,
    /// RFC 0030 §3.1 — TLS per listener (config-file only; `None` =
    /// plaintext). Carried here from this slice on; the acceptor wiring
    /// consumes them in the RFC0030.1/.2 slices.
    pub grpc_tls: Option<TlsSettings>,
    pub http_addr: SocketAddr,
    pub http_tls: Option<TlsSettings>,
    pub wal_root: PathBuf,
    /// RFC 0035 §3.1 — worker count for the concurrent encode pool
    /// (`receiver.encode_workers` / `OURIOS_RECEIVER_ENCODE_WORKERS`;
    /// default: the host's available cores, validated ≥ 1).
    pub encode_workers: usize,
    /// RFC 0050 §3.2 — the upstream-template dial (`miner.*`,
    /// config-file only; the env path always gets the defaults, whose
    /// `ignore` mode is byte-identical pre-RFC behaviour).
    pub miner: MinerConfig,
}

/// Raw inputs for [`build_store_config`]. Named fields so the env and
/// file front-ends can't transpose the same-typed values (four of the
/// six are `Option<&str>`); each front-end fills the struct from its
/// own source and the builder stays the single validation path.
#[derive(Debug, Default)]
pub struct StoreInputs<'a> {
    /// `OURIOS_STORAGE_BACKEND` / `storage.backend` (`local` (default)
    /// or `s3`), trimmed and treated as unset when empty.
    pub backend: Option<&'a str>,
    /// `OURIOS_BUCKET_ROOT` / `storage.local.bucket_root` (required for
    /// the `local` backend).
    pub bucket_root: Option<PathBuf>,
    /// `OURIOS_S3_BUCKET` / `storage.s3.bucket` (required for `s3`).
    pub s3_bucket: Option<&'a str>,
    pub s3_endpoint: Option<&'a str>,
    pub s3_region: Option<&'a str>,
    pub s3_prefix: Option<&'a str>,
}

/// Raw inputs for [`build_config`]'s compaction dial.
#[derive(Debug, Default, Clone, Copy)]
pub struct CompactionInputs<'a> {
    /// `OURIOS_COMPACTION_ENABLED` / `compaction.enabled` — opt-*out*
    /// (default on); a falsey value (`0`/`false`/`no`/`off`) disables
    /// this process's sweep (RFC 0009 §3.2).
    pub enabled: Option<&'a str>,
    /// `OURIOS_COMPACTION_INTERVAL_SECS` / `compaction.interval_secs`
    /// (default [`DEFAULT_COMPACTION_INTERVAL_SECS`]).
    pub interval_secs: Option<&'a str>,
}

/// Raw inputs for [`build_receiver_config`]. TLS sections and the
/// miner dial ride along so the builder constructs [`ReceiverParams`]
/// in one place — nothing is patched onto the params after build, and
/// a disabled role still never fails over settings it doesn't use
/// (they resolve inside the enabled branch only). `None` TLS sections
/// (the env path — TLS is config-file only, RFC 0030 §3.1) resolve to
/// plaintext.
#[derive(Debug, Default)]
pub struct ReceiverInputs<'a> {
    /// `OURIOS_RECEIVER_ENABLED` / `receiver.enabled` (`1`/`true`/`yes`
    /// enables; default off).
    pub enabled: Option<&'a str>,
    /// `OURIOS_RECEIVER_GRPC_ADDR` / `receiver.grpc_addr` (default
    /// [`DEFAULT_GRPC_ADDR`]).
    pub grpc_addr: Option<&'a str>,
    /// `OURIOS_RECEIVER_HTTP_ADDR` / `receiver.http_addr` (default
    /// [`DEFAULT_HTTP_ADDR`]).
    pub http_addr: Option<&'a str>,
    /// `OURIOS_WAL_ROOT` / `receiver.wal_root` (required when enabled).
    pub wal_root: Option<PathBuf>,
    /// `OURIOS_RECEIVER_ENCODE_WORKERS` / `receiver.encode_workers`
    /// (≥ 1; default: available cores — RFC 0035 §3.1).
    pub encode_workers: Option<&'a str>,
    /// `receiver.grpc_tls` (config-file only).
    pub grpc_tls: Option<&'a TlsSection>,
    /// `receiver.http_tls` (config-file only).
    pub http_tls: Option<&'a TlsSection>,
    /// `miner.*` (config-file only, RFC 0050 §3.2).
    pub miner: MinerInputs<'a>,
}

/// Raw inputs for [`build_querier_config`].
#[derive(Debug, Default, Clone, Copy)]
pub struct QuerierInputs<'a> {
    /// `OURIOS_QUERIER_ENABLED` / `querier.enabled` (`1`/`true`/`yes`
    /// enables; default off).
    pub enabled: Option<&'a str>,
    /// `OURIOS_QUERIER_HTTP_ADDR` / `querier.http_addr` (default
    /// [`DEFAULT_QUERIER_HTTP_ADDR`]).
    pub http_addr: Option<&'a str>,
    /// `OURIOS_QUERIER_DEFAULT_WINDOW_SECS` /
    /// `querier.default_window_secs` (default
    /// [`DEFAULT_QUERIER_WINDOW_SECS`]; non-zero seconds).
    pub default_window_secs: Option<&'a str>,
    /// `OURIOS_QUERIER_MCP_ENABLED` / `querier.mcp.enabled` (default
    /// off — RFC 0027 §3.1).
    pub mcp_enabled: Option<&'a str>,
    /// `querier.http_tls` (config-file only, RFC 0030 §3.1).
    pub http_tls: Option<&'a TlsSection>,
}

/// Raw inputs for [`build_miner_config`] (RFC 0050 §3.2; config-file
/// only — the env front-end passes the default, whose absent values
/// resolve to `ignore` mode / byte limit 8192 / association limit 4).
#[derive(Debug, Default, Clone, Copy)]
pub struct MinerInputs<'a> {
    pub upstream_templates: Option<&'a str>,
    pub upstream_template_byte_limit: Option<&'a str>,
    pub upstream_association_limit: Option<&'a str>,
}

/// Resolve [`ServerConfig`] from the environment:
/// - `OURIOS_STORAGE_BACKEND` (optional, `local` (default) or `s3`) — the data
///   + audit store backend (RFC 0019).
/// - `OURIOS_BUCKET_ROOT` (required for the `local` backend) — the store root.
/// - `OURIOS_S3_BUCKET` (required for `s3`) + `OURIOS_S3_ENDPOINT` /
///   `OURIOS_S3_REGION` / `OURIOS_S3_PREFIX` (optional) — S3 addressing.
/// - `OURIOS_S3_ACCESS_KEY_ID` / `OURIOS_S3_SECRET_ACCESS_KEY` /
///   `OURIOS_S3_SESSION_TOKEN` (optional, **secret**) — explicit S3 credentials
///   applied over the standard chain (RFC 0019 §3.4); when unset, credentials
///   come from the chain (`AmazonS3Builder::from_env`, incl. IRSA). Never
///   logged (RFC 0019 §3.4).
/// - `OURIOS_COMPACTION_ENABLED` (optional, default on) — set to a falsey value
///   (`0`/`false`/`no`/`off`) to disable this process's compaction sweep, so a
///   deployment can run a single dedicated compactor (RFC 0009 §3.2).
/// - `OURIOS_COMPACTION_INTERVAL_SECS` (optional, default
///   [`DEFAULT_COMPACTION_INTERVAL_SECS`]).
/// - `OURIOS_RECEIVER_ENABLED` (optional) — enable the receiver role.
/// - `OURIOS_RECEIVER_GRPC_ADDR` / `OURIOS_RECEIVER_HTTP_ADDR` (optional,
///   default [`DEFAULT_GRPC_ADDR`] / [`DEFAULT_HTTP_ADDR`]).
/// - `OURIOS_WAL_ROOT` (required when the receiver is enabled) — the
///   write-ahead-log root (always local, RFC 0019 §3.1).
/// - `OURIOS_RECEIVER_ENCODE_WORKERS` (optional, ≥ 1; default: available
///   cores) — the RFC 0035 §3.1 concurrent-encode pool size.
///
/// # Errors
///
/// Any `build_*` validator failure over the resolved values (see each
/// builder's `# Errors`).
pub fn config_from_env() -> Result<ServerConfig, String> {
    let backend = std::env::var("OURIOS_STORAGE_BACKEND").ok();
    let s3_bucket = std::env::var("OURIOS_S3_BUCKET").ok();
    let s3_endpoint = std::env::var("OURIOS_S3_ENDPOINT").ok();
    let s3_region = std::env::var("OURIOS_S3_REGION").ok();
    let s3_prefix = std::env::var("OURIOS_S3_PREFIX").ok();
    let store = build_store_config(StoreInputs {
        backend: backend.as_deref(),
        bucket_root: std::env::var_os("OURIOS_BUCKET_ROOT").map(PathBuf::from),
        s3_bucket: s3_bucket.as_deref(),
        s3_endpoint: s3_endpoint.as_deref(),
        s3_region: s3_region.as_deref(),
        s3_prefix: s3_prefix.as_deref(),
    })?;
    // Explicit S3 credentials (RFC 0019 §3.4), layered over the standard chain.
    // Bound to locals so the `as_deref` borrows outlive the call.
    let s3_access_key_id = std::env::var("OURIOS_S3_ACCESS_KEY_ID").ok();
    let s3_secret_access_key = std::env::var("OURIOS_S3_SECRET_ACCESS_KEY").ok();
    let s3_session_token = std::env::var("OURIOS_S3_SESSION_TOKEN").ok();
    let store = with_s3_credentials(
        store,
        s3_access_key_id.as_deref(),
        s3_secret_access_key.as_deref(),
        s3_session_token.as_deref(),
    );
    let receiver_enabled = std::env::var("OURIOS_RECEIVER_ENABLED").ok();
    let grpc_addr = std::env::var("OURIOS_RECEIVER_GRPC_ADDR").ok();
    let http_addr = std::env::var("OURIOS_RECEIVER_HTTP_ADDR").ok();
    let encode_workers = std::env::var("OURIOS_RECEIVER_ENCODE_WORKERS").ok();
    // TLS and the miner dial are config-file only (RFC 0030 §3.1 /
    // RFC 0050 §3.2): the env path takes the defaults (plaintext,
    // `ignore` mode).
    let receiver = build_receiver_config(ReceiverInputs {
        enabled: receiver_enabled.as_deref(),
        grpc_addr: grpc_addr.as_deref(),
        http_addr: http_addr.as_deref(),
        wal_root: std::env::var_os("OURIOS_WAL_ROOT").map(PathBuf::from),
        encode_workers: encode_workers.as_deref(),
        ..ReceiverInputs::default()
    })?;
    let querier_enabled = std::env::var("OURIOS_QUERIER_ENABLED").ok();
    let querier_http_addr = std::env::var("OURIOS_QUERIER_HTTP_ADDR").ok();
    let window_secs = std::env::var("OURIOS_QUERIER_DEFAULT_WINDOW_SECS").ok();
    let mcp_enabled = std::env::var("OURIOS_QUERIER_MCP_ENABLED").ok();
    let querier = build_querier_config(QuerierInputs {
        enabled: querier_enabled.as_deref(),
        http_addr: querier_http_addr.as_deref(),
        default_window_secs: window_secs.as_deref(),
        mcp_enabled: mcp_enabled.as_deref(),
        http_tls: None,
    })?;
    let compaction_enabled = std::env::var("OURIOS_COMPACTION_ENABLED").ok();
    let interval_raw = std::env::var("OURIOS_COMPACTION_INTERVAL_SECS").ok();
    build_config(
        store,
        CompactionInputs {
            enabled: compaction_enabled.as_deref(),
            interval_secs: interval_raw.as_deref(),
        },
        receiver,
        querier, // The promoted set and auth are config-file only (RFC 0022
        // §3.2 / RFC 0026 §3.1): the env path resolves the implicit
        // `service.name`-only set and open mode.
        PromotedAttributes::default(),
        None,
    )
}

/// Resolve [`ServerConfig`] from a YAML configuration file (RFC 0020). The file
/// is the **sole** source of Ourios's configuration; the environment
/// participates only through `${env:…}` substitution inside it (§3.2), so a bare
/// `OURIOS_*` env var never overrides a file value.
///
/// # Errors
///
/// An unreadable file, a parse/substitution failure (unknown key,
/// unresolvable `${env:…}`), or any [`server_config_from_file`] error.
pub fn config_from_file(path: &Path) -> Result<ServerConfig, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read config file {}: {e}", path.display()))?;
    let file = crate::config::file::parse(&text, &|name| std::env::var(name).ok())
        .map_err(|e| format!("config file {}: {e}", path.display()))?;
    server_config_from_file(&file)
}

/// Map a parsed [`FileConfig`] onto the resolved [`ServerConfig`] through the
/// **same** `build_*` validators the environment path uses — the single
/// validation path (RFC 0020 §3.1). `FileConfig`'s leaves are already the
/// string-valued inputs those functions expect, so this is a pass-through: the
/// file front-end adds no second set of validation rules.
///
/// The validators name `OURIOS_*` env vars in their error text; a file-sourced
/// value that fails reuses that message rather than duplicating the rule — the
/// §3.1 trade-off of one validation path (localising the error text to YAML keys
/// is a possible follow-up).
///
/// # Errors
///
/// Any `build_*` validator failure over the file's leaves (see each
/// builder's `# Errors`), an invalid TLS block, or a bad `auth`
/// section.
pub fn server_config_from_file(file: &FileConfig) -> Result<ServerConfig, String> {
    let store = build_store_config(StoreInputs {
        backend: file.storage.backend.as_deref(),
        bucket_root: file.storage.local.bucket_root.as_deref().map(PathBuf::from),
        s3_bucket: file.storage.s3.bucket.as_deref(),
        s3_endpoint: file.storage.s3.endpoint.as_deref(),
        s3_region: file.storage.s3.region.as_deref(),
        s3_prefix: file.storage.s3.prefix.as_deref(),
    })?;
    let store = with_s3_credentials(
        store,
        file.storage.s3.access_key_id.as_deref(),
        file.storage.s3.secret_access_key.as_deref(),
        file.storage.s3.session_token.as_deref(),
    );
    let receiver = build_receiver_config(ReceiverInputs {
        enabled: file.receiver.enabled.as_deref(),
        grpc_addr: file.receiver.grpc_addr.as_deref(),
        http_addr: file.receiver.http_addr.as_deref(),
        wal_root: file.receiver.wal_root.as_deref().map(PathBuf::from),
        encode_workers: file.receiver.encode_workers.as_deref(),
        grpc_tls: Some(&file.receiver.grpc_tls),
        http_tls: Some(&file.receiver.http_tls),
        miner: MinerInputs {
            upstream_templates: file.miner.upstream_templates.as_deref(),
            upstream_template_byte_limit: file.miner.upstream_template_byte_limit.as_deref(),
            upstream_association_limit: file.miner.upstream_association_limit.as_deref(),
        },
    })?;
    let querier = build_querier_config(QuerierInputs {
        enabled: file.querier.enabled.as_deref(),
        http_addr: file.querier.http_addr.as_deref(),
        default_window_secs: file.querier.default_window_secs.as_deref(),
        mcp_enabled: file.querier.mcp.enabled.as_deref(),
        http_tls: Some(&file.querier.http_tls),
    })?;
    let promoted = build_promoted_attributes(
        &file.storage.promoted_attributes.resource,
        &file.storage.promoted_attributes.log,
    )?;
    let auth = crate::auth::build_auth_config(file.auth.as_ref())?;
    build_config(
        store,
        CompactionInputs {
            enabled: file.compaction.enabled.as_deref(),
            interval_secs: file.compaction.interval_secs.as_deref(),
        },
        receiver,
        querier,
        promoted,
        auth,
    )
}

/// Pure storage-backend resolution (env reads live in [`config_from_env`];
/// this is the testable core, RFC 0019 §3.1/§3.2).
///
/// `backend_raw` is `OURIOS_STORAGE_BACKEND` (`local` (default) or `s3`),
/// trimmed and treated as unset when empty. The `local` backend requires a
/// non-empty `bucket_root`; `s3` requires a non-empty `s3_bucket` and accepts
/// optional endpoint/region/prefix. Credentials are never read here — the
/// explicit `OURIOS_S3_*` keys are applied separately by [`with_s3_credentials`]
/// and the chain is the fallback in [`StoreConfig::open`] (RFC 0019 §3.4), so
/// an error for a **missing required** value names only the key, never a secret;
/// other errors (an unknown backend) may echo the offending non-secret value for
/// diagnosability.
///
/// # Errors
///
/// - The `local` backend (also the unset default) with a missing or
///   empty `bucket_root`.
/// - The `s3` backend with a missing or blank `s3_bucket`.
/// - An unknown backend token (the error echoes the non-secret value).
pub fn build_store_config(inputs: StoreInputs<'_>) -> Result<StoreConfig, String> {
    // Trim and treat empty as unset, so " s3 " selects S3 and a blank value
    // falls back to the local default rather than reading as an unknown backend.
    match inputs.backend.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("local") => {
            let root = inputs
                .bucket_root
                .ok_or("OURIOS_BUCKET_ROOT must be set (the local data + audit store root)")?;
            if root.as_os_str().is_empty() {
                return Err("OURIOS_BUCKET_ROOT must not be empty".to_string());
            }
            Ok(StoreConfig::Local(root))
        }
        Some("s3") => {
            let bucket = inputs
                .s3_bucket
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .ok_or("OURIOS_S3_BUCKET must be set when OURIOS_STORAGE_BACKEND=s3")?;
            let mut cfg = S3Config::new(bucket);
            if let Some(endpoint) = inputs.s3_endpoint.map(str::trim).filter(|v| !v.is_empty()) {
                cfg = cfg.with_endpoint(endpoint);
            }
            if let Some(region) = inputs.s3_region.map(str::trim).filter(|v| !v.is_empty()) {
                cfg = cfg.with_region(region);
            }
            if let Some(prefix) = inputs.s3_prefix.map(str::trim).filter(|v| !v.is_empty()) {
                cfg = cfg.with_prefix(prefix);
            }
            Ok(StoreConfig::S3(cfg))
        }
        Some(other) => Err(format!(
            "OURIOS_STORAGE_BACKEND must be 'local' or 's3', got {other:?}"
        )),
    }
}

/// Apply explicit S3 credentials (RFC 0019 §3.4) onto a resolved [`StoreConfig`].
///
/// Each value is trimmed and an empty string is treated as unset (matching the
/// addressing knobs), so a present-but-blank env var does not count as "set"
/// and trip the partial-pair check at store-build time. A `local` backend
/// carries no credentials, so it passes through unchanged. The pairing rule
/// (access key + secret together; a session token only with the pair) and the
/// secret-scrubbing of any resulting error are enforced in
/// `ourios_parquet::Store::s3`, which names only the offending field, never a
/// value (RFC 0019 §3.4).
#[must_use]
pub fn with_s3_credentials(
    store: StoreConfig,
    access_key_id: Option<&str>,
    secret_access_key: Option<&str>,
    session_token: Option<&str>,
) -> StoreConfig {
    let clean = |v: Option<&str>| {
        v.map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    match store {
        StoreConfig::S3(mut cfg) => {
            cfg.access_key_id = clean(access_key_id);
            cfg.secret_access_key = clean(secret_access_key);
            cfg.session_token = clean(session_token);
            StoreConfig::S3(cfg)
        }
        local @ StoreConfig::Local(_) => local,
    }
}

/// Pure querier-config assembly + validation (env reads live in
/// [`config_from_env`]). `None` when the querier role is disabled.
///
///
/// # Errors
///
/// Only when enabled (a disabled role validates nothing): a malformed
/// address, a zero / non-numeric / nanosecond-overflowing window, or an
/// invalid `http_tls` block.
pub fn build_querier_config(inputs: QuerierInputs<'_>) -> Result<Option<QuerierParams>, String> {
    if !matches!(inputs.enabled, Some("1" | "true" | "yes")) {
        return Ok(None);
    }
    // Opt-in like the roles themselves (RFC 0027 §3.1; default off).
    let mcp_enabled = matches!(inputs.mcp_enabled, Some("1" | "true" | "yes"));
    let http_addr = parse_addr(inputs.http_addr, DEFAULT_QUERIER_HTTP_ADDR)?;
    let window_secs = match inputs.default_window_secs {
        None => DEFAULT_QUERIER_WINDOW_SECS,
        Some(raw) => {
            let secs: u64 = raw.parse().map_err(|_| {
                format!(
                    "OURIOS_QUERIER_DEFAULT_WINDOW_SECS must be a positive integer, got {raw:?}"
                )
            })?;
            if secs == 0 {
                return Err("OURIOS_QUERIER_DEFAULT_WINDOW_SECS must be non-zero".to_string());
            }
            secs
        }
    };
    let default_window_nanos = window_secs
        .checked_mul(NANOS_PER_SEC)
        .ok_or("OURIOS_QUERIER_DEFAULT_WINDOW_SECS overflows when converted to nanoseconds")?;
    let http_tls = match inputs.http_tls {
        Some(section) => tls_settings("querier.http_tls", section)?,
        None => None,
    };
    Ok(Some(QuerierParams {
        http_addr,
        http_tls,
        default_window_nanos,
        mcp_enabled,
    }))
}

/// Pure receiver-config assembly + validation (env reads live in
/// [`config_from_env`]). `None` when the receiver role is disabled.
///
/// # Errors
///
/// Only when enabled (a disabled role validates nothing): a malformed
/// address, a missing/empty WAL root, an encode-worker count below 1,
/// an invalid TLS block, or a bad miner dial.
pub fn build_receiver_config(inputs: ReceiverInputs<'_>) -> Result<Option<ReceiverParams>, String> {
    if !matches!(inputs.enabled, Some("1" | "true" | "yes")) {
        return Ok(None);
    }
    let grpc_addr = parse_addr(inputs.grpc_addr, DEFAULT_GRPC_ADDR)?;
    let http_addr = parse_addr(inputs.http_addr, DEFAULT_HTTP_ADDR)?;
    let wal_root = inputs
        .wal_root
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or("OURIOS_WAL_ROOT must be set when the receiver role is enabled")?;
    let encode_workers = parse_encode_workers(inputs.encode_workers)?;
    let grpc_tls = match inputs.grpc_tls {
        Some(section) => tls_settings("receiver.grpc_tls", section)?,
        None => None,
    };
    let http_tls = match inputs.http_tls {
        Some(section) => tls_settings("receiver.http_tls", section)?,
        None => None,
    };
    let miner = build_miner_config(inputs.miner)?;
    Ok(Some(ReceiverParams {
        grpc_addr,
        grpc_tls,
        http_addr,
        http_tls,
        wal_root,
        encode_workers,
        miner,
    }))
}

/// Pure miner-dial assembly + validation (RFC 0050 §3.2; config-file
/// only). Absent values take the [`MinerConfig`] defaults — `ignore`
/// mode, byte limit 8192, association limit 4.
///
/// # Errors
///
/// An unknown `miner.upstream_templates` mode token, or a non-integer
/// byte / association limit.
pub fn build_miner_config(inputs: MinerInputs<'_>) -> Result<MinerConfig, String> {
    let mut config = MinerConfig::default();
    if let Some(raw) = inputs
        .upstream_templates
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let mode = match raw {
            "ignore" => UpstreamTemplates::Ignore,
            "observe" => UpstreamTemplates::Observe,
            "adopt" => UpstreamTemplates::Adopt,
            other => {
                return Err(format!(
                    "miner.upstream_templates must be 'ignore', 'observe' or 'adopt', got {other:?}"
                ));
            }
        };
        config = config.with_upstream_templates(mode);
    }
    if let Some(raw) = inputs
        .upstream_template_byte_limit
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let limit: u32 = raw.parse().map_err(|_| {
            format!(
                "miner.upstream_template_byte_limit must be an integer of UTF-8 bytes \
                 (0 disables all upstream-template handling), got {raw:?}"
            )
        })?;
        config = config.with_upstream_template_byte_limit(limit);
    }
    if let Some(raw) = inputs
        .upstream_association_limit
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let limit: u16 = raw.parse().map_err(|_| {
            format!("miner.upstream_association_limit must be an integer ≥ 0, got {raw:?}")
        })?;
        config = config.with_upstream_association_limit(limit);
    }
    Ok(config)
}

/// Parse the RFC 0035 encode-pool worker count: ≥ 1 when set, else the
/// host's available cores (min 1 — `available_parallelism` can fail in
/// constrained environments, and the pool needs at least one worker).
///
/// # Errors
///
/// A set value that is not an integer ≥ 1.
pub fn parse_encode_workers(raw: Option<&str>) -> Result<usize, String> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get));
    };
    match raw.parse::<usize>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(format!(
            "OURIOS_RECEIVER_ENCODE_WORKERS must be an integer ≥ 1, got {raw:?}"
        )),
    }
}

/// Parse a socket address, falling back to `default` when unset.
///
/// # Errors
///
/// A value that does not parse as `host:port`.
pub fn parse_addr(raw: Option<&str>, default: &str) -> Result<SocketAddr, String> {
    let value = raw.unwrap_or(default);
    value
        .parse()
        .map_err(|e| format!("invalid socket address {value:?}: {e}"))
}

/// The receiver role's WAL config: `root` plus the workspace-standard
/// durability knobs (RFC 0008 §6.3).
#[must_use]
pub fn wal_config(root: &Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    }
}

/// Pure config assembly + validation (env reads live in
/// [`config_from_env`]; this is the testable core). Takes the already
/// built role params, promoted set, and auth, so the [`ServerConfig`]
/// is constructed in one place — nothing is patched onto it after
/// build.
///
/// # Errors
///
/// A zero or non-numeric compaction interval — and only when
/// compaction is enabled: a pod with compaction disabled must not fail
/// to start over an interval it never uses.
pub fn build_config(
    store: StoreConfig,
    compaction: CompactionInputs<'_>,
    receiver: Option<ReceiverParams>,
    querier: Option<QuerierParams>,
    promoted: PromotedAttributes,
    auth: Option<crate::auth::AuthConfig>,
) -> Result<ServerConfig, String> {
    // Compaction is opt-*out* (default on), unlike the opt-in receiver/querier
    // roles: an explicit falsey value disables the sweep, anything else (incl.
    // unset) keeps it on.
    let compaction_enabled = !matches!(
        compaction.enabled.map(str::trim),
        Some("0" | "false" | "no" | "off")
    );
    // Only parse/validate the interval when compaction is on (the
    // default is a placeholder there, never read).
    let compaction_interval = if compaction_enabled {
        match compaction.interval_secs {
            None => Duration::from_secs(DEFAULT_COMPACTION_INTERVAL_SECS),
            Some(raw) => {
                let secs: u64 = raw.parse().map_err(|_| {
                    format!(
                        "OURIOS_COMPACTION_INTERVAL_SECS must be a positive integer, got {raw:?}"
                    )
                })?;
                if secs == 0 {
                    return Err("OURIOS_COMPACTION_INTERVAL_SECS must be non-zero".to_string());
                }
                Duration::from_secs(secs)
            }
        }
    } else {
        Duration::from_secs(DEFAULT_COMPACTION_INTERVAL_SECS)
    };
    Ok(ServerConfig {
        store,
        compaction_enabled,
        compaction_interval,
        receiver,
        querier,
        promoted,
        auth,
    })
}

/// Resolve `storage.promoted_attributes` (RFC 0022 §3.2 / RFC 0042 §3.2)
/// into the effective promoted set. Keys are taken literally (no
/// globbing), so a key that is empty or carries surrounding whitespace —
/// e.g. an `${env:…}` reference that resolved to nothing, or a quoted
/// `" key"` — is a config error rather than a silently never-matching
/// promoted column. RFC0042.6 makes the remaining offences loud at
/// startup rather than silently collapsed: an unknown `type` token, a
/// key listed twice within a family (whatever spelling each occurrence
/// uses), and a re-typed `service.name`.
///
/// # Errors
///
/// An empty or whitespace-padded key, an unknown `type` token, a key
/// listed twice within a family, or a re-typed `service.name`.
pub fn build_promoted_attributes(
    resource: &[PromotedEntry],
    log: &[PromotedEntry],
) -> Result<PromotedAttributes, String> {
    fn family(
        which: &str,
        entries: &[PromotedEntry],
    ) -> Result<Vec<ourios_parquet::PromotedKey>, String> {
        let mut seen = std::collections::HashSet::new();
        entries
            .iter()
            .map(|e| {
                if e.key.is_empty() || e.key.trim() != e.key {
                    return Err(format!(
                        "storage.promoted_attributes.{which} keys must be non-empty \
                         attribute names without surrounding whitespace"
                    ));
                }
                if !seen.insert(e.key.as_str()) {
                    return Err(format!(
                        "storage.promoted_attributes.{which} lists {:?} more than once \
                         — one declaration per key",
                        e.key
                    ));
                }
                let class = match e.class.as_deref() {
                    None | Some("string") => ourios_parquet::PromotedClass::String,
                    Some("i64") => ourios_parquet::PromotedClass::I64,
                    Some("f64") => ourios_parquet::PromotedClass::F64,
                    Some(other) => {
                        return Err(format!(
                            "storage.promoted_attributes.{which} key {:?} declares unknown \
                             type {other:?} — expected \"string\", \"i64\", or \"f64\" \
                             (RFC 0042 §3.2)",
                            e.key
                        ));
                    }
                };
                if which == "resource"
                    && e.key == ourios_parquet::SERVICE_NAME_KEY
                    && class != ourios_parquet::PromotedClass::String
                {
                    return Err(format!(
                        "storage.promoted_attributes.resource cannot re-type \
                         {:?}: the implicit promotion is string-class (RFC 0042 §3.2)",
                        ourios_parquet::SERVICE_NAME_KEY
                    ));
                }
                Ok(ourios_parquet::PromotedKey {
                    key: e.key.clone(),
                    class,
                })
            })
            .collect()
    }
    Ok(PromotedAttributes::new_typed(
        family("resource", resource)?,
        family("log", log)?,
    ))
}

/// One `*_tls` block through the single validation path (RFC 0030
/// §3.1): the raw file leaves into [`TlsSettings::from_parts`], with
/// the block's YAML key as the error prefix.
///
/// # Errors
///
/// See [`TlsSettings::from_parts`]: a half-configured cert/key pair,
/// an unknown `min_version`, or a non-integer reload interval.
pub fn tls_settings(prefix: &str, section: &TlsSection) -> Result<Option<TlsSettings>, String> {
    TlsSettings::from_parts(
        prefix,
        section.cert_file.as_deref(),
        section.key_file.as_deref(),
        section.client_ca_file.as_deref(),
        section.min_version.as_deref(),
        section.reload_interval_secs.as_deref(),
    )
}

/// Fail fast on unusable TLS material (RFC0030.5): every configured
/// `*_tls` block's files are read and built into a `rustls` config at
/// startup, so an unreadable or malformed PEM is a startup error naming
/// the block and the path — not a first-handshake surprise.
///
/// # Errors
///
/// A TLS block whose files can't be read or built into a `rustls`
/// config, prefixed with the block's YAML key.
pub fn preflight_tls(config: &ServerConfig) -> Result<(), String> {
    let blocks = [
        (
            "receiver.grpc_tls",
            config.receiver.as_ref().and_then(|r| r.grpc_tls.as_ref()),
        ),
        (
            "receiver.http_tls",
            config.receiver.as_ref().and_then(|r| r.http_tls.as_ref()),
        ),
        (
            "querier.http_tls",
            config.querier.as_ref().and_then(|q| q.http_tls.as_ref()),
        ),
    ];
    for (key, settings) in blocks {
        if let Some(settings) = settings {
            settings.load().map_err(|e| format!("{key}: {e}"))?;
        }
    }
    Ok(())
}

/// RFC 0030 §3.4: credentials over a plaintext listener get one startup
/// warning naming the listener — visible, not fatal (TLS may
/// legitimately terminate at a fronting proxy or mesh).
pub fn warn_if_plaintext_credentials(config: &ServerConfig) {
    if config.auth.is_none() {
        return;
    }
    let mut plaintext: Vec<&str> = Vec::new();
    if let Some(receiver) = &config.receiver {
        if receiver.grpc_tls.is_none() {
            plaintext.push("receiver.grpc_addr");
        }
        if receiver.http_tls.is_none() {
            plaintext.push("receiver.http_addr");
        }
    }
    if let Some(querier) = &config.querier
        && querier.http_tls.is_none()
    {
        plaintext.push("querier.http_addr");
    }
    for listener in plaintext {
        tracing::warn!(
            name: ourios_semconv::EVENT_OURIOS_SERVER_TLS_PLAINTEXT_CREDENTIALS,
            listener,
            "{listener} serves bearer credentials over plaintext (no *_tls \
             block; RFC 0030 §3.4) — acceptable only behind a \
             TLS-terminating proxy or mesh"
        );
    }
}

/// RFC 0026 §3.1 open mode: with no `auth` configured, any client that can
/// reach a listener can write into and read from any tenant. Warn once at
/// startup so the exposure is a visible choice, not a silent default. A
/// compactor-only process binds nothing, so it has nothing to expose.
pub fn warn_if_open_mode(config: &ServerConfig) {
    if config.auth.is_none() && (config.receiver.is_some() || config.querier.is_some()) {
        tracing::warn!(
            name: ourios_semconv::EVENT_OURIOS_SERVER_AUTH_OPEN_MODE,
            "auth is not configured: the network listeners accept unauthenticated \
             requests for any tenant (RFC 0026 open mode)"
        );
    }
}

/// RFC 0047 §3.4 / RFC 0048 §3.2: the object column, the self-fast-path
/// column and every **operator-listed** identity column must be in the
/// effective promoted set — the operator hears a typo at startup, not as
/// an empty graph. The defaulted identity lists are exempt: they are the
/// RFC 0047 constants, which never required promotion (the emitter reads
/// record attributes, not the projection).
///
/// # Errors
///
/// An operator-listed identity column (or the object / self-fast-path
/// column) absent from the effective promoted set.
pub fn validate_graph_columns(
    openfga: &ourios_core::auth::openfga::OpenFgaConfig,
    promoted: &PromotedAttributes,
) -> Result<(), String> {
    let known: std::collections::BTreeSet<String> = promoted.column_names().collect();
    let visibility = openfga.visibility();
    let check = |what: &str, column: &str| -> Result<(), String> {
        if known.contains(column) {
            Ok(())
        } else {
            Err(format!(
                "auth.openfga.visibility.{what}: `{column}` is not a promoted column — add \
                 it to storage.promoted_attributes (RFC 0048 §3.2)"
            ))
        }
    };
    for object in visibility.objects() {
        check("objects[].column", object.column())?;
    }
    if visibility.user_columns_configured() {
        for column in visibility.user_columns() {
            check("identities.user_columns", column)?;
        }
    }
    if visibility.agent_columns_configured() {
        for column in visibility.agent_columns() {
            check("identities.agent_columns", column)?;
        }
    }
    if let Some(column) = visibility.self_principal_column() {
        check("self_principal_column", column)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    use crate::config::file::parse;
    use std::io::Write as _;

    /// A `local` [`StoreConfig`] for `path`, the common test fixture.
    fn local(path: &str) -> StoreConfig {
        StoreConfig::Local(PathBuf::from(path))
    }

    /// Parse `yaml` with an empty environment, then map it onto a `ServerConfig`
    /// through the shared `build_*` validators (RFC 0020 §3.1).
    fn server_config(yaml: &str) -> Result<ServerConfig, String> {
        let file = parse(yaml, &|_| None).expect("well-formed file");
        server_config_from_file(&file)
    }

    /// Scenario RFC0020.1 — a complete file resolves to the same `ServerConfig`
    /// the equivalent `OURIOS_*` environment would produce, field for field.
    /// See `docs/rfcs/0020-configuration-file.md` §5.
    #[test]
    fn rfc0020_1_file_resolves_to_the_same_config_as_the_env() {
        let from_file = server_config(
            "\
storage:
  backend: s3
  s3:
    bucket: my-logs
receiver:
  enabled: true
  wal_root: /var/lib/ourios/wal
querier:
  enabled: true
compaction:
  interval_secs: 120
",
        )
        .expect("valid");

        // The same values expressed through the env-path helpers (the shared
        // validators), as `config_from_env` would assemble them.
        let store = with_s3_credentials(
            build_store_config(StoreInputs {
                backend: Some("s3"),
                bucket_root: None,
                s3_bucket: Some("my-logs"),
                s3_endpoint: None,
                s3_region: None,
                s3_prefix: None,
            })
            .expect("s3"),
            None,
            None,
            None,
        );
        let mut expected = build_config(
            store,
            CompactionInputs {
                enabled: None,
                interval_secs: Some("120"),
            },
            None,
            None,
            PromotedAttributes::default(),
            None,
        )
        .expect("valid");
        expected.receiver = build_receiver_config(ReceiverInputs {
            enabled: Some("true"),
            grpc_addr: None,
            http_addr: None,
            wal_root: Some(PathBuf::from("/var/lib/ourios/wal")),
            encode_workers: None,
            ..ReceiverInputs::default()
        })
        .expect("receiver");
        expected.querier = build_querier_config(QuerierInputs {
            enabled: Some("true"),
            http_addr: None,
            default_window_secs: None,
            mcp_enabled: None,
            http_tls: None,
        })
        .expect("querier");

        assert_eq!(from_file, expected);
    }

    /// Scenario RFC0020.3 — the file is authoritative; a bare `OURIOS_*` env var
    /// does not override a file value (only `${env:…}` refs inside the file
    /// consult the environment). See `docs/rfcs/0020-configuration-file.md` §5.
    #[test]
    fn rfc0020_3_file_value_is_authoritative_over_bare_env() {
        let yaml = "\
storage:
  local:
    bucket_root: /store
querier:
  enabled: true
  default_window_secs: 1800
";
        // The lookup "sets" the bare env knob to 3600, but the file has no
        // `${env:…}` reference to it, so it is never consulted.
        let file = parse(yaml, &|name| {
            (name == "OURIOS_QUERIER_DEFAULT_WINDOW_SECS").then(|| "3600".to_owned())
        })
        .expect("valid");
        let config = server_config_from_file(&file).expect("valid");

        assert_eq!(
            config.querier.expect("enabled").default_window_nanos,
            1800 * NANOS_PER_SEC,
            "the file value wins; the bare env var is ignored",
        );
    }

    /// RFC 0050 §3.2 — the `miner.*` section resolves onto the
    /// receiver's `MinerConfig`, leaving every non-dial tunable at
    /// its code default.
    #[test]
    fn rfc0050_miner_section_resolves_the_dial() {
        let config = server_config(
            "\
storage:
  local:
    bucket_root: /store
receiver:
  enabled: true
  wal_root: /wal
miner:
  upstream_templates: adopt
  upstream_template_byte_limit: 4096
  upstream_association_limit: 2
",
        )
        .expect("valid");
        let receiver = config.receiver.expect("enabled");
        assert_eq!(receiver.miner.upstream_templates, UpstreamTemplates::Adopt);
        assert_eq!(receiver.miner.upstream_template_byte_limit, 4096);
        assert_eq!(receiver.miner.upstream_association_limit, 2);
        assert_eq!(
            receiver.miner.max_templates,
            MinerConfig::default().max_templates,
            "non-dial tunables stay at code defaults",
        );
    }

    /// RFC 0050 §3.2 — an absent `miner.*` section is the unchanged
    /// default: `ignore` mode, byte-identical pre-RFC behaviour.
    #[test]
    fn rfc0050_miner_section_defaults_to_ignore() {
        let config = server_config(
            "\
storage:
  local:
    bucket_root: /store
receiver:
  enabled: true
  wal_root: /wal
",
        )
        .expect("valid");
        assert_eq!(
            config.receiver.expect("enabled").miner,
            MinerConfig::default(),
        );
    }

    /// RFC 0050 §3.2 — an unknown mode fails startup loudly, naming
    /// the YAML key (RFC 0001 §3.2.2: refuse to serve rather than
    /// degrade silently).
    #[test]
    fn rfc0050_miner_mode_rejects_unknown_value() {
        let err = server_config(
            "\
storage:
  local:
    bucket_root: /store
receiver:
  enabled: true
  wal_root: /wal
miner:
  upstream_templates: maybe
",
        )
        .expect_err("unknown mode must fail");
        assert!(err.contains("miner.upstream_templates"), "{err}");
    }

    /// RFC 0050 §3.2 — the numeric limits fail startup loudly on
    /// malformed, negative, and out-of-range values, naming their
    /// YAML keys (RFC 0001 §3.2.2).
    #[test]
    fn rfc0050_miner_limits_reject_invalid_values() {
        for (raw, key) in [
            ("abc", "miner.upstream_template_byte_limit"),
            ("-1", "miner.upstream_template_byte_limit"),
            ("4294967296", "miner.upstream_template_byte_limit"), // u32::MAX + 1
        ] {
            let err = build_miner_config(MinerInputs {
                upstream_templates: None,
                upstream_template_byte_limit: Some(raw),
                upstream_association_limit: None,
            })
            .expect_err("invalid byte limit must fail");
            assert!(err.contains(key), "{raw:?} → {err}");
        }
        for (raw, key) in [
            ("abc", "miner.upstream_association_limit"),
            ("-1", "miner.upstream_association_limit"),
            ("65536", "miner.upstream_association_limit"), // u16::MAX + 1
        ] {
            let err = build_miner_config(MinerInputs {
                upstream_templates: None,
                upstream_template_byte_limit: None,
                upstream_association_limit: Some(raw),
            })
            .expect_err("invalid association limit must fail");
            assert!(err.contains(key), "{raw:?} → {err}");
        }
        // The documented boundary values are accepted: 0 disables all
        // upstream-template handling; whitespace reads as unset.
        let zero = build_miner_config(MinerInputs {
            upstream_templates: None,
            upstream_template_byte_limit: Some("0"),
            upstream_association_limit: Some("0"),
        })
        .expect("0 is valid");
        assert_eq!(zero.upstream_template_byte_limit, 0);
        assert_eq!(zero.upstream_association_limit, 0);
        assert_eq!(
            build_miner_config(MinerInputs {
                upstream_templates: Some("  "),
                upstream_template_byte_limit: Some(" "),
                upstream_association_limit: Some("")
            })
            .expect("blank = unset"),
            MinerConfig::default(),
        );
    }

    /// RFC 0020 §3.3 — the miner dial rides `${env:…}` substitution
    /// like every other scalar leaf.
    #[test]
    fn rfc0050_miner_mode_rides_env_substitution() {
        let yaml = "\
storage:
  local:
    bucket_root: /store
receiver:
  enabled: true
  wal_root: /wal
miner:
  upstream_templates: ${env:OURIOS_TEST_MODE}
";
        let file = parse(yaml, &|name| {
            (name == "OURIOS_TEST_MODE").then(|| "observe".to_owned())
        })
        .expect("valid");
        let config = server_config_from_file(&file).expect("valid");
        assert_eq!(
            config.receiver.expect("enabled").miner.upstream_templates,
            UpstreamTemplates::Observe,
        );
    }

    /// Scenario RFC0020.5 (value arm) — a well-formed file whose *value* the
    /// shared validators reject fails fast, through the same rule the env path
    /// enforces; no partial config is produced. (The malformed-reference and
    /// unknown-key arms are covered in `config::file`.)
    /// See `docs/rfcs/0020-configuration-file.md` §5.
    #[test]
    fn rfc0020_5_invalid_file_value_fails_fast() {
        // `s3` backend with no bucket — the same validation as the env path.
        let err = server_config("storage:\n  backend: s3\n").expect_err("s3 needs a bucket");
        assert!(
            err.contains("S3_BUCKET"),
            "names the missing bucket: {err:?}"
        );

        // A non-numeric querier window is rejected.
        let err = server_config(
            "\
storage:
  local:
    bucket_root: /store
querier:
  enabled: true
  default_window_secs: soon
",
        )
        .expect_err("bad window");
        assert!(
            err.contains("DEFAULT_WINDOW_SECS"),
            "names the offending field: {err:?}",
        );
    }

    /// Scenario RFC0020.6 — secret hygiene across the file path. A resolved
    /// credential is present in the `FileConfig`, yet a sibling value that the
    /// mapping rejects produces an error naming the offending key only — never
    /// the resolved secret (extends RFC 0019 §3.4 / RFC0019.6 to the file path;
    /// the `${env:…}`-only credential rule and `Debug` redaction are covered in
    /// `config::file`). See `docs/rfcs/0020-configuration-file.md` §5.
    #[test]
    fn rfc0020_6_secret_hygiene_across_the_file_path() {
        let secret = "topsecret-access-key";
        // The credentials are `${env:…}` references (§3.5); they resolve to real
        // values, but the backend is `s3` with no bucket — a sibling error.
        let file = parse(
            "\
storage:
  backend: s3
  s3:
    access_key_id: ${env:KEY}
    secret_access_key: ${env:SECRET}
",
            &|name| match name {
                "KEY" => Some("AKIAEXAMPLE".to_owned()),
                "SECRET" => Some(secret.to_owned()),
                _ => None,
            },
        )
        .expect("parses (credentials are references)");

        // The secret is resolved and present in the config...
        assert_eq!(file.storage.s3.secret_access_key.as_deref(), Some(secret));

        // ...but the mapping fails on the missing bucket, and the error names the
        // offending key, never the resolved secret.
        let err = server_config_from_file(&file).expect_err("s3 needs a bucket");
        assert!(err.contains("S3_BUCKET"), "names the missing key: {err}");
        assert!(
            !err.contains(secret),
            "the resolved secret must not leak: {err}"
        );
    }

    /// Scenario RFC0026.1 (mapping) — the file's `auth` section resolves
    /// through the shared validators like every other section: a token that
    /// arrived via `${env:…}` substitution authenticates in the resolved
    /// store, an absent section resolves open (`None`), and an empty token
    /// list fails the mapping. The schema/substitution/redaction arms live in
    /// `config::file`, the store validation matrix in `crate::auth`,
    /// and the startup-observable arms in `tests/rfc0026_auth.rs`.
    /// See `docs/rfcs/0026-authentication-tenant-binding.md` §5.
    #[test]
    fn rfc0026_1_auth_section_maps_onto_the_token_store() {
        let yaml = "\
storage:
  backend: local
  local:
    bucket_root: /var/lib/ourios
auth:
  tokens:
    - name: edge-collector
      token: ${env:TOK}
      tenants: [acme]
";
        let file = parse(yaml, &|name| {
            (name == "TOK").then(|| "resolved-token".to_owned())
        })
        .expect("well-formed file");
        let config = server_config_from_file(&file).expect("valid");
        let store = config
            .auth
            .expect("auth resolved")
            .static_tokens
            .expect("static half");
        assert_eq!(
            store.authenticate("resolved-token").expect("match").name(),
            "edge-collector",
        );

        let open = server_config("storage:\n  local:\n    bucket_root: /x\n").expect("valid");
        assert!(open.auth.is_none(), "no auth section resolves open");

        let err = server_config("storage:\n  local:\n    bucket_root: /x\nauth:\n  tokens: []\n")
            .expect_err("empty token list");
        assert!(err.contains("auth.tokens"), "names the key: {err}");
    }

    /// Scenario RFC0029.1 (mapping) — the file's `auth.oidc` section resolves
    /// through the shared validators: an `${env:…}`-substituted issuer lands
    /// in the resolved config, an oidc-only section resolves with an *empty*
    /// enforcement store (enforced, not open), a missing `audience` fails,
    /// and `tokens: []` fails even with `oidc` present. The schema arms live
    /// in `config::file`, the validation matrix in `ourios_core::auth`, and
    /// the startup-observable arms in `tests/it/rfc0029_oidc.rs`.
    /// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
    #[test]
    fn rfc0029_1_oidc_section_maps_onto_the_auth_config() {
        let yaml = "\
storage:
  backend: local
  local:
    bucket_root: /var/lib/ourios
auth:
  oidc:
    issuer: ${env:ISSUER}
    audience: ourios
    tenant_claim: ourios_tenants
";
        let file = parse(yaml, &|name| {
            (name == "ISSUER").then(|| "https://dex.internal.example".to_owned())
        })
        .expect("well-formed file");
        let config = server_config_from_file(&file).expect("valid");
        let auth = config.auth.expect("auth resolved");
        let oidc = auth.oidc.as_ref().expect("oidc half");
        assert_eq!(oidc.issuer(), "https://dex.internal.example");
        assert_eq!(oidc.name_claim(), "sub", "core default applied");
        assert!(auth.static_tokens.is_none(), "no static half");
        // Enforced-not-open for oidc-only is a resolver/serving property
        // now (the `enforcement_store` bridge is retired): the served
        // `rfc0029_oidc` arms assert the wire-level 401.
        assert!(auth.oidc.is_some() && auth.static_tokens.is_none());

        let err = server_config(
            "storage:\n  local:\n    bucket_root: /x\nauth:\n  oidc:\n    issuer: https://x\n    tenant_claim: t\n",
        )
        .expect_err("missing audience");
        assert!(err.contains("auth.oidc.audience"), "names the key: {err}");

        let err = server_config(
            "storage:\n  local:\n    bucket_root: /x\nauth:\n  tokens: []\n  oidc:\n    issuer: https://x\n    audience: a\n    tenant_claim: t\n",
        )
        .expect_err("explicit empty list stays an error with oidc present");
        assert!(err.contains("auth.tokens"), "names the key: {err}");
    }

    /// RFC 0027 §3.1 — the MCP flag is opt-in: absent/falsey values leave
    /// it off, the role-standard truthy values enable it, on both the env
    /// and file paths (the same `build_querier_config`).
    #[test]
    fn querier_mcp_flag_defaults_off_and_accepts_truthy() {
        for off in [None, Some("0"), Some("false"), Some("off"), Some("")] {
            let params = build_querier_config(QuerierInputs {
                enabled: Some("1"),
                http_addr: None,
                default_window_secs: None,
                mcp_enabled: off,
                http_tls: None,
            })
            .expect("valid")
            .expect("enabled");
            assert!(!params.mcp_enabled, "{off:?} leaves MCP off");
        }
        for on in ["1", "true", "yes"] {
            let params = build_querier_config(QuerierInputs {
                enabled: Some("1"),
                http_addr: None,
                default_window_secs: None,
                mcp_enabled: Some(on),
                http_tls: None,
            })
            .expect("valid")
            .expect("enabled");
            assert!(params.mcp_enabled, "{on:?} enables MCP");
        }
    }

    /// `config_from_file` end-to-end through the real filesystem: a valid file
    /// reads and resolves, and both failure paths name the offending file — a
    /// missing file via the read-error prefix, a parse failure via the
    /// config-file prefix (RFC0020.1 read path / RFC0020.5 error reporting).
    #[test]
    fn config_from_file_reads_maps_and_names_the_path() {
        // Happy path: a self-contained file (no `${env:…}` refs) reads and maps.
        let mut good = tempfile::NamedTempFile::new().expect("temp file");
        write!(
            good,
            "storage:\n  local:\n    bucket_root: /store\nquerier:\n  enabled: true\n"
        )
        .expect("write");
        let config = config_from_file(good.path()).expect("valid file resolves");
        assert_eq!(config.store, local("/store"));
        assert!(config.querier.is_some(), "the querier role is enabled");

        // A missing file is reported with the read-error prefix and the path.
        let missing = Path::new("/no/such/ourios-config.yaml");
        let err = config_from_file(missing).expect_err("missing file");
        assert!(err.contains("read config file"), "read-error prefix: {err}");
        assert!(err.contains("ourios-config.yaml"), "names the path: {err}");

        // A parse failure is reported with the config-file prefix, the path, and
        // the offending reference — never a resolved value.
        let mut bad = tempfile::NamedTempFile::new().expect("temp file");
        write!(bad, "storage:\n  backend: ${{1BAD}}\n").expect("write");
        let err = config_from_file(bad.path()).expect_err("malformed reference");
        assert!(err.contains("config file"), "config-file prefix: {err}");
        assert!(err.contains("${1BAD}"), "names the reference: {err}");
        assert!(
            err.contains(&bad.path().display().to_string()),
            "names the path: {err}",
        );
    }

    #[test]
    fn build_config_defaults_the_interval() {
        // Arrange / Act
        let config = build_config(
            local("/store"),
            CompactionInputs {
                enabled: None,
                interval_secs: None,
            },
            None,
            None,
            PromotedAttributes::default(),
            None,
        )
        .expect("valid");

        // Assert
        assert_eq!(
            config.compaction_interval,
            Duration::from_secs(DEFAULT_COMPACTION_INTERVAL_SECS),
        );
        assert_eq!(config.store, local("/store"));
    }

    /// RFC 0022 §3.2 — `storage.promoted_attributes` resolves onto the
    /// `ServerConfig` through the shared validator: configured keys land in
    /// the effective set (the implicit `service.name` and dedup are the
    /// `PromotedAttributes` contract), an omitted section is the default
    /// (`service.name`-only) set, and an empty key is a config error.
    #[test]
    fn promoted_attributes_resolve_onto_the_server_config() {
        let config = server_config(
            "storage:\n  local:\n    bucket_root: /store\n  promoted_attributes:\n    resource: [k8s.namespace.name]\n    log: [http.route]\n",
        )
        .expect("valid");
        assert_eq!(
            config.promoted,
            PromotedAttributes::new(
                ["k8s.namespace.name".to_string()],
                ["http.route".to_string()],
            ),
        );

        let defaulted =
            server_config("storage:\n  local:\n    bucket_root: /store\n").expect("valid");
        assert_eq!(defaulted.promoted, PromotedAttributes::default());

        let bare = |key: &str| PromotedEntry {
            key: key.to_string(),
            class: None,
        };
        let typed = |key: &str, class: &str| PromotedEntry {
            key: key.to_string(),
            class: Some(class.to_string()),
        };

        let err = build_promoted_attributes(&[bare("k8s.namespace.name")], &[bare("")])
            .expect_err("empty key");
        assert!(err.contains("non-empty"), "the error names the rule: {err}");
        // Surrounding whitespace would mint a promoted column whose name can
        // never match the intended attribute key — rejected, not normalised.
        build_promoted_attributes(&[bare(" k8s.namespace.name")], &[])
            .expect_err("whitespace-padded key");
        build_promoted_attributes(&[], &[bare("http.route ")])
            .expect_err("trailing-whitespace key");

        // RFC0042.6 — typed entries build the classed set; `string` is the
        // bare spelling's explicit form.
        let set = build_promoted_attributes(
            &[],
            &[
                bare("model"),
                typed("cost_usd", "f64"),
                typed("input_tokens", "i64"),
                typed("decision", "string"),
            ],
        )
        .expect("typed set");
        assert_eq!(
            set.log_keys()
                .iter()
                .map(|k| (k.key.as_str(), k.class))
                .collect::<Vec<_>>(),
            [
                ("model", ourios_parquet::PromotedClass::String),
                ("cost_usd", ourios_parquet::PromotedClass::F64),
                ("input_tokens", ourios_parquet::PromotedClass::I64),
                ("decision", ourios_parquet::PromotedClass::String),
            ]
        );

        // RFC0042.6 — the three loud startup offences, each error naming
        // its offence.
        let err = build_promoted_attributes(&[], &[typed("cost_usd", "float")])
            .expect_err("unknown type");
        assert!(
            err.contains("unknown") && err.contains("float") && err.contains("cost_usd"),
            "names the token and the key: {err}"
        );
        let err = build_promoted_attributes(&[], &[bare("cost_usd"), typed("cost_usd", "f64")])
            .expect_err("duplicate across spellings");
        assert!(
            err.contains("more than once") && err.contains("cost_usd"),
            "names the duplicated key: {err}"
        );
        let err = build_promoted_attributes(&[typed("service.name", "i64")], &[])
            .expect_err("re-typed service.name");
        assert!(
            err.contains("service.name") && err.contains("string-class"),
            "names the implicit promotion rule: {err}"
        );
        // A string-class service.name declaration is redundant but legal.
        build_promoted_attributes(&[typed("service.name", "string")], &[])
            .expect("explicit string service.name collapses");
        // service.name under `log` is an ordinary key (the implicit
        // promotion is the *resource* family), so an i64 there is legal.
        build_promoted_attributes(&[], &[typed("service.name", "i64")])
            .expect("log-family service.name is not the implicit promotion");
    }

    #[test]
    fn build_config_parses_a_custom_interval() {
        // Arrange / Act
        let config = build_config(
            local("/store"),
            CompactionInputs {
                enabled: None,
                interval_secs: Some("60"),
            },
            None,
            None,
            PromotedAttributes::default(),
            None,
        )
        .expect("valid");

        // Assert
        assert_eq!(config.compaction_interval, Duration::from_secs(60));
    }

    #[test]
    fn build_config_rejects_a_zero_or_nonnumeric_interval() {
        // Arrange / Act / Assert
        assert!(
            build_config(
                local("/store"),
                CompactionInputs {
                    enabled: None,
                    interval_secs: Some("0")
                },
                None,
                None,
                PromotedAttributes::default(),
                None
            )
            .is_err(),
            "a zero interval would busy-loop the daemon",
        );
        assert!(
            build_config(
                local("/store"),
                CompactionInputs {
                    enabled: None,
                    interval_secs: Some("soon")
                },
                None,
                None,
                PromotedAttributes::default(),
                None
            )
            .is_err(),
            "non-numeric interval is rejected",
        );
    }

    #[test]
    fn build_config_compaction_is_opt_out() {
        // Default (unset) and any non-falsey value keep compaction on; only an
        // explicit falsey token (trimmed) turns it off — the inverse of the
        // opt-in receiver/querier roles.
        for raw in [None, Some("1"), Some("true"), Some("yes"), Some("anything")] {
            assert!(
                build_config(
                    local("/store"),
                    CompactionInputs {
                        enabled: raw,
                        interval_secs: None
                    },
                    None,
                    None,
                    PromotedAttributes::default(),
                    None
                )
                .expect("valid")
                .compaction_enabled,
                "compaction stays on for {raw:?}",
            );
        }
        for raw in [
            Some("0"),
            Some("false"),
            Some("no"),
            Some("off"),
            Some("  off  "),
        ] {
            assert!(
                !build_config(
                    local("/store"),
                    CompactionInputs {
                        enabled: raw,
                        interval_secs: None
                    },
                    None,
                    None,
                    PromotedAttributes::default(),
                    None
                )
                .expect("valid")
                .compaction_enabled,
                "compaction is disabled for {raw:?}",
            );
        }
    }

    #[test]
    fn build_config_disabled_compaction_ignores_a_bad_interval() {
        // With compaction off, the interval is never used, so an otherwise-
        // rejected value must not block startup.
        for bad in [Some("0"), Some("soon")] {
            assert!(
                build_config(
                    local("/store"),
                    CompactionInputs {
                        enabled: Some("off"),
                        interval_secs: bad
                    },
                    None,
                    None,
                    PromotedAttributes::default(),
                    None
                )
                .is_ok(),
                "a disabled pod starts despite interval {bad:?}",
            );
        }
    }

    /// Scenario RFC0019.1 — backend selection from config.
    /// See `docs/rfcs/0019-storage-backend-selection.md` §5.
    // One scenario per RFC criterion; the named-field input structs
    // push the assertion table past the line lint without adding cases.
    #[allow(clippy::too_many_lines)]
    #[test]
    fn rfc0019_1_backend_selection_from_config() {
        // Unset backend + a bucket root → local.
        assert_eq!(
            build_store_config(StoreInputs {
                backend: None,
                bucket_root: Some(PathBuf::from("/store")),
                s3_bucket: None,
                s3_endpoint: None,
                s3_region: None,
                s3_prefix: None
            })
            .expect("local default"),
            local("/store"),
        );
        // Explicit `local` behaves the same.
        assert_eq!(
            build_store_config(StoreInputs {
                backend: Some("local"),
                bucket_root: Some(PathBuf::from("/store")),
                s3_bucket: None,
                s3_endpoint: None,
                s3_region: None,
                s3_prefix: None
            })
            .expect("explicit local"),
            local("/store"),
        );
        // `s3` + a bucket (and optional addressing) → an S3 backend.
        let s3 = build_store_config(StoreInputs {
            backend: Some("s3"),
            bucket_root: None,
            s3_bucket: Some("my-bucket"),
            s3_endpoint: Some("http://localhost:4566"),
            s3_region: Some("us-east-1"),
            s3_prefix: Some("ourios"),
        })
        .expect("s3 selected");
        assert_eq!(
            s3,
            StoreConfig::S3(
                S3Config::new("my-bucket")
                    .with_endpoint("http://localhost:4566")
                    .with_region("us-east-1")
                    .with_prefix("ourios"),
            ),
        );
        // `s3` without a bucket, and an unknown backend, both fail fast.
        assert!(
            build_store_config(StoreInputs {
                backend: Some("s3"),
                bucket_root: None,
                s3_bucket: None,
                s3_endpoint: None,
                s3_region: None,
                s3_prefix: None
            })
            .is_err(),
            "s3 backend requires OURIOS_S3_BUCKET",
        );
        assert!(
            build_store_config(StoreInputs {
                backend: Some("gcs"),
                bucket_root: Some(PathBuf::from("/store")),
                s3_bucket: None,
                s3_endpoint: None,
                s3_region: None,
                s3_prefix: None
            })
            .is_err(),
            "an unknown backend is rejected",
        );
        // Local backend with no bucket root is rejected — "must be set" for an
        // unset var, distinct from "must not be empty" for a present-but-empty
        // one (clearer operator diagnostics).
        let unset = build_store_config(StoreInputs {
            backend: None,
            bucket_root: None,
            s3_bucket: None,
            s3_endpoint: None,
            s3_region: None,
            s3_prefix: None,
        })
        .expect_err("unset");
        assert!(
            unset.contains("must be set"),
            "unset names the missing key, got {unset:?}",
        );
        let empty = build_store_config(StoreInputs {
            backend: Some("local"),
            bucket_root: Some(PathBuf::from("")),
            s3_bucket: None,
            s3_endpoint: None,
            s3_region: None,
            s3_prefix: None,
        })
        .expect_err("empty");
        assert!(
            empty.contains("must not be empty"),
            "an empty bucket root is reported distinctly, got {empty:?}",
        );
        // The backend value is trimmed; a blank value is treated as unset
        // (→ local), not as an unknown backend.
        assert_eq!(
            build_store_config(StoreInputs {
                backend: Some("  s3  "),
                bucket_root: None,
                s3_bucket: Some("b"),
                s3_endpoint: None,
                s3_region: None,
                s3_prefix: None
            })
            .expect("trimmed s3"),
            StoreConfig::S3(S3Config::new("b")),
        );
        assert_eq!(
            build_store_config(StoreInputs {
                backend: Some("   "),
                bucket_root: Some(PathBuf::from("/store")),
                s3_bucket: None,
                s3_endpoint: None,
                s3_region: None,
                s3_prefix: None
            })
            .expect("blank backend → local"),
            local("/store"),
        );
    }

    /// Scenario RFC0019.6 — config governed by RFC 0004; no secret leakage.
    /// See `docs/rfcs/0019-storage-backend-selection.md` §5.
    #[test]
    fn rfc0019_6_config_governed_no_secret_leakage() {
        // A missing S3 bucket names only the *key*, never a value, and config
        // resolution never reads credentials (those come from the AWS chain in
        // `StoreConfig::open`), so no secret can appear in an error.
        let err = build_store_config(StoreInputs {
            backend: Some("s3"),
            bucket_root: None,
            s3_bucket: None,
            s3_endpoint: None,
            s3_region: None,
            s3_prefix: None,
        })
        .expect_err("missing bucket");
        assert!(
            err.contains("OURIOS_S3_BUCKET"),
            "the error names the missing key, got {err:?}",
        );
        // The credential env vars are never echoed in a config error — neither
        // the AWS-chain names nor the explicit OURIOS_S3_* keys (RFC 0019 §3.4).
        for secret_key in [
            "AWS_SECRET_ACCESS_KEY",
            "AWS_ACCESS_KEY_ID",
            "OURIOS_S3_SECRET_ACCESS_KEY",
            "OURIOS_S3_ACCESS_KEY_ID",
            "OURIOS_S3_SESSION_TOKEN",
        ] {
            assert!(
                !err.contains(secret_key),
                "a credential key must not appear in a config error, got {err:?}",
            );
        }
    }

    /// RFC0019.8 (config layer) — explicit S3 credentials are applied to an
    /// `s3` `StoreConfig`, a present-but-blank value reads as unset, and a
    /// `local` config carries none. The pairing/validation and the redaction of
    /// any build error live in `ourios_parquet::Store::s3` (covered there).
    /// See `docs/rfcs/0019-storage-backend-selection.md` §3.4 / §5 (RFC0019.8).
    #[test]
    fn rfc0019_8_explicit_s3_credentials_applied() {
        let s3 = with_s3_credentials(
            StoreConfig::S3(S3Config::new("b")),
            Some("AKIAEXAMPLE"),
            Some("s3cr3t"),
            Some("tok"),
        );
        assert_eq!(
            s3,
            StoreConfig::S3(
                S3Config::new("b")
                    .with_access_key_id("AKIAEXAMPLE")
                    .with_secret_access_key("s3cr3t")
                    .with_session_token("tok"),
            ),
        );
        // A present-but-blank credential reads as unset (so it can't trip the
        // partial-pair check at store-build time).
        let blank =
            with_s3_credentials(StoreConfig::S3(S3Config::new("b")), Some("  "), None, None);
        assert_eq!(blank, StoreConfig::S3(S3Config::new("b")));
        // A local backend carries no credentials — passes through untouched.
        let local_cfg = with_s3_credentials(local("/store"), Some("x"), Some("y"), None);
        assert_eq!(local_cfg, local("/store"));
    }

    /// Scenario RFC0019.7 — local backend regression (the default path).
    /// See `docs/rfcs/0019-storage-backend-selection.md` §5.
    #[test]
    fn rfc0019_7_local_backend_regression() {
        // The default (no `OURIOS_STORAGE_BACKEND`, a bucket root set) resolves
        // to exactly the local store used before RFC 0019 — the
        // receiver/querier/compactor behaviour is then guarded by their
        // existing local suites, unchanged.
        let config = build_config(
            build_store_config(StoreInputs {
                backend: None,
                bucket_root: Some(PathBuf::from("/store")),
                s3_bucket: None,
                s3_endpoint: None,
                s3_region: None,
                s3_prefix: None,
            })
            .expect("default local"),
            CompactionInputs {
                enabled: None,
                interval_secs: None,
            },
            None,
            None,
            PromotedAttributes::default(),
            None,
        )
        .expect("valid");
        assert_eq!(config.store, local("/store"));
        assert!(config.compaction_enabled, "compaction is on by default");
    }

    #[test]
    fn build_receiver_config_disabled_unless_explicitly_enabled() {
        // Arrange / Act / Assert — unset or a falsey value disables the role.
        for raw in [None, Some("0"), Some("false"), Some("nope")] {
            assert_eq!(
                build_receiver_config(ReceiverInputs {
                    enabled: raw,
                    grpc_addr: None,
                    http_addr: None,
                    wal_root: Some(PathBuf::from("/wal")),
                    encode_workers: None,
                    ..ReceiverInputs::default()
                })
                .expect("ok"),
                None,
                "receiver disabled for enabled_raw = {raw:?}",
            );
        }
    }

    #[test]
    fn build_receiver_config_enabled_defaults_the_addresses() {
        // Arrange / Act
        let params = build_receiver_config(ReceiverInputs {
            enabled: Some("1"),
            grpc_addr: None,
            http_addr: None,
            wal_root: Some(PathBuf::from("/wal")),
            encode_workers: None,
            ..ReceiverInputs::default()
        })
        .expect("ok")
        .expect("enabled");

        // Assert
        assert_eq!(params.grpc_addr, DEFAULT_GRPC_ADDR.parse().unwrap());
        assert_eq!(params.http_addr, DEFAULT_HTTP_ADDR.parse().unwrap());
        assert_eq!(params.wal_root, PathBuf::from("/wal"));
    }

    #[test]
    fn build_receiver_config_parses_custom_addresses() {
        // Arrange / Act
        let params = build_receiver_config(ReceiverInputs {
            enabled: Some("yes"),
            grpc_addr: Some("127.0.0.1:1"),
            http_addr: Some("127.0.0.1:2"),
            wal_root: Some(PathBuf::from("/wal")),
            encode_workers: None,
            ..ReceiverInputs::default()
        })
        .expect("ok")
        .expect("enabled");

        // Assert
        assert_eq!(params.grpc_addr, "127.0.0.1:1".parse().unwrap());
        assert_eq!(params.http_addr, "127.0.0.1:2".parse().unwrap());
    }

    #[test]
    fn build_receiver_config_requires_a_wal_root_when_enabled() {
        // Arrange / Act / Assert — the WAL root is mandatory (and must be
        // non-empty) once the receiver role is on.
        assert!(
            build_receiver_config(ReceiverInputs {
                enabled: Some("1"),
                grpc_addr: None,
                http_addr: None,
                wal_root: None,
                encode_workers: None,
                ..ReceiverInputs::default()
            })
            .is_err(),
            "a missing WAL root is rejected",
        );
        assert!(
            build_receiver_config(ReceiverInputs {
                enabled: Some("1"),
                grpc_addr: None,
                http_addr: None,
                wal_root: Some(PathBuf::from("")),
                encode_workers: None,
                ..ReceiverInputs::default()
            })
            .is_err(),
            "an empty WAL root is rejected",
        );
    }

    #[test]
    fn build_receiver_config_encode_workers_defaults_and_validates() {
        // RFC 0035: unset → available cores (≥ 1); explicit values parse;
        // zero / junk are startup errors.
        let params = build_receiver_config(ReceiverInputs {
            enabled: Some("1"),
            grpc_addr: None,
            http_addr: None,
            wal_root: Some(PathBuf::from("/wal")),
            encode_workers: None,
            ..ReceiverInputs::default()
        })
        .expect("ok")
        .expect("enabled");
        assert!(params.encode_workers >= 1, "the default is at least one");

        let params = build_receiver_config(ReceiverInputs {
            enabled: Some("1"),
            grpc_addr: None,
            http_addr: None,
            wal_root: Some(PathBuf::from("/wal")),
            encode_workers: Some("3"),
            ..ReceiverInputs::default()
        })
        .expect("ok")
        .expect("enabled");
        assert_eq!(params.encode_workers, 3);

        for bad in ["0", "-1", "many"] {
            assert!(
                build_receiver_config(ReceiverInputs {
                    enabled: Some("1"),
                    grpc_addr: None,
                    http_addr: None,
                    wal_root: Some(PathBuf::from("/wal")),
                    encode_workers: Some(bad),
                    ..ReceiverInputs::default()
                })
                .is_err(),
                "encode_workers = {bad:?} is rejected",
            );
        }
    }

    #[test]
    fn build_receiver_config_rejects_a_malformed_address() {
        // Arrange / Act / Assert
        assert!(
            build_receiver_config(ReceiverInputs {
                enabled: Some("1"),
                grpc_addr: Some("not-an-addr"),
                http_addr: None,
                wal_root: Some(PathBuf::from("/wal")),
                encode_workers: None,
                ..ReceiverInputs::default()
            })
            .is_err(),
            "a malformed bind address is rejected",
        );
    }

    #[test]
    fn build_querier_config_disabled_unless_explicitly_enabled() {
        // Arrange / Act / Assert — unset or a falsey value disables the role.
        for raw in [None, Some("0"), Some("false"), Some("nope")] {
            assert_eq!(
                build_querier_config(QuerierInputs {
                    enabled: raw,
                    http_addr: None,
                    default_window_secs: None,
                    mcp_enabled: None,
                    http_tls: None
                })
                .expect("ok"),
                None,
                "querier disabled for enabled_raw = {raw:?}",
            );
        }
    }

    #[test]
    fn build_querier_config_enabled_defaults_address_and_window() {
        // Arrange / Act
        let params = build_querier_config(QuerierInputs {
            enabled: Some("1"),
            http_addr: None,
            default_window_secs: None,
            mcp_enabled: None,
            http_tls: None,
        })
        .expect("ok")
        .expect("enabled");

        // Assert
        assert_eq!(params.http_addr, DEFAULT_QUERIER_HTTP_ADDR.parse().unwrap());
        assert_eq!(
            params.default_window_nanos,
            DEFAULT_QUERIER_WINDOW_SECS * NANOS_PER_SEC,
        );
    }

    #[test]
    fn build_querier_config_parses_custom_address_and_window() {
        // Arrange / Act
        let params = build_querier_config(QuerierInputs {
            enabled: Some("yes"),
            http_addr: Some("127.0.0.1:9"),
            default_window_secs: Some("120"),
            mcp_enabled: None,
            http_tls: None,
        })
        .expect("ok")
        .expect("enabled");

        // Assert
        assert_eq!(params.http_addr, "127.0.0.1:9".parse().unwrap());
        assert_eq!(params.default_window_nanos, 120 * NANOS_PER_SEC);
    }

    #[test]
    fn build_querier_config_rejects_a_zero_or_nonnumeric_window() {
        // Arrange / Act / Assert — a zero window would make every no-`range`
        // query empty; a non-numeric value is a config typo.
        assert!(
            build_querier_config(QuerierInputs {
                enabled: Some("1"),
                http_addr: None,
                default_window_secs: Some("0"),
                mcp_enabled: None,
                http_tls: None
            })
            .is_err(),
            "a zero default window is rejected",
        );
        assert!(
            build_querier_config(QuerierInputs {
                enabled: Some("1"),
                http_addr: None,
                default_window_secs: Some("soon"),
                mcp_enabled: None,
                http_tls: None
            })
            .is_err(),
            "a non-numeric default window is rejected",
        );
    }

    #[test]
    fn build_querier_config_rejects_a_malformed_address() {
        // Arrange / Act / Assert
        assert!(
            build_querier_config(QuerierInputs {
                enabled: Some("1"),
                http_addr: Some("not-an-addr"),
                default_window_secs: None,
                mcp_enabled: None,
                http_tls: None
            })
            .is_err(),
            "a malformed bind address is rejected",
        );
    }
}
