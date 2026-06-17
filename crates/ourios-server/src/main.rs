//! `ourios-server` — the Ourios binary (`CLAUDE.md` §1, §7).
//!
//! It always runs the **background compaction role** (RFC 0009 §3.2):
//! it boots OpenTelemetry (the OTLP push `MeterProvider`, RFC 0001 §6.8),
//! opens a durable audit sink for the §3.6 compaction events (RFC 0005
//! §3.7), and runs the compactor until shutdown.
//!
//! When `OURIOS_RECEIVER_ENABLED` is set it also runs the **OTLP receiver
//! role** (RFC 0003 §6.2 / the §9 process-model resolution): gRPC + HTTP
//! listeners over one shared pipeline (see [`receiver`]). Both roles
//! share the tokio runtime and shut down gracefully on SIGINT or SIGTERM
//! (the latter is what k8s / `nerdctl stop` send), then telemetry flushes.
//!
//! The querier (RFC 0007) role and a structured-logging framework
//! (`CLAUDE.md` §6.3 — errors go to stderr as a stopgap here) are
//! follow-ups.

#![deny(unsafe_code)]

mod receiver;

use std::error::Error;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ourios_ingester::Compactor;
use ourios_parquet::{CompactionPolicy, ParquetAuditSink};
use ourios_telemetry::TelemetryConfig;
use ourios_wal::WalConfig;

/// Default compaction sweep cadence when `OURIOS_COMPACTION_INTERVAL_SECS`
/// is unset.
const DEFAULT_COMPACTION_INTERVAL_SECS: u64 = 300;

/// Default OTLP/gRPC bind address (port 4317, the OTLP default).
const DEFAULT_GRPC_ADDR: &str = "0.0.0.0:4317";
/// Default OTLP/HTTP bind address (port 4318, the OTLP default).
const DEFAULT_HTTP_ADDR: &str = "0.0.0.0:4318";

/// Resolved server configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerConfig {
    /// Root of the data + audit store (the compactor sweeps under it).
    bucket_root: PathBuf,
    /// How often the compaction daemon sweeps.
    compaction_interval: Duration,
    /// The OTLP receiver role, if enabled (RFC 0003 §9).
    receiver: Option<ReceiverParams>,
}

/// Resolved OTLP-receiver-role configuration (RFC 0003 §6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReceiverParams {
    grpc_addr: SocketAddr,
    http_addr: SocketAddr,
    wal_root: PathBuf,
}

