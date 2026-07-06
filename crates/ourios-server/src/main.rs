//! `ourios-server` — the Ourios binary (`CLAUDE.md` §1, §7).
//!
//! It boots OpenTelemetry (the OTLP push `MeterProvider`, RFC 0001 §6.8) and,
//! unless `OURIOS_COMPACTION_ENABLED` is set falsey, runs the **background
//! compaction role** (RFC 0009 §3.2) — opening a durable audit sink for the
//! §3.6 compaction events (RFC 0005 §3.7) and sweeping until shutdown. A
//! multi-pod deployment disables it on the receiver/querier pods so a single
//! dedicated compactor sweeps.
//!
//! When `OURIOS_RECEIVER_ENABLED` is set it also runs the **OTLP receiver
//! role** (RFC 0003 §6.2 / the §9 process-model resolution): gRPC + HTTP
//! listeners over one shared pipeline (see [`receiver`]). When
//! `OURIOS_QUERIER_ENABLED` is set it runs the **querier role** (RFC 0016):
//! the HTTP query API over the logs DSL (`ourios_server::querier`), reading
//! the same `OURIOS_BUCKET_ROOT` store. Every role shares the tokio runtime
//! and shuts down gracefully on SIGINT or SIGTERM (the latter is what k8s /
//! `nerdctl stop` send), then telemetry flushes.
//!
//! Configuration comes from `OURIOS_*` environment variables, or — when
//! `--config <path>` is given — from a YAML file with `${env:…}` substitution
//! (RFC 0020). With `--config` the file is the sole source of Ourios's
//! configuration and bare `OURIOS_*` env vars do not override it; both paths
//! resolve the same [`ServerConfig`] through the same `build_*` validators.
//!
//! Logs are dogfooded (`CLAUDE.md` §6.3): everything after the telemetry
//! bootstrap logs through `tracing`, which `ourios-telemetry` bridges to an
//! `OTel` log record pushed over OTLP — Ourios's own logs travel the same
//! protocol its users' logs arrive on — with a human-readable copy on stderr.
//! stdout stays reserved for the machine-parsed start-up lines (the
//! bound-port announcements integration tests read).

#![deny(unsafe_code)]

mod receiver;

use std::error::Error;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use ourios_ingester::Compactor;
use ourios_parquet::{
    CompactionPolicy, ParquetAuditSink, PromotedAttributes, S3Config, StoreConfig,
};
use ourios_server::config::file::FileConfig;
use ourios_telemetry::TelemetryConfig;
use ourios_wal::WalConfig;

/// Default compaction sweep cadence when `OURIOS_COMPACTION_INTERVAL_SECS`
/// is unset.
const DEFAULT_COMPACTION_INTERVAL_SECS: u64 = 300;

/// Default OTLP/gRPC bind address (port 4317, the OTLP default).
const DEFAULT_GRPC_ADDR: &str = "0.0.0.0:4317";
/// Default OTLP/HTTP bind address (port 4318, the OTLP default).
const DEFAULT_HTTP_ADDR: &str = "0.0.0.0:4318";
/// Default querier HTTP bind address (port 4319, adjacent to the OTLP
/// receiver ports).
const DEFAULT_QUERIER_HTTP_ADDR: &str = "0.0.0.0:4319";
/// Default look-back window for a query with no `range(...)` stage — one
/// hour (RFC 0002 §4 P5; RFC 0016 §7).
const DEFAULT_QUERIER_WINDOW_SECS: u64 = 3600;
/// Nanoseconds per second — the unit the DSL compiler's window is in.
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Resolved server configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerConfig {
    /// The data + audit store backend (local or S3, RFC 0019).
    store: StoreConfig,
    /// Whether this process runs the background compaction sweep. Default on;
    /// `OURIOS_COMPACTION_ENABLED=0` disables it so a multi-pod deployment can
    /// run a single dedicated compactor rather than every pod sweeping (RFC 0009
    /// §3.2 — `publish_cas` keeps concurrent sweeps correct, but one sweeper
    /// avoids the redundant per-interval object listing).
    compaction_enabled: bool,
    /// How often the compaction daemon sweeps (when enabled).
    compaction_interval: Duration,
    /// The OTLP receiver role, if enabled (RFC 0003 §9).
    receiver: Option<ReceiverParams>,
    /// The querier role, if enabled (RFC 0016).
    querier: Option<QuerierParams>,
    /// The effective RFC 0022 promoted attribute set
    /// (`storage.promoted_attributes`, §3.2) — applied by every write path
    /// (receiver flushes and compaction rewrites; §3.4).
    promoted: PromotedAttributes,
    /// The RFC 0026 token store (`auth.tokens`), or `None` for open mode.
    /// Config-file only (§3.1 — tokens ride the `${env:…}` indirection); the
    /// env-only path always resolves open. Enforcement on the listeners lands
    /// with the ingest/query slices; this slice resolves and validates the
    /// store and makes open mode observable at startup.
    auth: Option<ourios_server::auth::TokenStore>,
}

