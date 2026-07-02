//! `ourios-telemetry` — the OpenTelemetry bootstrap for Ourios.
//!
//! RFC 0001 §6.8 ("Export architecture") pins the dependency split:
//! instrumented library crates depend only on the lightweight
//! `opentelemetry` **API** and resolve instruments through
//! `global::meter("ourios.<subsystem>")`; this crate is the single
//! place the heavy SDK + OTLP exporter + transport live. It builds the
//! OTLP **push** `MeterProvider` (periodic-reader export), installs it
//! as the process-global provider, and hands back a [`TelemetryGuard`]
//! whose [`TelemetryGuard::shutdown`] flushes pending telemetry on exit.
//!
//! **Logs are dogfooded** (CLAUDE.md §6.3): [`init`] also builds an OTLP
//! `SdkLoggerProvider` and installs a `tracing` subscriber whose
//! [`OpenTelemetryTracingBridge`] turns every `tracing::info!`/`warn!`/
//! `error!` event into an `OTel` log record pushed over OTLP — Ourios's
//! own logs ship as the `OTel` Logs signal, the same way its users' logs
//! arrive. A `fmt` layer keeps a human-readable copy on **stderr**
//! (stdout stays reserved for the binary's machine-parsed start-up
//! lines). Both layers honour `RUST_LOG` (default `info`); the bridge
//! additionally carries a loop guard muting the export stack's own
//! crates (`tonic`/`hyper`/`h2`/`tower`/`opentelemetry*`) so
//! exporter-internal events cannot feed back into the exporter
//! (telemetry-induced-telemetry, per the `OTel` self-observability
//! guidelines) — the guard wins over `RUST_LOG`.
//!
//! The binary (`ourios-server`) calls [`init`] once at start-up with
//! the role it is running as; benches and integration tests use the
//! `testing`-feature [`init_in_memory`] to collect the exported metric
//! stream through an in-memory reader instead of a live OTLP endpoint.

#![deny(unsafe_code)]

use std::time::Duration;

use opentelemetry::global;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer as _};

/// Default OTLP export interval. The OpenTelemetry spec's default
/// periodic-reader interval is 60 s; we follow it unless a deployment
/// overrides.
pub const DEFAULT_EXPORT_INTERVAL: Duration = Duration::from_secs(60);

/// Bootstrap configuration for the metrics pipeline.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// The role this process runs as, e.g. `ourios-ingester` /
    /// `ourios-querier`. Becomes the `service.name` **resource**
    /// attribute (RFC 0001 §6.8 — set once on the provider, never a
    /// per-data-point attribute).
    pub service_name: String,
    /// OTLP collector endpoint. `None` uses the exporter's default
    /// (`http://localhost:4317` for gRPC), so the `OTEL_EXPORTER_*`
    /// environment overrides still apply.
    pub otlp_endpoint: Option<String>,
    /// Periodic-reader export interval ([`DEFAULT_EXPORT_INTERVAL`]).
    pub export_interval: Duration,
}

impl TelemetryConfig {
    /// Config for `service_name` with spec defaults (default endpoint,
    /// [`DEFAULT_EXPORT_INTERVAL`]).
    #[must_use]
    pub fn new(service_name: impl Into<String>) -> Self {
        Self {
            service_name: service_name.into(),
            otlp_endpoint: None,
            export_interval: DEFAULT_EXPORT_INTERVAL,
        }
    }
}

/// Errors raised while standing up or tearing down the telemetry
/// pipelines (metrics and logs).
#[derive(Debug)]
pub enum TelemetryError {
    /// An OTLP exporter (metrics or logs) could not be built (bad
    /// endpoint, TLS, …).
    Exporter(opentelemetry_otlp::ExporterBuildError),
    /// `MeterProvider::force_flush` failed to export pending metrics.
    Flush(opentelemetry_sdk::error::OTelSdkError),
    /// A provider (meter or logger) failed to flush on teardown.
    Shutdown(opentelemetry_sdk::error::OTelSdkError),
}

impl std::fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exporter(e) => write!(f, "building an OTLP exporter failed: {e}"),
            Self::Flush(e) => write!(f, "flushing the meter provider failed: {e}"),
            Self::Shutdown(e) => write!(f, "shutting down a telemetry provider failed: {e}"),
        }
    }
}

impl std::error::Error for TelemetryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Exporter(e) => Some(e),
            Self::Flush(e) | Self::Shutdown(e) => Some(e),
        }
    }
}

impl From<opentelemetry_otlp::ExporterBuildError> for TelemetryError {
    fn from(e: opentelemetry_otlp::ExporterBuildError) -> Self {
        Self::Exporter(e)
    }
}

