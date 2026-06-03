//! `ourios-telemetry` — the OpenTelemetry bootstrap for Ourios.
//!
//! RFC 0001 §6.8 ("Export architecture") pins the dependency split:
//! instrumented library crates depend only on the lightweight
//! `opentelemetry` **API** and resolve instruments through
//! `global::meter("ourios.<subsystem>")`; this crate is the single
//! place the heavy SDK + OTLP exporter + transport live. It builds the
//! OTLP **push** `MeterProvider` (periodic-reader export), installs it
//! as the process-global provider, and hands back a [`TelemetryGuard`]
//! whose [`TelemetryGuard::shutdown`] flushes pending metrics on exit.
//!
//! The binary (`ourios-server`) calls [`init`] once at start-up with
//! the role it is running as; benches and integration tests use the
//! `testing`-feature [`init_in_memory`] to collect the exported metric
//! stream through an in-memory reader instead of a live OTLP endpoint.

#![deny(unsafe_code)]

use std::time::Duration;

use opentelemetry::global;
use opentelemetry_otlp::{MetricExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};

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

/// Errors raised while standing up or tearing down the metrics
/// pipeline.
#[derive(Debug)]
pub enum TelemetryError {
    /// The OTLP exporter could not be built (bad endpoint, TLS, …).
    Exporter(opentelemetry_otlp::ExporterBuildError),
    /// `MeterProvider::force_flush` failed to export pending metrics.
    Flush(opentelemetry_sdk::error::OTelSdkError),
    /// `MeterProvider::shutdown` failed to flush on teardown.
    Shutdown(opentelemetry_sdk::error::OTelSdkError),
}

impl std::fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exporter(e) => write!(f, "building the OTLP metric exporter failed: {e}"),
            Self::Flush(e) => write!(f, "flushing the meter provider failed: {e}"),
            Self::Shutdown(e) => write!(f, "shutting down the meter provider failed: {e}"),
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

/// Owns the installed [`SdkMeterProvider`]. Hold it for the process
/// lifetime; dropping it (or calling [`TelemetryGuard::shutdown`])
/// flushes any metrics the periodic reader has not yet exported.
#[must_use = "dropping the guard immediately tears the metrics pipeline back down"]
pub struct TelemetryGuard {
    provider: SdkMeterProvider,
}

impl TelemetryGuard {
    /// Flush and shut the metrics pipeline down explicitly, surfacing
    /// any flush error (the `Drop` path can only swallow it).
    ///
    /// # Errors
    /// Returns [`TelemetryError::Shutdown`] if the meter provider fails
    /// to flush or shut down.
    pub fn shutdown(&self) -> Result<(), TelemetryError> {
        self.provider.shutdown().map_err(TelemetryError::Shutdown)
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
    }
}

fn resource(service_name: &str) -> Resource {
    Resource::builder()
        .with_service_name(service_name.to_owned())
        .build()
}

/// Build the OTLP push `MeterProvider`, install it as the process-
/// global provider, and return the [`TelemetryGuard`] that owns it.
///
/// Call this **once**, at process start-up: it is the bootstrap entry
/// point, not a reconfiguration API. OpenTelemetry's `set_meter_provider` is
/// last-wins, so a second call replaces the global provider and leaks
/// the prior pipeline's periodic-reader thread. `ourios-server` owns
/// the single call; tests use [`init_in_memory`] or build a provider
/// directly.
///
/// Must run inside a tokio runtime: the gRPC (tonic) OTLP exporter and
/// the periodic reader export on it.
///
/// # Errors
/// Returns [`TelemetryError::Exporter`] if the OTLP exporter cannot be
/// constructed.
pub fn init(config: &TelemetryConfig) -> Result<TelemetryGuard, TelemetryError> {
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
        .with_resource(resource(&config.service_name))
        .build();

    global::set_meter_provider(provider.clone());
    Ok(TelemetryGuard { provider })
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
    (TelemetryGuard { provider }, exporter)
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
        let guard = TelemetryGuard { provider };

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
}