/// Resolved querier-role configuration (RFC 0016 §3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
struct QuerierParams {
    http_addr: SocketAddr,
    default_window_nanos: u64,
    /// Serve the RFC 0027 MCP surface at `/mcp` (`querier.mcp.enabled` /
    /// `OURIOS_QUERIER_MCP_ENABLED`; default off).
    mcp_enabled: bool,
}

/// Resolved OTLP-receiver-role configuration (RFC 0003 §6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReceiverParams {
    grpc_addr: SocketAddr,
    http_addr: SocketAddr,
    wal_root: PathBuf,
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
fn config_from_env() -> Result<ServerConfig, String> {
    let store = build_store_config(
        std::env::var("OURIOS_STORAGE_BACKEND").ok().as_deref(),
        std::env::var_os("OURIOS_BUCKET_ROOT").map(PathBuf::from),
        std::env::var("OURIOS_S3_BUCKET").ok().as_deref(),
        std::env::var("OURIOS_S3_ENDPOINT").ok().as_deref(),
        std::env::var("OURIOS_S3_REGION").ok().as_deref(),
        std::env::var("OURIOS_S3_PREFIX").ok().as_deref(),
    )?;
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
    let interval_raw = std::env::var("OURIOS_COMPACTION_INTERVAL_SECS").ok();
    let mut config = build_config(
        store,
        std::env::var("OURIOS_COMPACTION_ENABLED").ok().as_deref(),
        interval_raw.as_deref(),
    )?;
    config.receiver = build_receiver_config(
        std::env::var("OURIOS_RECEIVER_ENABLED").ok().as_deref(),
        std::env::var("OURIOS_RECEIVER_GRPC_ADDR").ok().as_deref(),
        std::env::var("OURIOS_RECEIVER_HTTP_ADDR").ok().as_deref(),
        std::env::var_os("OURIOS_WAL_ROOT").map(PathBuf::from),
    )?;
    config.querier = build_querier_config(
        std::env::var("OURIOS_QUERIER_ENABLED").ok().as_deref(),
        std::env::var("OURIOS_QUERIER_HTTP_ADDR").ok().as_deref(),
        std::env::var("OURIOS_QUERIER_DEFAULT_WINDOW_SECS")
            .ok()
            .as_deref(),
        std::env::var("OURIOS_QUERIER_MCP_ENABLED").ok().as_deref(),
    )?;
    Ok(config)
}

/// Ourios log-storage server (`CLAUDE.md` §1).
///
/// Configuration is read from the `--config` file (RFC 0020) when given,
/// otherwise from `OURIOS_*` environment variables.
// A derived `clap` parser (rather than hand-rolled) for `--help`/`--version`,
// usage, and argument-error handling (missing value, unknown flag, trailing
// arguments) — the RFC 0020 §3.2 CLI contract, for free.
#[derive(Debug, clap::Parser)]
#[command(name = "ourios-server", version, about = "Ourios log-storage server")]
struct Cli {
    /// Path to a YAML configuration file (RFC 0020). When given, the file is the
    /// sole source of configuration and the environment participates only through
    /// `${env:…}` substitution inside it; without it, configuration comes from
    /// `OURIOS_*` environment variables.
    #[arg(long, value_name = "PATH", value_parser = non_empty_path)]
    config: Option<PathBuf>,
}

/// A `--config` value parser that rejects an empty path (a required argument
/// must name a file), yielding a clear `clap` error rather than a later
/// file-not-found on `""`.
fn non_empty_path(value: &str) -> Result<PathBuf, String> {
    if value.is_empty() {
        Err("the config path must not be empty".to_owned())
    } else {
        Ok(PathBuf::from(value))
    }
}