/// Owns the installed [`SdkMeterProvider`] (and, from [`init`], the
/// [`SdkLoggerProvider`] behind the `tracing` bridge). Hold it for the
/// process lifetime; dropping it (or calling
/// [`TelemetryGuard::shutdown`]) flushes any telemetry not yet exported.
#[must_use = "dropping the guard immediately tears the telemetry pipeline back down"]
pub struct TelemetryGuard {
    provider: SdkMeterProvider,
    /// `None` on the metrics-only paths ([`init_in_memory`], tests).
    logger: Option<SdkLoggerProvider>,
}

impl TelemetryGuard {
    /// Flush and shut the telemetry pipelines down explicitly,
    /// surfacing any flush error (the `Drop` path can only swallow it).
    /// Both pipelines are always attempted; the first error wins.
    ///
    /// # Errors
    /// Returns [`TelemetryError::Shutdown`] if the meter or logger
    /// provider fails to flush or shut down.
    pub fn shutdown(&self) -> Result<(), TelemetryError> {
        let metrics = self.provider.shutdown().map_err(TelemetryError::Shutdown);
        let logs = match &self.logger {
            Some(logger) => logger.shutdown().map_err(TelemetryError::Shutdown),
            None => Ok(()),
        };
        metrics.and(logs)
    }

    /// Export pending metrics now, without tearing the pipeline down —
    /// the periodic reader otherwise exports on its own interval. Tests
    /// call this before collecting from an in-memory exporter (see
    /// [`init_in_memory`]).
    ///
    /// # Errors
    /// Returns [`TelemetryError::Flush`] if the meter provider fails to
    /// export.
    pub fn force_flush(&self) -> Result<(), TelemetryError> {
        self.provider.force_flush().map_err(TelemetryError::Flush)
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        // Best-effort final flush; `shutdown()` is the path that can
        // report failure. A second shutdown (after an explicit one) is
        // a no-op we deliberately ignore.
        let _ = self.provider.shutdown();
        if let Some(logger) = &self.logger {
            let _ = logger.shutdown();
        }
    }
}

fn resource(service_name: &str) -> Resource {
    Resource::builder()
        .with_service_name(service_name.to_owned())
        .build()
}

/// Build the OTLP push `MeterProvider` **and** the OTLP `LoggerProvider`
/// with its `tracing` bridge, install them process-globally, and return
/// the [`TelemetryGuard`] that owns them.
///
/// Call this **once**, at process start-up: it is the bootstrap entry
/// point, not a reconfiguration API. OpenTelemetry's `set_meter_provider` is
/// last-wins, so a second call replaces the global provider and leaks
/// the prior pipeline's periodic-reader thread; the `tracing` subscriber
/// can only be installed once at all (a second call silently keeps the
/// first subscriber). `ourios-server` owns the single call;
/// tests use [`init_in_memory`] or build a provider directly.
///
/// Must run inside a tokio runtime: the gRPC (tonic) OTLP exporters
/// export on it.
///
/// # Errors
/// Returns [`TelemetryError::Exporter`] if either OTLP exporter cannot
/// be constructed.
pub fn init(config: &TelemetryConfig) -> Result<TelemetryGuard, TelemetryError> {
    let resource = resource(&config.service_name);

    let mut builder = MetricExporter::builder().with_tonic();
    if let Some(endpoint) = &config.otlp_endpoint {
        builder = builder.with_endpoint(endpoint.clone());
    }
    let exporter = builder.build()?;

    let reader = PeriodicReader::builder(exporter)
        .with_interval(config.export_interval)
        .build();

    let provider = SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource.clone())
        .build();

    global::set_meter_provider(provider.clone());

    // Logs (CLAUDE.md §6.3 dogfooding): `tracing` events → OTel log
    // records → the OTLP batch exporter. The batch (not simple)
    // processor is load-bearing — a simple/synchronous export deadlocks
    // inside tonic request contexts.
    let mut builder = LogExporter::builder().with_tonic();
    if let Some(endpoint) = &config.otlp_endpoint {
        builder = builder.with_endpoint(endpoint.clone());
    }
    let log_exporter = builder.build()?;
    let logger = SdkLoggerProvider::builder()
        .with_batch_exporter(log_exporter)
        .with_resource(resource)
        .build();

    // The bridge honours `RUST_LOG` (default `info`) like the stderr copy,
    // so exported volume can be turned down (`warn`) or up (`debug`) the
    // same way. On top of that sits the telemetry-induced-telemetry loop
    // guard (OTel self-observability guidelines): the OTLP exporter is
    // itself a tonic/hyper client, so its internal `tracing` events must
    // not re-enter the bridge or every failed export would emit records
    // that trigger more exports — the guard's `off` directives always win,
    // whatever `RUST_LOG` says. The directives are compile-time constants;
    // an unparsable one is skipped rather than panicking.
    let mut bridge_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    for directive in [
        "hyper=off",
        "tonic=off",
        "h2=off",
        "tower=off",
        "reqwest=off",
        "opentelemetry=off",
        "opentelemetry_sdk=off",
        "opentelemetry_otlp=off",
    ] {
        if let Ok(directive) = directive.parse() {
            bridge_filter = bridge_filter.add_directive(directive);
        }
    }
    let bridge = OpenTelemetryTracingBridge::new(&logger).with_filter(bridge_filter);
    // Human-readable copy on stderr — stdout is reserved for the
    // binary's machine-parsed start-up lines (bound-port announcements).
    // `RUST_LOG` overrides the default `info`.
    let fmt = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")));
    // `try_init` (not `init`): a process can only ever have one global
    // subscriber. If one is already installed (a test harness), keep it —
    // metrics still work and the caller's logs still go wherever that
    // subscriber sends them. In that case the bridge was not wired, so
    // tear the logger pipeline down rather than keep an idle batch
    // processor alive for the process lifetime.
    let logger = if tracing_subscriber::registry()
        .with(bridge)
        .with(fmt)
        .try_init()
        .is_ok()
    {
        Some(logger)
    } else {
        let _ = logger.shutdown();
        None
    };

    Ok(TelemetryGuard { provider, logger })
}

