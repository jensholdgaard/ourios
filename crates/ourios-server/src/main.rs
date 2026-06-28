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
//! A structured-logging framework (`CLAUDE.md` §6.3 — errors go to stderr as
//! a stopgap here) is a follow-up.

#![deny(unsafe_code)]

mod receiver;

use std::error::Error;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ourios_ingester::Compactor;
use ourios_parquet::{CompactionPolicy, ParquetAuditSink, S3Config, StoreConfig};
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
}

/// Resolved querier-role configuration (RFC 0016 §3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
struct QuerierParams {
    http_addr: SocketAddr,
    default_window_nanos: u64,
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
///   applied over the standard chain (RFC 0019 §9); when unset, credentials
///   come from the chain (`AmazonS3Builder::from_env`, incl. IRSA). Never
///   logged (RFC 0019 §9.4 / §3.4).
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
    // Explicit S3 credentials (RFC 0019 §9), layered over the standard chain.
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
    )?;
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
/// and the chain is the fallback in [`StoreConfig::open`] (RFC 0019 §3.4/§9), so
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

/// Apply explicit S3 credentials (RFC 0019 §9) onto a resolved [`StoreConfig`].
///
/// Each value is trimmed and an empty string is treated as unset (matching the
/// addressing knobs), so a present-but-blank env var does not count as "set"
/// and trip the partial-pair check at store-build time. A `local` backend
/// carries no credentials, so it passes through unchanged. The pairing rule
/// (access key + secret together; a session token only with the pair) and the
/// secret-scrubbing of any resulting error are enforced in
/// `ourios_parquet::Store::s3`, which names only the offending field, never a
/// value (RFC 0019 §9.3/§9.4).
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
) -> Result<Option<QuerierParams>, String> {
    if !matches!(enabled_raw, Some("1" | "true" | "yes")) {
        return Ok(None);
    }
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
    })
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
            eprintln!("install SIGTERM handler: {e}");
            std::future::pending::<()>().await;
        }
    }
    #[cfg(not(unix))]
    std::future::pending::<()>().await;
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = config_from_env()?;

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
                default_window_nanos: params.default_window_nanos,
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
            .with_audit_sink(Box::new(ParquetAuditSink::new(audit_store))),
        )
    } else {
        println!("compaction disabled for this process (OURIOS_COMPACTION_ENABLED)");
        std::io::stdout().flush().ok();
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
                            eprintln!("compaction sweep error: {err}");
                        }
                    }
                    Err(e) => eprintln!("compaction sweep failed: {e}"),
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
    if let Some(handle) = querier {
        if let Err(e) = handle.shutdown().await {
            eprintln!("querier shutdown error: {e}");
        }
    }
    if let Some(handle) = receiver {
        if let Err(e) = handle.shutdown().await {
            eprintln!("receiver shutdown error: {e}");
        }
    }

    // Flush pending telemetry on the way out (best-effort: a failed final
    // export — e.g. the metrics collector is unreachable at shutdown —
    // must not turn an otherwise-clean shutdown into a non-zero exit).
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

    /// A `local` [`StoreConfig`] for `path`, the common test fixture.
    fn local(path: &str) -> StoreConfig {
        StoreConfig::Local(PathBuf::from(path))
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
        // the AWS-chain names nor the explicit OURIOS_S3_* keys (RFC 0019 §9.4).
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
    /// See `docs/rfcs/0019-storage-backend-selection.md` §9.
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
                build_querier_config(raw, None, None).expect("ok"),
                None,
                "querier disabled for enabled_raw = {raw:?}",
            );
        }
    }

    #[test]
    fn build_querier_config_enabled_defaults_address_and_window() {
        // Arrange / Act
        let params = build_querier_config(Some("1"), None, None)
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
        let params = build_querier_config(Some("yes"), Some("127.0.0.1:9"), Some("120"))
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
            build_querier_config(Some("1"), None, Some("0")).is_err(),
            "a zero default window is rejected",
        );
        assert!(
            build_querier_config(Some("1"), None, Some("soon")).is_err(),
            "a non-numeric default window is rejected",
        );
    }

    #[test]
    fn build_querier_config_rejects_a_malformed_address() {
        // Arrange / Act / Assert
        assert!(
            build_querier_config(Some("1"), Some("not-an-addr"), None).is_err(),
            "a malformed bind address is rejected",
        );
    }
}
