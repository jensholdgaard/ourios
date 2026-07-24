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

use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer as _};

/// The instrumentation-scope name for the tracer that opens Ourios's own
/// spans (RFC 0038). The library crates create instruments through
/// `global::meter("ourios.<subsystem>")`; spans go through this one tracer.
const TRACER_SCOPE: &str = "ourios";

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
    /// environment overrides still apply. The periodic-reader export
    /// interval is the standard `OTEL_METRIC_EXPORT_INTERVAL` env var
    /// — **milliseconds**, default `60000` (60 s) — resolved by the SDK, not a field here.
    pub otlp_endpoint: Option<String>,
    /// Dogfood the traces signal (RFC 0038). `true` installs a
    /// `TracerProvider` + `tracing-opentelemetry` layer, so `tracing`
    /// spans become `OTel` spans and every log record carries the active
    /// span's `trace_id`/`span_id`. `false` restores the logs+metrics-only
    /// posture (no tracer, no `trace_id` on logs). Operators disable via the
    /// standard `OTEL_TRACES_EXPORTER=none` (mapped to this flag by the
    /// server); the trace **sampler** is the standard `OTEL_TRACES_SAMPLER`
    /// env var, resolved by the SDK — not a field here (RFC 0038 §3.4).
    pub traces_enabled: bool,
    /// Install the metrics pipeline (`true`) or not. `false` installs no
    /// meter provider, so instruments resolve to the global no-op and nothing
    /// exports. Operators disable via the standard `OTEL_METRICS_EXPORTER=none`
    /// (mapped to this flag by the server).
    pub metrics_enabled: bool,
    /// Install the logs pipeline (`true`) or not. `false` skips the OTLP
    /// logger + the `tracing`→OTel-logs bridge (the stderr `fmt` layer stays),
    /// so Ourios's own logs no longer ship as the `OTel` Logs signal. Operators
    /// disable via the standard `OTEL_LOGS_EXPORTER=none` (mapped by the server).
    pub logs_enabled: bool,
}

impl TelemetryConfig {
    /// Config for `service_name` with spec defaults (default endpoint,
    /// traces on). The metric export interval and the trace sampler both
    /// come from the SDK's own env resolution (`OTEL_METRIC_EXPORT_INTERVAL`
    /// default `60000` ms; `OTEL_TRACES_SAMPLER` default `parentbased_always_on`).
    #[must_use]
    pub fn new(service_name: impl Into<String>) -> Self {
        Self {
            service_name: service_name.into(),
            otlp_endpoint: None,
            traces_enabled: true,
            metrics_enabled: true,
            logs_enabled: true,
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
    /// `None` when metrics are disabled (`metrics_enabled: false`).
    provider: Option<SdkMeterProvider>,
    /// `None` when logs are disabled (`logs_enabled: false`), on the
    /// subscriber-already-installed path, or the metrics-only test paths.
    logger: Option<SdkLoggerProvider>,
    /// `None` when traces are disabled (`traces_enabled: false`) or on the
    /// metrics-only paths.
    tracer: Option<SdkTracerProvider>,
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
        let metrics = match &self.provider {
            Some(provider) => provider.shutdown().map_err(TelemetryError::Shutdown),
            None => Ok(()),
        };
        let logs = match &self.logger {
            Some(logger) => logger.shutdown().map_err(TelemetryError::Shutdown),
            None => Ok(()),
        };
        let traces = match &self.tracer {
            Some(tracer) => tracer.shutdown().map_err(TelemetryError::Shutdown),
            None => Ok(()),
        };
        metrics.and(logs).and(traces)
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
        match &self.provider {
            Some(provider) => provider.force_flush().map_err(TelemetryError::Flush),
            None => Ok(()),
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        // Best-effort final flush; `shutdown()` is the path that can
        // report failure. A second shutdown (after an explicit one) is
        // a no-op we deliberately ignore.
        if let Some(provider) = &self.provider {
            let _ = provider.shutdown();
        }
        if let Some(logger) = &self.logger {
            let _ = logger.shutdown();
        }
        if let Some(tracer) = &self.tracer {
            let _ = tracer.shutdown();
        }
    }
}

fn resource(service_name: &str) -> Resource {
    Resource::builder()
        .with_service_name(service_name.to_owned())
        .build()
}

/// A boxed subscriber layer over the root `Registry`, so a conditionally-built
/// layer (the traces layer, absent when traces are disabled) can be stored in a
/// binding before entering the subscriber chain.
type BoxedLayer = Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>;

/// A `RUST_LOG`-honouring filter with the telemetry-induced-telemetry loop
/// guard applied (`CLAUDE.md` §6.3): the OTLP exporters are themselves
/// tonic/hyper clients, so their own `tracing` events **and spans** must be
/// muted, or every export would generate more telemetry to export. The `off`
/// directives win over `RUST_LOG`. Shared by the logs appender bridge and the
/// traces layer. Directives are compile-time constants; an unparsable one is
/// skipped rather than panicking.
fn guarded_env_filter() -> EnvFilter {
    let mut filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
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
            filter = filter.add_directive(directive);
        }
    }
    filter
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