/// Build an in-memory metrics pipeline for tests: a `MeterProvider`
/// whose periodic reader exports into the returned
/// [`InMemoryMetricExporter`](opentelemetry_sdk::metrics::InMemoryMetricExporter),
/// installed as the global provider so `global::meter(...)` resolves
/// against it. Record through the global meter, call the returned
/// guard's [`force_flush`](TelemetryGuard::force_flush), then read the
/// exporter's `get_finished_metrics()` to assert what was produced —
/// no OTLP endpoint required. Runs inside a tokio runtime (the periodic
/// reader's export path).
///
/// This installs the **process-global** provider, so tests that call
/// it share global state: run them serially (or one per test binary),
/// not concurrently with other telemetry tests. OpenTelemetry exposes no
/// primitive to restore a previously-installed global provider, so the
/// returned guard's `Drop` shuts the pipeline down but cannot reinstate
/// an earlier global. For a fully isolated test, skip the global and
/// build a provider directly, reading through `provider.meter(...)`
/// (as this crate's own unit test does).
///
/// Returns the guard plus the exporter to collect from.
#[cfg(feature = "testing")]
pub fn init_in_memory(
    service_name: &str,
) -> (
    TelemetryGuard,
    opentelemetry_sdk::metrics::InMemoryMetricExporter,
) {
    let exporter = opentelemetry_sdk::metrics::InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .with_resource(resource(service_name))
        .build();
    global::set_meter_provider(provider.clone());
    (
        TelemetryGuard {
            provider,
            logger: None,
        },
        exporter,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::data::{ResourceMetrics, ScopeMetrics};

    // Build a provider over an in-memory exporter (no global state, no
    // OTLP endpoint), wrap it in a guard, and assert that the guard's
    // `force_flush` exports the recorded instrument.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn guard_force_flush_exports_recorded_metrics() {
        // Arrange.
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .with_resource(resource("ourios-test"))
            .build();
        let meter = provider.meter("ourios.compaction");
        let counter = meter.u64_counter("ourios.compaction.sweeps").build();
        let guard = TelemetryGuard {
            provider,
            logger: None,
        };

        // Act.
        counter.add(1, &[]);
        guard.force_flush().expect("force_flush succeeds");

        // Assert.
        let resource_metrics = exporter.get_finished_metrics().expect("metrics exported");
        let names: Vec<String> = resource_metrics
            .iter()
            .flat_map(ResourceMetrics::scope_metrics)
            .flat_map(ScopeMetrics::metrics)
            .map(|m| m.name().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n == "ourios.compaction.sweeps"),
            "collected stream should contain the recorded instrument, got {names:?}",
        );
    }

    // The dogfooding pipe end-to-end at the unit level: a `tracing`
    // event crosses the `OpenTelemetryTracingBridge` and lands in the
    // logger provider's exporter as an OTel log record. Uses a *scoped*
    // subscriber (`with_default`) over an in-memory log exporter — no
    // global subscriber, no OTLP endpoint, no cross-test state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn tracing_events_bridge_to_otel_log_records() {
        use opentelemetry_sdk::logs::InMemoryLogExporter;
        use tracing_subscriber::layer::SubscriberExt as _;

        // Arrange.
        let exporter = InMemoryLogExporter::default();
        let logger = SdkLoggerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .with_resource(resource("ourios-test"))
            .build();
        let subscriber =
            tracing_subscriber::registry().with(OpenTelemetryTracingBridge::new(&logger));

        // Act.
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(sweep.files = 3_i64, "compaction sweep finished");
        });
        logger.force_flush().expect("force_flush succeeds");

        // Assert.
        let records = exporter.get_emitted_logs().expect("logs exported");
        assert!(
            records.iter().any(|log| {
                log.record
                    .body()
                    .is_some_and(|b| format!("{b:?}").contains("compaction sweep finished"))
            }),
            "the tracing event should surface as an OTel log record, got {records:?}",
        );
    }
}