/// Resolve [`ServerConfig`] from a YAML configuration file (RFC 0020). The file
/// is the **sole** source of Ourios's configuration; the environment
/// participates only through `${env:…}` substitution inside it (§3.2), so a bare
/// `OURIOS_*` env var never overrides a file value.
fn config_from_file(path: &Path) -> Result<ServerConfig, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read config file {}: {e}", path.display()))?;
    let file = ourios_server::config::file::parse(&text, &|name| std::env::var(name).ok())
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
fn server_config_from_file(file: &FileConfig) -> Result<ServerConfig, String> {
    let store = build_store_config(
        file.storage.backend.as_deref(),
        file.storage.local.bucket_root.as_deref().map(PathBuf::from),
        file.storage.s3.bucket.as_deref(),
        file.storage.s3.endpoint.as_deref(),
        file.storage.s3.region.as_deref(),
        file.storage.s3.prefix.as_deref(),
    )?;
    let store = with_s3_credentials(
        store,
        file.storage.s3.access_key_id.as_deref(),
        file.storage.s3.secret_access_key.as_deref(),
        file.storage.s3.session_token.as_deref(),
    );
    let mut config = build_config(
        store,
        file.compaction.enabled.as_deref(),
        file.compaction.interval_secs.as_deref(),
    )?;
    config.receiver = build_receiver_config(
        file.receiver.enabled.as_deref(),
        file.receiver.grpc_addr.as_deref(),
        file.receiver.http_addr.as_deref(),
        file.receiver.wal_root.as_deref().map(PathBuf::from),
    )?;
    config.querier = build_querier_config(
        file.querier.enabled.as_deref(),
        file.querier.http_addr.as_deref(),
        file.querier.default_window_secs.as_deref(),
        file.querier.mcp.enabled.as_deref(),
    )?;
    config.promoted = build_promoted_attributes(
        &file.storage.promoted_attributes.resource,
        &file.storage.promoted_attributes.log,
    )?;
    config.auth = ourios_server::auth::build_token_store(file.auth.as_ref())?;
    Ok(config)
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
fn build_store_config(
    backend_raw: Option<&str>,
    bucket_root: Option<PathBuf>,
    s3_bucket: Option<&str>,
    s3_endpoint: Option<&str>,
    s3_region: Option<&str>,
    s3_prefix: Option<&str>,
) -> Result<StoreConfig, String> {
    // Trim and treat empty as unset, so " s3 " selects S3 and a blank value
    // falls back to the local default rather than reading as an unknown backend.
    match backend_raw.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("local") => {
            let root = bucket_root
                .ok_or("OURIOS_BUCKET_ROOT must be set (the local data + audit store root)")?;
            if root.as_os_str().is_empty() {
                return Err("OURIOS_BUCKET_ROOT must not be empty".to_string());
            }
            Ok(StoreConfig::Local(root))
        }
        Some("s3") => {
            let bucket = s3_bucket
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .ok_or("OURIOS_S3_BUCKET must be set when OURIOS_STORAGE_BACKEND=s3")?;
            let mut cfg = S3Config::new(bucket);
            if let Some(endpoint) = s3_endpoint.map(str::trim).filter(|v| !v.is_empty()) {
                cfg = cfg.with_endpoint(endpoint);
            }
            if let Some(region) = s3_region.map(str::trim).filter(|v| !v.is_empty()) {
                cfg = cfg.with_region(region);
            }
            if let Some(prefix) = s3_prefix.map(str::trim).filter(|v| !v.is_empty()) {
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
fn with_s3_credentials(
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
/// - `enabled_raw` — `OURIOS_QUERIER_ENABLED` (`1`/`true`/`yes` enables).
/// - `http_raw` — `OURIOS_QUERIER_HTTP_ADDR` (default
///   [`DEFAULT_QUERIER_HTTP_ADDR`]).
/// - `window_raw` — `OURIOS_QUERIER_DEFAULT_WINDOW_SECS` (default
///   [`DEFAULT_QUERIER_WINDOW_SECS`]); must be a non-zero integer of seconds.
fn build_querier_config(
    enabled_raw: Option<&str>,
    http_raw: Option<&str>,
    window_raw: Option<&str>,
    mcp_enabled_raw: Option<&str>,
) -> Result<Option<QuerierParams>, String> {
    if !matches!(enabled_raw, Some("1" | "true" | "yes")) {
        return Ok(None);
    }
    // Opt-in like the roles themselves (RFC 0027 §3.1; default off).
    let mcp_enabled = matches!(mcp_enabled_raw, Some("1" | "true" | "yes"));
    let http_addr = parse_addr(http_raw, DEFAULT_QUERIER_HTTP_ADDR)?;
    let window_secs = match window_raw {
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
    Ok(Some(QuerierParams {
        http_addr,
        default_window_nanos,
        mcp_enabled,
    }))
}

/// Pure receiver-config assembly + validation (env reads live in
/// [`config_from_env`]). `None` when the receiver role is disabled.
fn build_receiver_config(
    enabled_raw: Option<&str>,
    grpc_raw: Option<&str>,
    http_raw: Option<&str>,
    wal_root: Option<PathBuf>,
) -> Result<Option<ReceiverParams>, String> {
    if !matches!(enabled_raw, Some("1" | "true" | "yes")) {
        return Ok(None);
    }
    let grpc_addr = parse_addr(grpc_raw, DEFAULT_GRPC_ADDR)?;
    let http_addr = parse_addr(http_raw, DEFAULT_HTTP_ADDR)?;
    let wal_root = wal_root
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or("OURIOS_WAL_ROOT must be set when the receiver role is enabled")?;
    Ok(Some(ReceiverParams {
        grpc_addr,
        http_addr,
        wal_root,
    }))
}

/// Parse a socket address, falling back to `default` when unset.
fn parse_addr(raw: Option<&str>, default: &str) -> Result<SocketAddr, String> {
    let value = raw.unwrap_or(default);
    value
        .parse()
        .map_err(|e| format!("invalid socket address {value:?}: {e}"))
}

/// The receiver role's WAL config: `root` plus the workspace-standard
/// durability knobs (RFC 0008 §6.3).
fn wal_config(root: &Path) -> WalConfig {
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
/// [`config_from_env`]; this is the testable core).
fn build_config(
    store: StoreConfig,
    compaction_enabled_raw: Option<&str>,
    interval_raw: Option<&str>,
) -> Result<ServerConfig, String> {
    // Compaction is opt-*out* (default on), unlike the opt-in receiver/querier
    // roles: an explicit falsey value disables the sweep, anything else (incl.
    // unset) keeps it on.
    let compaction_enabled = !matches!(
        compaction_enabled_raw.map(str::trim),
        Some("0" | "false" | "no" | "off")
    );
    // Only parse/validate the interval when compaction is on — a pod with
    // compaction disabled must not fail to start over an interval it never uses
    // (the default is a placeholder there, never read).
    let compaction_interval = if compaction_enabled {
        match interval_raw {
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
        receiver: None,
        querier: None,
        promoted: PromotedAttributes::default(),
        auth: None,
    })
}

/// Resolve `storage.promoted_attributes` (RFC 0022 §3.2) into the effective
/// promoted set. Keys are taken literally (no globbing), so a key that is
/// empty or carries surrounding whitespace — e.g. an `${env:…}` reference
/// that resolved to nothing, or a quoted `" key"` — is a config error rather
/// than a silently never-matching promoted column. Deduplication and the
/// implicit `service.name` are [`PromotedAttributes::new`]'s contract.
fn build_promoted_attributes(
    resource: &[String],
    log: &[String],
) -> Result<PromotedAttributes, String> {
    if resource
        .iter()
        .chain(log)
        .any(|k| k.is_empty() || k.trim() != k)
    {
        return Err(
            "storage.promoted_attributes keys must be non-empty attribute names \
                    without surrounding whitespace"
                .to_string(),
        );
    }
    Ok(PromotedAttributes::new(
        resource.iter().cloned(),
        log.iter().cloned(),
    ))
}

/// Resolve when the process receives `SIGTERM` (what k8s / `nerdctl stop`
/// send). Non-Unix targets have no `SIGTERM`, so this never resolves and
/// SIGINT (`ctrl_c`) stays the shutdown path; a SIGTERM-handler install
/// failure is logged and likewise leaves SIGINT in charge.
async fn terminate_signal() {
    #[cfg(unix)]
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut sigterm) => {
            sigterm.recv().await;
        }
        Err(e) => {
            tracing::error!(name: ourios_semconv::EVENT_OURIOS_SERVER_SIGNAL_HANDLER_ERROR, "install SIGTERM handler (SIGINT remains the shutdown path): {e}");
            std::future::pending::<()>().await;
        }
    }
    #[cfg(not(unix))]
    std::future::pending::<()>().await;
}

/// RFC 0026 §3.1 open mode: with no `auth` configured, any client that can
/// reach a listener can write into and read from any tenant. Warn once at
/// startup so the exposure is a visible choice, not a silent default. A
/// compactor-only process binds nothing, so it has nothing to expose.
fn warn_if_open_mode(config: &ServerConfig) {
    if config.auth.is_none() && (config.receiver.is_some() || config.querier.is_some()) {
        tracing::warn!(
            name: ourios_semconv::EVENT_OURIOS_SERVER_AUTH_OPEN_MODE,
            "auth is not configured: the network listeners accept unauthenticated \
             requests for any tenant (RFC 0026 open mode)"
        );
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // `--config <path>` selects the RFC 0020 file front-end; without it the
    // env-only path runs unchanged (§3.2). Both resolve the same `ServerConfig`.
    let cli = Cli::parse();
    let config = match cli.config.as_deref() {
        Some(path) => config_from_file(path)?,
        None => config_from_env()?,
    };

    // Pre-create a local store root (`Store::local` canonicalises it and errors
    // on a missing dir); an S3 backend needs no such step. Mirrors the querier
    // role's `serve()`.
    if let StoreConfig::Local(root) = &config.store {
        std::fs::create_dir_all(root)
            .map_err(|e| format!("create store root {}: {e}", root.display()))?;
    }

    // Preflight the data store *before* binding any network role, so a
    // store-open failure early-returns here rather than after the
    // receiver/querier handles are live — which would bypass their graceful
    // shutdown. `open` only validates local-root existence / backend config; an
    // S3 backend doesn't contact the endpoint here (credentials and connectivity
    // resolve on first request, surfacing later). This opened handle is cloned
    // into the receiver and moved into `Compactor::new` below (both write/sweep
    // the same store); the querier opens its own handle from the same
    // `StoreConfig` in `querier::serve`.
    let store = config.store.open()?;

    // Boot OpenTelemetry first so the compactor's instruments export
    // (RFC 0001 §6.8). The guard flushes pending metrics on shutdown;
    // OTEL_EXPORTER_OTLP_ENDPOINT et al. tune the exporter.
    let telemetry = ourios_telemetry::init(&TelemetryConfig::new("ourios-server"))?;

    warn_if_open_mode(&config);

    // Start the OTLP receiver role if enabled (RFC 0003 §9). Report the
    // bound addresses on stdout so an operator — or a test binding `:0` —
    // learns the actual ports.
    let receiver = match &config.receiver {
        // The receiver's RFC 0014 data write path runs on the resolved store
        // (local or S3, RFC 0019 slice 2c) — the same store the querier reads
        // and the compactor sweeps. The WAL stays local regardless (§3.6).
        Some(params) => {
            let handle = receiver::serve(receiver::ReceiverConfig {
                grpc_addr: params.grpc_addr,
                http_addr: params.http_addr,
                wal: wal_config(&params.wal_root),
                // The data store the receiver's RFC 0014 write path lands
                // Parquet in — the same store the compactor sweeps (cloned; the
                // handle is cheap to share, the compactor keeps the original).
                store: store.clone(),
                promoted: config.promoted.clone(),
                auth: config.auth.clone().map(std::sync::Arc::new),
            })
            .await?;
            println!("receiver gRPC listening on {}", handle.grpc_addr);
            println!("receiver HTTP listening on {}", handle.http_addr);
            std::io::stdout().flush().ok();
            Some(handle)
        }
        None => None,
    };

    // Start the querier role if enabled (RFC 0016), over the same store the
    // receiver writes and the compactor sweeps. Report the bound address on
    // stdout (an operator — or a test binding `:0` — learns the actual port).
    let querier = match &config.querier {
        Some(params) => {
            let handle = ourios_server::querier::serve(ourios_server::querier::QuerierConfig {
                http_addr: params.http_addr,
                // The querier engine is Store-capable (RFC 0019 slice 2a), so it
                // reads whichever backend config resolved (local or S3).
                store: config.store.clone(),
                auth: config.auth.clone().map(std::sync::Arc::new),
                default_window_nanos: params.default_window_nanos,
                mcp_enabled: params.mcp_enabled,
            })
            .await?;
            println!("querier HTTP listening on {}", handle.http_addr);
            std::io::stdout().flush().ok();
            Some(handle)
        }
        None => None,
    };

    // The compactor sweeps the resolved store (local or S3, RFC 0019 slice 2b),
    // opened in the preflight above so a store failure never leaks a live role,
    // and writes durable compaction audit events through the same `Store` via
    // the `ParquetAuditSink` (RFC 0009 §3.6 → RFC 0005 §3.7, slice 2d). Built
    // only when compaction is enabled (RFC 0009 §3.2) — a deployment disables it
    // on receiver/querier pods so a single dedicated compactor sweeps. When
    // disabled, neither the store clone nor the audit sink is constructed, and
    // the disabled state is logged so it's visible in a multi-pod rollout.
    let compactor = if config.compaction_enabled {
        let audit_store = store.clone();
        Some(
            Compactor::new(
                store,
                CompactionPolicy::default(),
                config.compaction_interval,
            )
            .with_promoted_attributes(config.promoted.clone())
            .with_audit_sink(Box::new(ParquetAuditSink::new(audit_store))),
        )
    } else {
        tracing::info!(name: ourios_semconv::EVENT_OURIOS_SERVER_COMPACTION_DISABLED, "compaction disabled for this process (OURIOS_COMPACTION_ENABLED)");
        None
    };

    // Run until SIGINT or SIGTERM (k8s / `nerdctl stop` send SIGTERM). The
    // compaction loop never returns on its own (it sweeps forever, or just
    // pends when disabled), so the select resolves on a signal (or a SIGINT
    // setup failure).
    let compaction = async {
        match compactor {
            Some(c) => {
                c.run(|result| match result {
                    Ok(report) => {
                        for err in &report.errors {
                            tracing::error!(name: ourios_semconv::EVENT_OURIOS_COMPACTION_SWEEP_ERROR, "compaction sweep error: {err}");
                        }
                    }
                    Err(e) => tracing::error!(name: ourios_semconv::EVENT_OURIOS_COMPACTION_SWEEP_ERROR, "compaction sweep failed: {e}"),
                })
                .await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    let shutdown = tokio::select! {
        () = compaction => Ok(()),
        signal = tokio::signal::ctrl_c() => signal,
        () = terminate_signal() => Ok(()),
    };

    // Drain the listeners gracefully (the receiver release frees the single
    // `Wal`) before flushing telemetry and exiting.
    if let Some(handle) = querier
        && let Err(e) = handle.shutdown().await
    {
        tracing::error!(name: ourios_semconv::EVENT_OURIOS_QUERIER_SHUTDOWN_ERROR, "querier shutdown error: {e}");
    }
    if let Some(handle) = receiver
        && let Err(e) = handle.shutdown().await
    {
        tracing::error!(name: ourios_semconv::EVENT_OURIOS_RECEIVER_SHUTDOWN_ERROR, "receiver shutdown error: {e}");
    }

    // Flush pending telemetry on the way out (best-effort: a failed final
    // export — e.g. the metrics collector is unreachable at shutdown —
    // must not turn an otherwise-clean shutdown into a non-zero exit).
    // eprintln!, not tracing: the log pipeline this tears down is the one
    // a tracing event would need, so stderr is the only channel left.
    if let Err(e) = telemetry.shutdown() {
        eprintln!("telemetry shutdown error: {e}");
    }

    // A SIGINT (`ctrl_c`) handler setup failure is fatal: cancelling the
    // compactor and exiting 0 would leave the server silently doing no
    // work. (A SIGTERM-handler failure is non-fatal — see
    // `terminate_signal` — leaving SIGINT in charge.)
    shutdown?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use ourios_server::config::file::parse;

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
            build_store_config(Some("s3"), None, Some("my-logs"), None, None, None).expect("s3"),
            None,
            None,
            None,
        );
        let mut expected = build_config(store, None, Some("120")).expect("valid");
        expected.receiver = build_receiver_config(
            Some("true"),
            None,
            None,
            Some(PathBuf::from("/var/lib/ourios/wal")),
        )
        .expect("receiver");
        expected.querier = build_querier_config(Some("true"), None, None, None).expect("querier");

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

    /// Scenario RFC0020.4 — no `--config` selects the env-only path unchanged;
    /// the `--config` CLI contract is enforced by `clap`.
    /// See `docs/rfcs/0020-configuration-file.md` §5.
    #[test]
    fn rfc0020_4_no_config_flag_selects_the_env_path() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|cli| cli.config);

        // No flag → None → `config_from_env` runs (its behaviour is unchanged,
        // guarded by the `build_*`/`config_from_env` suites).
        assert_eq!(parse(&["ourios-server"]).expect("ok"), None);
        // `--config <path>` and `--config=<path>` both select the file.
        assert_eq!(
            parse(&["ourios-server", "--config", "/c.yaml"]).expect("ok"),
            Some(PathBuf::from("/c.yaml")),
        );
        assert_eq!(
            parse(&["ourios-server", "--config=/c.yaml"]).expect("ok"),
            Some(PathBuf::from("/c.yaml")),
        );
        // A dangling `--config`, an empty path, a trailing extra argument, and an
        // unknown argument are all rejected (clap enforces the CLI contract).
        assert!(parse(&["ourios-server", "--config"]).is_err());
        assert!(parse(&["ourios-server", "--config="]).is_err());
        assert!(parse(&["ourios-server", "--config", "/c.yaml", "--extra"]).is_err());
        assert!(parse(&["ourios-server", "--config=/c.yaml", "x"]).is_err());
        assert!(parse(&["ourios-server", "--nope"]).is_err());
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
    /// `config::file`, the store validation matrix in `ourios_server::auth`,
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
        let store = config.auth.expect("auth resolved");
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

    /// RFC 0027 §3.1 — the MCP flag is opt-in: absent/falsey values leave
    /// it off, the role-standard truthy values enable it, on both the env
    /// and file paths (the same `build_querier_config`).
    #[test]
    fn querier_mcp_flag_defaults_off_and_accepts_truthy() {
        for off in [None, Some("0"), Some("false"), Some("off"), Some("")] {
            let params = build_querier_config(Some("1"), None, None, off)
                .expect("valid")
                .expect("enabled");
            assert!(!params.mcp_enabled, "{off:?} leaves MCP off");
        }
        for on in ["1", "true", "yes"] {
            let params = build_querier_config(Some("1"), None, None, Some(on))
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
        use std::io::Write as _;

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
        let config = build_config(local("/store"), None, None).expect("valid");

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

        let err = build_promoted_attributes(&["k8s.namespace.name".to_string()], &[String::new()])
            .expect_err("empty key");
        assert!(err.contains("non-empty"), "the error names the rule: {err}");
        // Surrounding whitespace would mint a promoted column whose name can
        // never match the intended attribute key — rejected, not normalised.
        build_promoted_attributes(&[" k8s.namespace.name".to_string()], &[])
            .expect_err("whitespace-padded key");
        build_promoted_attributes(&[], &["http.route ".to_string()])
            .expect_err("trailing-whitespace key");
    }

    #[test]
    fn build_config_parses_a_custom_interval() {
        // Arrange / Act
        let config = build_config(local("/store"), None, Some("60")).expect("valid");

        // Assert
        assert_eq!(config.compaction_interval, Duration::from_secs(60));
    }

    #[test]
    fn build_config_rejects_a_zero_or_nonnumeric_interval() {
        // Arrange / Act / Assert
        assert!(
            build_config(local("/store"), None, Some("0")).is_err(),
            "a zero interval would busy-loop the daemon",
        );
        assert!(
            build_config(local("/store"), None, Some("soon")).is_err(),
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
                build_config(local("/store"), raw, None)
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
                !build_config(local("/store"), raw, None)
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
                build_config(local("/store"), Some("off"), bad).is_ok(),
                "a disabled pod starts despite interval {bad:?}",
            );
        }
    }

    /// Scenario RFC0019.1 — backend selection from config.
    /// See `docs/rfcs/0019-storage-backend-selection.md` §5.
    #[test]
    fn rfc0019_1_backend_selection_from_config() {
        // Unset backend + a bucket root → local.
        assert_eq!(
            build_store_config(None, Some(PathBuf::from("/store")), None, None, None, None)
                .expect("local default"),
            local("/store"),
        );
        // Explicit `local` behaves the same.
        assert_eq!(
            build_store_config(
                Some("local"),
                Some(PathBuf::from("/store")),
                None,
                None,
                None,
                None
            )
            .expect("explicit local"),
            local("/store"),
        );
        // `s3` + a bucket (and optional addressing) → an S3 backend.
        let s3 = build_store_config(
            Some("s3"),
            None,
            Some("my-bucket"),
            Some("http://localhost:4566"),
            Some("us-east-1"),
            Some("ourios"),
        )
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
            build_store_config(Some("s3"), None, None, None, None, None).is_err(),
            "s3 backend requires OURIOS_S3_BUCKET",
        );
        assert!(
            build_store_config(
                Some("gcs"),
                Some(PathBuf::from("/store")),
                None,
                None,
                None,
                None
            )
            .is_err(),
            "an unknown backend is rejected",
        );
        // Local backend with no bucket root is rejected — "must be set" for an
        // unset var, distinct from "must not be empty" for a present-but-empty
        // one (clearer operator diagnostics).
        let unset = build_store_config(None, None, None, None, None, None).expect_err("unset");
        assert!(
            unset.contains("must be set"),
            "unset names the missing key, got {unset:?}",
        );
        let empty = build_store_config(
            Some("local"),
            Some(PathBuf::from("")),
            None,
            None,
            None,
            None,
        )
        .expect_err("empty");
        assert!(
            empty.contains("must not be empty"),
            "an empty bucket root is reported distinctly, got {empty:?}",
        );
        // The backend value is trimmed; a blank value is treated as unset
        // (→ local), not as an unknown backend.
        assert_eq!(
            build_store_config(Some("  s3  "), None, Some("b"), None, None, None)
                .expect("trimmed s3"),
            StoreConfig::S3(S3Config::new("b")),
        );
        assert_eq!(
            build_store_config(
                Some("   "),
                Some(PathBuf::from("/store")),
                None,
                None,
                None,
                None
            )
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
        let err = build_store_config(Some("s3"), None, None, None, None, None)
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
            build_store_config(None, Some(PathBuf::from("/store")), None, None, None, None)
                .expect("default local"),
            None,
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
                build_receiver_config(raw, None, None, Some(PathBuf::from("/wal"))).expect("ok"),
                None,
                "receiver disabled for enabled_raw = {raw:?}",
            );
        }
    }

    #[test]
    fn build_receiver_config_enabled_defaults_the_addresses() {
        // Arrange / Act
        let params = build_receiver_config(Some("1"), None, None, Some(PathBuf::from("/wal")))
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
        let params = build_receiver_config(
            Some("yes"),
            Some("127.0.0.1:1"),
            Some("127.0.0.1:2"),
            Some(PathBuf::from("/wal")),
        )
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
            build_receiver_config(Some("1"), None, None, None).is_err(),
            "a missing WAL root is rejected",
        );
        assert!(
            build_receiver_config(Some("1"), None, None, Some(PathBuf::from(""))).is_err(),
            "an empty WAL root is rejected",
        );
    }

    #[test]
    fn build_receiver_config_rejects_a_malformed_address() {
        // Arrange / Act / Assert
        assert!(
            build_receiver_config(
                Some("1"),
                Some("not-an-addr"),
                None,
                Some(PathBuf::from("/wal"))
            )
            .is_err(),
            "a malformed bind address is rejected",
        );
    }

    #[test]
    fn build_querier_config_disabled_unless_explicitly_enabled() {
        // Arrange / Act / Assert — unset or a falsey value disables the role.
        for raw in [None, Some("0"), Some("false"), Some("nope")] {
            assert_eq!(
                build_querier_config(raw, None, None, None).expect("ok"),
                None,
                "querier disabled for enabled_raw = {raw:?}",
            );
        }
    }

    #[test]
    fn build_querier_config_enabled_defaults_address_and_window() {
        // Arrange / Act
        let params = build_querier_config(Some("1"), None, None, None)
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
        let params = build_querier_config(Some("yes"), Some("127.0.0.1:9"), Some("120"), None)
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
            build_querier_config(Some("1"), None, Some("0"), None).is_err(),
            "a zero default window is rejected",
        );
        assert!(
            build_querier_config(Some("1"), None, Some("soon"), None).is_err(),
            "a non-numeric default window is rejected",
        );
    }

    #[test]
    fn build_querier_config_rejects_a_malformed_address() {
        // Arrange / Act / Assert
        assert!(
            build_querier_config(Some("1"), Some("not-an-addr"), None, None).is_err(),
            "a malformed bind address is rejected",
        );
    }
}