    // Metrics: `None` when disabled (`OTEL_METRICS_EXPORTER=none` → the server
    // clears `metrics_enabled`), so instruments resolve to the global no-op and
    // nothing exports. No `.with_interval(...)`: the SDK's periodic reader
    // resolves the interval from the standard `OTEL_METRIC_EXPORT_INTERVAL` env
    // var (milliseconds, default 60000 = 60 s).
    let provider: Option<SdkMeterProvider> = if config.metrics_enabled {
        let mut builder = MetricExporter::builder().with_tonic();
        if let Some(endpoint) = &config.otlp_endpoint {
            builder = builder.with_endpoint(endpoint.clone());
        }
        let exporter = builder.build()?;
        let reader = PeriodicReader::builder(exporter).build();
        Some(
            SdkMeterProvider::builder()
                .with_reader(reader)
                .with_resource(resource.clone())
                .build(),
        )
    } else {
        None
    };

    // Logs (CLAUDE.md §6.3 dogfooding): `tracing` events → OTel log
    // records → the OTLP batch exporter. The batch (not simple)
    // processor is load-bearing — a simple/synchronous export deadlocks
    // inside tonic request contexts. Built *before* the meter provider is
    // installed globally: every fallible step happens first, so a failed
    // `init` cannot leave a live pipeline behind that the caller has no
    // guard to shut down. `None` when disabled (`OTEL_LOGS_EXPORTER=none`);
    // the stderr `fmt` layer stays, so only the OTel Logs signal is dropped.
    let logger: Option<SdkLoggerProvider> = if config.logs_enabled {
        let mut builder = LogExporter::builder().with_tonic();
        if let Some(endpoint) = &config.otlp_endpoint {
            builder = builder.with_endpoint(endpoint.clone());
        }
        let log_exporter = builder.build()?;
        Some(
            SdkLoggerProvider::builder()
                .with_batch_exporter(log_exporter)
                .with_resource(resource.clone())
                .build(),
        )
    } else {
        None
    };

    // Traces (RFC 0038): a `TracerProvider` over the OTLP batch span
    // exporter, whose tracer feeds a `tracing-opentelemetry` layer — so
    // `tracing` spans become OTel spans and their ids reach every log
    // record through the appender bridge. Also built before installing any
    // global (the span exporter is the last fallible step). `None` when
    // traces are disabled, which keeps today's logs+metrics posture exactly.
    // We deliberately do NOT set a sampler: the SDK resolves it from the
    // standard `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG` env vars
    // (default `parentbased_always_on`), so operators tune sampling with the
    // universal OTel knob instead of a bespoke Ourios one (RFC 0038 §3.4). The
    // disciplined span count sits far below the ~1000 traces/sec threshold
    // OTel says to sample at, so the always-on default is the right baseline.
    let (tracer, otel_layer): (Option<SdkTracerProvider>, Option<BoxedLayer>) =
        if config.traces_enabled {
            let mut builder = SpanExporter::builder().with_tonic();
            if let Some(endpoint) = &config.otlp_endpoint {
                builder = builder.with_endpoint(endpoint.clone());
            }
            let span_exporter = builder.build()?;
            let tracer_provider = SdkTracerProvider::builder()
                .with_batch_exporter(span_exporter)
                .with_resource(resource)
                .build();
            // Strip `tracing`'s synthetic per-span attributes: `busy_ns`/`idle_ns`
            // (tracked inactivity) and `target` carry no semconv namespace, and
            // `with_location` emits `code.module.name`, which collides with the
            // upstream `code` namespace — all four fail `weaver registry
            // live-check`. They are instrumentation-source metadata, not domain
            // telemetry; our spans carry their identity in the span name plus the
            // attributes we add deliberately. `thread.{id,name}` are valid semconv
            // (kept by leaving `with_threads` at its default).
            let layer = tracing_opentelemetry::layer()
                .with_tracer(tracer_provider.tracer(TRACER_SCOPE))
                .with_tracked_inactivity(false)
                .with_location(false)
                .with_target(false)
                .with_filter(guarded_env_filter())
                .boxed();
            (Some(tracer_provider), Some(layer))
        } else {
            (None, None)
        };