/// Resolve [`ServerConfig`] from the environment:
/// - `OURIOS_BUCKET_ROOT` (required) — the store root.
/// - `OURIOS_COMPACTION_INTERVAL_SECS` (optional, default
///   [`DEFAULT_COMPACTION_INTERVAL_SECS`]).
/// - `OURIOS_RECEIVER_ENABLED` (optional) — enable the receiver role.
/// - `OURIOS_RECEIVER_GRPC_ADDR` / `OURIOS_RECEIVER_HTTP_ADDR` (optional,
///   default [`DEFAULT_GRPC_ADDR`] / [`DEFAULT_HTTP_ADDR`]).
/// - `OURIOS_WAL_ROOT` (required when the receiver is enabled) — the
///   write-ahead-log root.
fn config_from_env() -> Result<ServerConfig, String> {
    let bucket_root = std::env::var_os("OURIOS_BUCKET_ROOT").map(PathBuf::from);
    let interval_raw = std::env::var("OURIOS_COMPACTION_INTERVAL_SECS").ok();
    let mut config = build_config(bucket_root, interval_raw.as_deref())?;
    config.receiver = build_receiver_config(
        std::env::var("OURIOS_RECEIVER_ENABLED").ok().as_deref(),
        std::env::var("OURIOS_RECEIVER_GRPC_ADDR").ok().as_deref(),
        std::env::var("OURIOS_RECEIVER_HTTP_ADDR").ok().as_deref(),
        std::env::var_os("OURIOS_WAL_ROOT").map(PathBuf::from),
    )?;
    Ok(config)
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
    bucket_root: Option<PathBuf>,
    interval_raw: Option<&str>,
) -> Result<ServerConfig, String> {
    let bucket_root =
        bucket_root.ok_or("OURIOS_BUCKET_ROOT must be set (the data + audit store root)")?;
    if bucket_root.as_os_str().is_empty() {
        return Err("OURIOS_BUCKET_ROOT must not be empty".to_string());
    }
    let compaction_interval = match interval_raw {
        None => Duration::from_secs(DEFAULT_COMPACTION_INTERVAL_SECS),
        Some(raw) => {
            let secs: u64 = raw.parse().map_err(|_| {
                format!("OURIOS_COMPACTION_INTERVAL_SECS must be a positive integer, got {raw:?}")
            })?;
            if secs == 0 {
                return Err("OURIOS_COMPACTION_INTERVAL_SECS must be non-zero".to_string());
            }
            Duration::from_secs(secs)
        }
    };
    Ok(ServerConfig {
        bucket_root,
        compaction_interval,
        receiver: None,
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

    // Boot OpenTelemetry first so the compactor's instruments export
    // (RFC 0001 §6.8). The guard flushes pending metrics on shutdown;
    // OTEL_EXPORTER_OTLP_ENDPOINT et al. tune the exporter.
    let telemetry = ourios_telemetry::init(&TelemetryConfig::new("ourios-server"))?;

    // Start the OTLP receiver role if enabled (RFC 0003 §9). Report the
    // bound addresses on stdout so an operator — or a test binding `:0` —
    // learns the actual ports.
    let receiver = match &config.receiver {
        Some(params) => {
            let handle = receiver::serve(receiver::ReceiverConfig {
                grpc_addr: params.grpc_addr,
                http_addr: params.http_addr,
                wal: wal_config(&params.wal_root),
                // The data store the receiver's RFC 0014 write path lands
                // Parquet in — the same root the compactor sweeps (cloned
                // because `bucket_root` is moved into the compactor below).
                bucket_root: config.bucket_root.clone(),
            })
            .await?;
            println!("receiver gRPC listening on {}", handle.grpc_addr);
            println!("receiver HTTP listening on {}", handle.http_addr);
            std::io::stdout().flush().ok();
            Some(handle)
        }
        None => None,
    };

    // Durable compaction audit events (RFC 0009 §3.6 → RFC 0005 §3.7).
    let sink = Box::new(ParquetAuditSink::new(&config.bucket_root));
    let compactor = Compactor::new(
        config.bucket_root,
        CompactionPolicy::default(),
        config.compaction_interval,
    )
    .with_audit_sink(sink);

    // Run until SIGINT or SIGTERM (k8s / `nerdctl stop` send SIGTERM).
    // `compactor.run` never returns on its own, so the select resolves on
    // a signal (or a SIGINT-setup failure).
    let shutdown = tokio::select! {
        () = compactor.run(|result| match result {
            Ok(report) => {
                for err in &report.errors {
                    eprintln!("compaction sweep error: {err}");
                }
            }
            Err(e) => eprintln!("compaction sweep failed: {e}"),
        }) => Ok(()),
        signal = tokio::signal::ctrl_c() => signal,
        () = terminate_signal() => Ok(()),
    };

    // Drain the receiver listeners gracefully (releasing the single `Wal`)
    // before flushing telemetry and exiting.
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

    #[test]
    fn build_config_requires_bucket_root() {
        // Arrange / Act
        let result = build_config(None, None);

        // Assert
        assert!(result.is_err(), "OURIOS_BUCKET_ROOT is mandatory");
    }

    #[test]
    fn build_config_rejects_an_empty_bucket_root() {
        // Arrange / Act — an empty `OURIOS_BUCKET_ROOT` would resolve to a
        // relative/empty path and silently misdirect the store.
        let result = build_config(Some(PathBuf::from("")), None);

        // Assert
        assert!(result.is_err(), "an empty bucket root must be rejected");
    }

    #[test]
    fn build_config_defaults_the_interval() {
        // Arrange / Act
        let config = build_config(Some(PathBuf::from("/store")), None).expect("valid");

        // Assert
        assert_eq!(
            config.compaction_interval,
            Duration::from_secs(DEFAULT_COMPACTION_INTERVAL_SECS),
        );
        assert_eq!(config.bucket_root, PathBuf::from("/store"));
    }

    #[test]
    fn build_config_parses_a_custom_interval() {
        // Arrange / Act
        let config = build_config(Some(PathBuf::from("/store")), Some("60")).expect("valid");

        // Assert
        assert_eq!(config.compaction_interval, Duration::from_secs(60));
    }

    #[test]
    fn build_config_rejects_a_zero_or_nonnumeric_interval() {
        // Arrange / Act / Assert
        assert!(
            build_config(Some(PathBuf::from("/store")), Some("0")).is_err(),
            "a zero interval would busy-loop the daemon",
        );
        assert!(
            build_config(Some(PathBuf::from("/store")), Some("soon")).is_err(),
            "non-numeric interval is rejected",
        );
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
}
