//! `ourios-server` — the Ourios binary (`CLAUDE.md` §1, §7).
//!
//! Today it runs only the **background compaction role** (RFC 0009
//! §3.2): it boots OpenTelemetry (the OTLP push `MeterProvider`,
//! RFC 0001 §6.8), opens a durable audit sink for the §3.6 compaction
//! events (RFC 0005 §3.7), and runs the compactor until SIGINT, then
//! flushes telemetry on the way out.
//!
//! The OTLP receiver (RFC 0003) and querier (RFC 0007) roles, and a
//! structured-logging framework (`CLAUDE.md` §6.3 — sweep errors go to
//! stderr as a stopgap here), are follow-ups.

#![deny(unsafe_code)]

use std::error::Error;
use std::path::PathBuf;
use std::time::Duration;

use ourios_ingester::Compactor;
use ourios_parquet::{CompactionPolicy, ParquetAuditSink};
use ourios_telemetry::TelemetryConfig;

/// Default compaction sweep cadence when `OURIOS_COMPACTION_INTERVAL_SECS`
/// is unset.
const DEFAULT_COMPACTION_INTERVAL_SECS: u64 = 300;

/// Resolved server configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerConfig {
    /// Root of the data + audit store (the compactor sweeps under it).
    bucket_root: PathBuf,
    /// How often the compaction daemon sweeps.
    compaction_interval: Duration,
}

/// Resolve [`ServerConfig`] from the environment:
/// - `OURIOS_BUCKET_ROOT` (required) — the store root.
/// - `OURIOS_COMPACTION_INTERVAL_SECS` (optional, default
///   [`DEFAULT_COMPACTION_INTERVAL_SECS`]).
fn config_from_env() -> Result<ServerConfig, String> {
    let bucket_root = std::env::var_os("OURIOS_BUCKET_ROOT").map(PathBuf::from);
    let interval_raw = std::env::var("OURIOS_COMPACTION_INTERVAL_SECS").ok();
    build_config(bucket_root, interval_raw.as_deref())
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
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = config_from_env()?;

    // Boot OpenTelemetry first so the compactor's instruments export
    // (RFC 0001 §6.8). The guard flushes pending metrics on shutdown;
    // OTEL_EXPORTER_OTLP_ENDPOINT et al. tune the exporter.
    let telemetry = ourios_telemetry::init(&TelemetryConfig::new("ourios-server"))?;

    // Durable compaction audit events (RFC 0009 §3.6 → RFC 0005 §3.7).
    let sink = Box::new(ParquetAuditSink::new(&config.bucket_root));
    let compactor = Compactor::new(
        config.bucket_root,
        CompactionPolicy::default(),
        config.compaction_interval,
    )
    .with_audit_sink(sink);

    // Run the compaction daemon until SIGINT. `run` never returns on its
    // own, so the select resolves on ctrl-c (or a SIGINT-setup failure).
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
    };

    // Flush pending telemetry on the way out regardless of how we got here.
    telemetry.shutdown()?;

    // A SIGINT-handler setup failure is fatal: cancelling the compactor and
    // exiting 0 would leave the server silently doing no work.
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
}