    if let Some(provider) = &provider {
        global::set_meter_provider(provider.clone());
    }
    // The global tracer provider is set *after* `try_init` confirms the traces
    // layer is wired (below) — not here. Setting it before would, on a lost
    // subscriber-install race, leave the global pointing at a provider this
    // function then shuts down (the metrics global is safe to set early: it is
    // kept regardless of the subscriber outcome).

    // The appender bridge and the traces layer both honour `RUST_LOG`
    // (default `info`) with the telemetry-induced-telemetry loop guard
    // applied (`guarded_env_filter`): the OTLP exporters are tonic/hyper
    // clients, so their internal events *and spans* must not re-enter the
    // pipeline or every failed export would emit more telemetry to export —
    // the guard's `off` directives always win over `RUST_LOG`.
    let bridge = logger
        .as_ref()
        .map(|logger| OpenTelemetryTracingBridge::new(logger).with_filter(guarded_env_filter()));
    // Human-readable copy on stderr — stdout is reserved for the
    // binary's machine-parsed start-up lines (bound-port announcements).
    // `RUST_LOG` overrides the default `info`.
    let fmt = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")));
    // `try_init` (not `init`): a process can only ever have one global
    // subscriber. If one is already installed (a test harness), keep it —
    // metrics still work and the caller's logs still go wherever that
    // subscriber sends them. In that case the bridge/traces layer were not
    // wired, so tear the logger and tracer pipelines down rather than keep
    // idle batch processors alive for the process lifetime.
    let installed = tracing_subscriber::registry()
        .with(otel_layer)
        .with(bridge)
        .with(fmt)
        .try_init()
        .is_ok();
    let (logger, tracer) = if installed {
        // The subscriber — and its traces layer — is live, so register the
        // tracer provider globally now (any direct `global::tracer(...)` use
        // resolves against a running pipeline).
        if let Some(tracer_provider) = &tracer {
            global::set_tracer_provider(tracer_provider.clone());
        }
        (logger, tracer)
    } else {
        if let Some(logger) = &logger {
            let _ = logger.shutdown();
        }
        if let Some(tracer_provider) = &tracer {
            let _ = tracer_provider.shutdown();
        }
        (None, None)
    };

    Ok(TelemetryGuard {
        provider,
        logger,
        tracer,
    })
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
            provider: Some(provider),
            logger: None,
            tracer: None,
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
            provider: Some(provider),
            logger: None,
            tracer: None,
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

    // Scenario RFC0038.1 (unit slice): with the traces layer and the appender
    // bridge on the same subscriber, a `tracing::info!` emitted *inside* a
    // `tracing` span carries that span's (non-zero) `trace_id`/`span_id` on the
    // resulting OTel log record — the correlation the reported gap was about.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn rfc0038_1_log_within_a_span_carries_trace_context() {
        use opentelemetry::trace::{SpanId, TraceId};
        use opentelemetry_sdk::logs::InMemoryLogExporter;
        use opentelemetry_sdk::trace::InMemorySpanExporter;

        // Arrange — a tracer + logger over in-memory exporters, wired through
        // the same two bridges `init` installs.
        let span_exporter = InMemorySpanExporter::default();
        let tracer_provider = SdkTracerProvider::builder()
            .with_simple_exporter(span_exporter)
            .with_resource(resource("ourios-test"))
            .build();
        let log_exporter = InMemoryLogExporter::default();
        let logger = SdkLoggerProvider::builder()
            .with_simple_exporter(log_exporter.clone())
            .with_resource(resource("ourios-test"))
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer(TRACER_SCOPE)))
            .with(OpenTelemetryTracingBridge::new(&logger));

        // Act — emit a log *inside* an entered span.
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("ourios.test.request");
            let _enter = span.enter();
            tracing::info!("a log emitted within the span");
        });
        logger.force_flush().expect("logs flush");

        // Assert — the log carries a non-zero trace context.
        let records = log_exporter.get_emitted_logs().expect("logs exported");
        let correlated = records.iter().any(|log| {
            log.record
                .trace_context()
                .is_some_and(|tc| tc.trace_id != TraceId::INVALID && tc.span_id != SpanId::INVALID)
        });
        assert!(
            correlated,
            "a log emitted inside a span should carry a non-zero trace_id/span_id, got {records:?}",
        );
    }
}
