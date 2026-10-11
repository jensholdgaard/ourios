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
//! The same provider carries the upstream `OTel` process metrics
//! (`process.memory.usage`, `process.cpu.time`, …), so memory and CPU
//! pressure show up in Ourios's own telemetry before the kernel acts on it.
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

#[cfg(feature = "testing")]
pub mod live_check;
mod process;
mod runtime;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::resource::{
    EnvResourceDetector, SdkProvidedResourceDetector, TelemetryResourceDetector,
};
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
    /// The release this process runs, the `service.version` **resource**
    /// attribute. The binary sets its own `CARGO_PKG_VERSION`, which names the
    /// artefact actually deployed; `None` falls back to this crate's version,
    /// the same workspace release. An operator-set `service.version` in
    /// `OTEL_RESOURCE_ATTRIBUTES` overrides either.
    pub service_version: Option<String>,
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
    /// posture (no tracer, no `trace_id` on logs). This is a **programmatic
    /// override**: operators disable via the standard `OTEL_TRACES_EXPORTER=none`
    /// (or `OTEL_SDK_DISABLED=true`), which [`init`] honors on top of this flag.
    /// The trace **sampler** is likewise the standard `OTEL_TRACES_SAMPLER` env
    /// var, resolved by the SDK — not a field here (RFC 0038 §3.4).
    pub traces_enabled: bool,
    /// Install the metrics pipeline (`true`) or not. `false` installs no meter
    /// provider, so instruments resolve to the global no-op and nothing exports.
    /// A **programmatic override**: operators disable via the standard
    /// `OTEL_METRICS_EXPORTER=none` (or `OTEL_SDK_DISABLED=true`), honored by
    /// [`init`] on top of this flag.
    pub metrics_enabled: bool,
    /// Install the logs pipeline (`true`) or not. `false` skips the OTLP logger +
    /// the `tracing`→OTel-logs bridge (the stderr `fmt` layer stays), so Ourios's
    /// own logs no longer ship as the `OTel` Logs signal. A **programmatic
    /// override**: operators disable via the standard `OTEL_LOGS_EXPORTER=none`
    /// (or `OTEL_SDK_DISABLED=true`), honored by [`init`] on top of this flag.
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
            service_version: None,
            otlp_endpoint: None,
            traces_enabled: true,
            metrics_enabled: true,
            logs_enabled: true,
        }
    }

    fn resource(&self) -> Resource {
        resource(
            &self.service_name,
            self.service_version
                .as_deref()
                .unwrap_or(DEFAULT_SERVICE_VERSION),
        )
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

/// Owns whichever signal providers [`init`] installed — the meter provider,
/// the [`SdkLoggerProvider`] behind the `tracing` bridge, and/or the tracer.
/// With per-signal disable (`OTEL_{METRICS,LOGS,TRACES}_EXPORTER=none`, or
/// `OTEL_SDK_DISABLED=true` for all), any or all of them may be absent — each
/// field is [`Option`]. Hold it for the process lifetime; dropping it (or
/// calling [`TelemetryGuard::shutdown`]) flushes any telemetry not yet exported.
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

/// The `service.version` used when [`TelemetryConfig::service_version`] is unset.
const DEFAULT_SERVICE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The resource every signal shares: the service, and the `process` entity
/// that the process metrics describe.
///
/// Precedence, later wins: `service.version` (a default), then the SDK's
/// detectors (`OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES`, `telemetry.sdk.*`),
/// then `service.name` and the `process` identity. So an operator can relabel
/// the version but not the service name or the process identity.
fn resource(service_name: &str, service_version: &str) -> Resource {
    Resource::builder_empty()
        .with_attribute(KeyValue::new("service.version", service_version.to_owned()))
        .with_detectors(&[
            Box::new(SdkProvidedResourceDetector),
            Box::new(TelemetryResourceDetector),
            Box::new(EnvResourceDetector::new()),
        ])
        .with_service_name(service_name.to_owned())
        .with_attributes(process::identity())
        .build()
}

/// A boxed subscriber layer over the root `Registry`, so a conditionally-built
/// layer (the traces layer, absent when traces are disabled) can be stored in a
/// binding before entering the subscriber chain.
type BoxedLayer = Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>;

/// A `RUST_LOG`-honouring filter that mutes third-party crates whose internal
/// `tracing` output is not Ourios's own signal. Two reasons, both wanting the
/// crate silenced in the exported stream:
///
/// - **Telemetry-induced-telemetry loop guard** (`CLAUDE.md` §6.3): the OTLP
///   exporters are themselves tonic/hyper clients, so their own events **and
///   spans** must be muted, or every export would generate more telemetry to
///   export.
/// - **Transport-library internals**: `rmcp` (the MCP server SDK) instruments
///   its own session/notification handling with bare, non-semconv fields
///   (`session_id`, `peer_info`, `notification`). Exporting those would drift
///   the telemetry from the pinned registry (the live-check gate) while adding
///   no Ourios-level signal — Ourios's own `execute_tool` span (target
///   `ourios_server::mcp`, untouched by this directive) is the signal.
///
/// The `off` directives win over `RUST_LOG`. Shared by the logs appender bridge
/// and the traces layer. Directives are compile-time constants; an unparsable
/// one is skipped rather than panicking.
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
        "rmcp=off",
    ] {
        if let Ok(directive) = directive.parse() {
            filter = filter.add_directive(directive);
        }
    }
    filter
}

/// The standard `OTEL_SDK_DISABLED` boolean kill-switch: `true`
/// (case-insensitive) selects a no-op SDK for **all** signals; any other value
/// or absence leaves the SDK enabled (per the `OTel` env-var spec).
fn otel_sdk_disabled() -> bool {
    std::env::var("OTEL_SDK_DISABLED").is_ok_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

/// Interpret a standard `OTEL_{TRACES,METRICS,LOGS}_EXPORTER` selector value as
/// install-or-not: `none` (any casing/whitespace) is the only value that turns
/// the signal off; unset and every other value (`otlp`, `console`, …) leave it
/// on. Ourios always exports over OTLP when a signal is on — it does not
/// implement the full exporter *selector*, so a non-`none` value just means
/// "on" (OTLP). We honor this universal var here (the SDK's manual-construction
/// path doesn't read it — there is no Rust `autoexport`), so the operator never
/// needs a bespoke Ourios knob (#618).
fn exporter_selected(otel_signal_exporter: Option<&str>) -> bool {
    !otel_signal_exporter.is_some_and(|value| value.trim().eq_ignore_ascii_case("none"))
}

/// Whether the signal exporting to `exporter_var` (e.g. `OTEL_TRACES_EXPORTER`)
/// is enabled: the programmatic `config` flag AND not globally disabled AND the
/// selector env var is not `none`.
fn signal_enabled(config_enabled: bool, sdk_disabled: bool, exporter_var: &str) -> bool {
    config_enabled
        && !sdk_disabled
        && exporter_selected(std::env::var(exporter_var).ok().as_deref())
}

/// Whether a signal exports over OTLP/HTTP with protobuf payloads: the
/// signal-specific `OTEL_EXPORTER_OTLP_<SIGNAL>_PROTOCOL` wins over
/// `OTEL_EXPORTER_OTLP_PROTOCOL`, per the exporter spec. Anything else,
/// including unset, keeps gRPC: the server has always exported over gRPC, and
/// the spec's `http/protobuf` default would silently retarget existing
/// deployments.
#[cfg(feature = "http-proto")]
fn http_protobuf_selected(signal: &str) -> bool {
    is_http_protobuf(
        env_value(&format!("OTEL_EXPORTER_OTLP_{signal}_PROTOCOL")).as_deref(),
        env_value("OTEL_EXPORTER_OTLP_PROTOCOL").as_deref(),
    )
}

/// Whether the standard OTLP env sends metrics to an OTLP/HTTP endpoint with
/// protobuf payloads: an endpoint is set (`OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`,
/// else `OTEL_EXPORTER_OTLP_ENDPOINT`) and the metrics protocol resolves to
/// `http/protobuf` with the same precedence [`init`] applies. Empty values
/// count as unset. A process with no tokio runtime checks this before calling
/// [`init`], since the gRPC exporter it would otherwise build needs one.
#[cfg(feature = "http-proto")]
#[must_use]
pub fn metrics_http_export_configured() -> bool {
    http_export_configured(
        env_value("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT").as_deref(),
        env_value("OTEL_EXPORTER_OTLP_ENDPOINT").as_deref(),
        env_value("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL").as_deref(),
        env_value("OTEL_EXPORTER_OTLP_PROTOCOL").as_deref(),
    )
}

#[cfg(feature = "http-proto")]
fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The first of a signal-specific and a general OTLP setting that is set and
/// not blank: an empty signal-specific value must not shadow the general one.
#[cfg(feature = "http-proto")]
fn first_set<'a>(signal: Option<&'a str>, general: Option<&'a str>) -> Option<&'a str> {
    let set = |value: &&str| !value.trim().is_empty();
    signal.filter(set).or(general.filter(set))
}

/// The protocol is an Enum in the configuration spec, whose values SHOULD be
/// read case-insensitively.
#[cfg(feature = "http-proto")]
fn is_http_protobuf(signal_protocol: Option<&str>, protocol: Option<&str>) -> bool {
    first_set(signal_protocol, protocol)
        .is_some_and(|protocol| protocol.trim().eq_ignore_ascii_case("http/protobuf"))
}

#[cfg(feature = "http-proto")]
fn http_export_configured(
    signal_endpoint: Option<&str>,
    endpoint: Option<&str>,
    signal_protocol: Option<&str>,
    protocol: Option<&str>,
) -> bool {
    first_set(signal_endpoint, endpoint).is_some() && is_http_protobuf(signal_protocol, protocol)
}

/// The OTLP/HTTP client is built with `rustls-no-provider`, so it uses the
/// process-default crypto provider and panics when there is none. Install ring
/// unless the process already chose one.
#[cfg(feature = "http-proto")]
fn ensure_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // Losing an install race to another thread still leaves a provider.
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

/// Build one signal's OTLP exporter: OTLP/HTTP when the `http-proto` feature is
/// on and the protocol env selects it, gRPC otherwise. Endpoint and headers
/// come from the standard `OTEL_EXPORTER_OTLP_*` env unless
/// [`TelemetryConfig::otlp_endpoint`] overrides the endpoint; over HTTP that
/// override is a base URL, so the signal path is appended as the env form does.
macro_rules! otlp_exporter {
    ($exporter:ident, $config:expr, $signal:literal, $path:literal) => {{
        let config: &TelemetryConfig = $config;
        #[cfg(feature = "http-proto")]
        let http = http_protobuf_selected($signal).then(|| {
            ensure_crypto_provider();
            let mut builder = $exporter::builder()
                .with_http()
                .with_protocol(opentelemetry_otlp::Protocol::HttpBinary);
            if let Some(endpoint) = &config.otlp_endpoint {
                builder =
                    builder.with_endpoint(format!("{}{}", endpoint.trim_end_matches('/'), $path));
            }
            builder.build()
        });
        #[cfg(not(feature = "http-proto"))]
        let http: Option<Result<$exporter, opentelemetry_otlp::ExporterBuildError>> = None;
        match http {
            Some(built) => built?,
            None => {
                let mut builder = $exporter::builder().with_tonic();
                if let Some(endpoint) = &config.otlp_endpoint {
                    builder = builder.with_endpoint(endpoint.clone());
                }
                builder.build()?
            }
        }
    }};
}

/// Build the enabled signals' OTLP pipelines — the push `MeterProvider`, the
/// `LoggerProvider` with its `tracing` bridge, and the tracer — install them
/// process-globally, and return the [`TelemetryGuard`] that owns them. Each
/// signal is skipped when disabled (`OTEL_{METRICS,LOGS,TRACES}_EXPORTER=none`
/// per-signal, or `OTEL_SDK_DISABLED=true` for all), so the guard may own none.
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
/// export on it. A signal exporting over OTLP/HTTP (the `http-proto`
/// feature) does not need one.
///
/// # Errors
/// Returns [`TelemetryError::Exporter`] if either OTLP exporter cannot
/// be constructed.
pub fn init(config: &TelemetryConfig) -> Result<TelemetryGuard, TelemetryError> {
    let resource = config.resource();

    // RFC 0039 §3.1: the W3C trace-context propagator, installed
    // unconditionally. It is stateless and cheap, and extraction is inert when
    // no traces pipeline is running — installing it regardless keeps the
    // ingress code uniform (no "is tracing on?" branch at every ingress).
    global::set_text_map_propagator(TraceContextPropagator::new());

    // Per-signal install decisions honor the universal OTel env vars directly —
    // `ourios-telemetry` plays the "autoconfigure" role Go's `autoexport` /
    // Java's `sdk-extension-autoconfigure` play, since the Rust SDK's manual
    // exporter construction reads neither `OTEL_{TRACES,METRICS,LOGS}_EXPORTER`
    // nor `OTEL_SDK_DISABLED` (#618). The `config.*_enabled` flags are a
    // programmatic override on top (default on), so a signal is installed only
    // when the flag, the global kill-switch, and the per-signal selector all
    // agree.
    let sdk_disabled = otel_sdk_disabled();
    let metrics_enabled = signal_enabled(
        config.metrics_enabled,
        sdk_disabled,
        "OTEL_METRICS_EXPORTER",
    );
    let logs_enabled = signal_enabled(config.logs_enabled, sdk_disabled, "OTEL_LOGS_EXPORTER");
    let traces_enabled =
        signal_enabled(config.traces_enabled, sdk_disabled, "OTEL_TRACES_EXPORTER");

    // Metrics: `None` when disabled (`OTEL_METRICS_EXPORTER=none` → the server
    // clears `metrics_enabled`; or `OTEL_SDK_DISABLED=true`), so instruments
    // resolve to the global no-op and nothing exports. No `.with_interval(...)`:
    // the SDK's periodic reader resolves the interval from the standard
    // `OTEL_METRIC_EXPORT_INTERVAL` env var (milliseconds, default 60000 = 60 s).
    let provider: Option<SdkMeterProvider> = if metrics_enabled {
        Some(meter_provider(config, &resource)?)
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
    let logger: Option<SdkLoggerProvider> = if logs_enabled {
        let log_exporter = otlp_exporter!(LogExporter, config, "LOGS", "/v1/logs");
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
    let (tracer, otel_layer): (Option<SdkTracerProvider>, Option<BoxedLayer>) = if traces_enabled {
        let span_exporter = otlp_exporter!(SpanExporter, config, "TRACES", "/v1/traces");
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

/// The OTLP metrics pipeline, carrying the metrics Ourios observes about
/// itself rather than records at call sites.
fn meter_provider(
    config: &TelemetryConfig,
    resource: &Resource,
) -> Result<SdkMeterProvider, TelemetryError> {
    let exporter = otlp_exporter!(MetricExporter, config, "METRICS", "/v1/metrics");
    let reader = PeriodicReader::builder(exporter).build();
    let provider = SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource.clone())
        .build();
    process::register(&provider.meter(process::SCOPE));
    // The runtime `init` runs on is the binary's main runtime.
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime::register(&provider.meter(runtime::SCOPE), &runtime);
    }
    Ok(provider)
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
        .with_resource(resource(service_name, DEFAULT_SERVICE_VERSION))
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
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::data::{ResourceMetrics, ScopeMetrics};

    /// RFC0038.4 / #618 — a disabled signal installs no pipeline. With every
    /// signal off, `init` succeeds and the guard holds no meter/logger/tracer
    /// provider, so instruments/spans resolve to the global no-op and nothing
    /// exports. Each provider is built *inside* its `if <signal>_enabled` block,
    /// so a regression that installed a disabled pipeline fails this test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn init_with_signals_disabled_installs_no_pipelines() {
        let guard = init(&TelemetryConfig {
            service_name: "ourios-test-disabled".into(),
            service_version: None,
            otlp_endpoint: None,
            traces_enabled: false,
            metrics_enabled: false,
            logs_enabled: false,
        })
        .expect("init succeeds with every signal disabled");
        assert!(
            guard.provider.is_none(),
            "no meter provider when metrics disabled",
        );
        assert!(guard.logger.is_none(), "no logger when logs disabled");
        assert!(guard.tracer.is_none(), "no tracer when traces disabled");
    }

    /// #618 — `exporter_selected` reads an `OTEL_{TRACES,METRICS,LOGS}_EXPORTER`
    /// value as an on/off switch, not a full selector: `none` (any
    /// casing/whitespace) is the only value that turns the signal off; unset and
    /// every other value (`otlp`, `console`, …) leave it on (always OTLP).
    #[test]
    fn exporter_selected_treats_none_as_off() {
        assert!(exporter_selected(None), "unset → on (OTLP)");
        assert!(exporter_selected(Some("otlp")), "otlp → on");
        assert!(
            exporter_selected(Some("console")),
            "unsupported selector → still on (OTLP), not off",
        );
        assert!(!exporter_selected(Some("none")), "none → off");
        assert!(!exporter_selected(Some("  none  ")), "trimmed none → off");
        assert!(
            !exporter_selected(Some("None")),
            "case-insensitive none → off"
        );
    }

    /// `http/protobuf` selects OTLP/HTTP; the signal-specific variable wins over
    /// the general one, and unset or any other value keeps gRPC.
    #[cfg(feature = "http-proto")]
    #[test]
    fn protocol_env_selects_http_protobuf() {
        assert!(!is_http_protobuf(None, None), "unset keeps gRPC");
        assert!(is_http_protobuf(None, Some("http/protobuf")));
        assert!(is_http_protobuf(None, Some(" http/protobuf ")));
        assert!(!is_http_protobuf(None, Some("grpc")));
        assert!(
            !is_http_protobuf(None, Some("http/json")),
            "JSON is not built"
        );
        assert!(is_http_protobuf(Some("http/protobuf"), Some("grpc")));
        assert!(!is_http_protobuf(Some("grpc"), Some("http/protobuf")));
        assert!(
            is_http_protobuf(Some(""), Some("http/protobuf")),
            "an empty signal-specific protocol counts as unset"
        );
        assert!(is_http_protobuf(Some("  "), Some("http/protobuf")));
        assert!(!is_http_protobuf(Some(""), Some("")));
        assert!(
            is_http_protobuf(None, Some("HTTP/PROTOBUF")),
            "the protocol enum is case-insensitive"
        );
        assert!(is_http_protobuf(Some("Http/Protobuf"), Some("grpc")));
        assert!(!is_http_protobuf(Some("GrPc"), Some("http/protobuf")));
    }

    /// Metrics export over OTLP/HTTP is configured only with an endpoint (the
    /// metrics-specific one, else the general one) and a metrics protocol of
    /// `http/protobuf`; empty values count as unset.
    #[cfg(feature = "http-proto")]
    #[test]
    fn metrics_http_export_needs_an_endpoint_and_http_protobuf() {
        let http = Some("http/protobuf");
        let url = Some("https://otlp.example");
        assert!(
            !http_export_configured(None, None, None, http),
            "no endpoint"
        );
        assert!(!http_export_configured(Some(""), Some(" "), None, http));
        assert!(http_export_configured(None, url, None, http));
        assert!(http_export_configured(url, None, None, http));
        assert!(http_export_configured(Some(""), url, None, http));
        assert!(
            !http_export_configured(None, url, None, None),
            "gRPC default"
        );
        assert!(!http_export_configured(None, url, None, Some("grpc")));
        assert!(http_export_configured(None, url, http, Some("grpc")));
        assert!(!http_export_configured(None, url, Some("grpc"), http));
        assert!(http_export_configured(None, url, Some(""), http));
    }

    /// The env the re-executed child of
    /// `http_protobuf_metrics_export_reaches_the_endpoint` reads.
    #[cfg(feature = "http-proto")]
    const HTTP_EXPORT_CHILD: &str = "OURIOS_TELEMETRY_HTTP_EXPORT_CHILD";

    /// The OTLP/HTTP path end to end, as the fuzz harnesses use it: `init` with
    /// only the standard env, in a process with no tokio runtime, exports a
    /// recorded metric as a protobuf `POST /v1/metrics` carrying the configured
    /// headers. The child process owns the env, so this test mutates none.
    #[cfg(feature = "http-proto")]
    #[test]
    fn http_protobuf_metrics_export_reaches_the_endpoint() {
        let sink = otlp_sink::OtlpSink::start();
        let exe = std::env::current_exe().expect("test binary path");
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                "tests::http_export_child",
                "--ignored",
                "--nocapture",
            ])
            .env(HTTP_EXPORT_CHILD, "1")
            .env("OTEL_EXPORTER_OTLP_ENDPOINT", sink.url())
            .env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf")
            .env(
                "OTEL_EXPORTER_OTLP_HEADERS",
                "Authorization=Bearer test-token",
            )
            .env("RUST_LOG", "off")
            .env_remove("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
            .env_remove("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL")
            .env_remove("OTEL_SDK_DISABLED")
            .env_remove("OTEL_METRICS_EXPORTER")
            .output()
            .expect("re-execute the test binary");
        let requests = sink.stop();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "child test failed or did not run:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr),
        );

        let metrics = requests
            .iter()
            .find(|request| request.head.starts_with("POST /v1/metrics "))
            .unwrap_or_else(|| panic!("no POST /v1/metrics, got {requests:?}"));
        let head = metrics.head.to_ascii_lowercase();
        assert!(
            head.contains("\r\ncontent-type: application/x-protobuf\r\n"),
            "{head}"
        );
        assert!(
            head.contains("\r\nauthorization: bearer test-token\r\n"),
            "{head}"
        );
        assert!(
            metrics
                .body
                .windows(b"ourios.compaction.sweeps".len())
                .any(|w| w == b"ourios.compaction.sweeps"),
            "the recorded instrument should be in the protobuf payload",
        );
    }

    #[cfg(feature = "http-proto")]
    #[test]
    #[ignore = "re-executed by http_protobuf_metrics_export_reaches_the_endpoint"]
    fn http_export_child() {
        if std::env::var_os(HTTP_EXPORT_CHILD).is_none() {
            return;
        }
        assert!(tokio::runtime::Handle::try_current().is_err(), "no runtime");
        assert!(metrics_http_export_configured());
        let guard = init(&TelemetryConfig {
            traces_enabled: false,
            logs_enabled: false,
            ..TelemetryConfig::new("ourios-test")
        })
        .expect("init over OTLP/HTTP without a runtime");
        global::meter("ourios.compaction")
            .u64_counter("ourios.compaction.sweeps")
            .build()
            .add(1, &[]);
        guard.force_flush().expect("the export is acknowledged");
    }

    // Build a provider over an in-memory exporter (no global state, no
    // OTLP endpoint), wrap it in a guard, and assert that the guard's
    // `force_flush` exports the recorded instrument.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn guard_force_flush_exports_recorded_metrics() {
        // Arrange.
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .with_resource(resource("ourios-test", DEFAULT_SERVICE_VERSION))
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

        // The `process` entity's identity rides on the resource, so replicas
        // and restarts export distinguishable series.
        let resource = resource_metrics[0].resource();
        assert_eq!(
            resource.get(&"process.pid".into()),
            Some(opentelemetry::Value::I64(i64::from(std::process::id()))),
        );
        let created = resource
            .get(&"process.creation.time".into())
            .expect("process.creation.time on the resource")
            .as_str()
            .into_owned();
        let created = chrono::DateTime::parse_from_rfc3339(&created)
            .unwrap_or_else(|e| panic!("{created:?} is not ISO 8601: {e}"));
        assert!(
            created.timestamp_millis()
                <= chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now())
                    .timestamp_millis(),
            "the process was created in the past, got {created}",
        );

        // The release rides on the resource too, so telemetry compares by version.
        assert_eq!(
            resource.get(&"service.version".into()),
            Some(opentelemetry::Value::from(env!("CARGO_PKG_VERSION"))),
        );
    }

    /// The configured `service_version` names the release; unset, it falls
    /// back to this crate's (workspace) version.
    #[test]
    fn config_service_version_defaults_to_the_crate_version() {
        let version = |config: &TelemetryConfig| config.resource().get(&"service.version".into());
        assert_eq!(
            version(&TelemetryConfig::new("ourios-test")),
            Some(opentelemetry::Value::from(env!("CARGO_PKG_VERSION"))),
        );
        let pinned = TelemetryConfig {
            service_version: Some("1.2.3".into()),
            ..TelemetryConfig::new("ourios-test")
        };
        assert_eq!(version(&pinned), Some(opentelemetry::Value::from("1.2.3")));
    }

    /// The env var the re-executed child of
    /// `otel_resource_attributes_service_version_wins` reads, so the child runs
    /// with the process environment set before start rather than mutating the
    /// shared environment of this test binary.
    const RESOURCE_ATTRIBUTES_CHILD: &str = "OURIOS_TELEMETRY_RESOURCE_ATTRIBUTES_CHILD";

    /// An operator's `OTEL_RESOURCE_ATTRIBUTES=service.version=…` wins over the
    /// configured version, while `service.name` stays the configured one. Runs
    /// the assertion in a re-executed copy of this test binary, since the env
    /// detector reads process-global state.
    #[test]
    fn otel_resource_attributes_service_version_wins() {
        let exe = std::env::current_exe().expect("test binary path");
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                "tests::resource_attributes_child",
                "--ignored",
                "--nocapture",
            ])
            .env(RESOURCE_ATTRIBUTES_CHILD, "1")
            .env(
                "OTEL_RESOURCE_ATTRIBUTES",
                "service.version=9.9.9-operator,service.name=operator-name",
            )
            .env_remove("OTEL_SERVICE_NAME")
            .output()
            .expect("re-execute the test binary");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "child test failed or did not run:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    #[ignore = "re-executed by otel_resource_attributes_service_version_wins"]
    fn resource_attributes_child() {
        if std::env::var_os(RESOURCE_ATTRIBUTES_CHILD).is_none() {
            return;
        }
        let resource = resource("ourios-test", "0.0.0-configured");
        assert_eq!(
            resource.get(&"service.version".into()),
            Some(opentelemetry::Value::from("9.9.9-operator")),
        );
        assert_eq!(
            resource.get(&"service.name".into()),
            Some(opentelemetry::Value::from("ourios-test")),
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
            .with_resource(resource("ourios-test", DEFAULT_SERVICE_VERSION))
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
            .with_resource(resource("ourios-test", DEFAULT_SERVICE_VERSION))
            .build();
        let log_exporter = InMemoryLogExporter::default();
        let logger = SdkLoggerProvider::builder()
            .with_simple_exporter(log_exporter.clone())
            .with_resource(resource("ourios-test", DEFAULT_SERVICE_VERSION))
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

    /// A minimal OTLP/HTTP endpoint on a `std::net` listener: it answers every
    /// request `200` and closes the connection, recording what it received.
    #[cfg(feature = "http-proto")]
    mod otlp_sink {
        use std::io::{BufRead as _, BufReader, Read as _, Write as _};
        use std::net::{TcpListener, TcpStream};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread::JoinHandle;
        use std::time::Duration;

        #[derive(Debug)]
        pub(super) struct Request {
            /// Request line and headers, CRLF-separated as received.
            pub(super) head: String,
            pub(super) body: Vec<u8>,
        }

        pub(super) struct OtlpSink {
            url: String,
            stop: Arc<AtomicBool>,
            server: JoinHandle<Vec<Request>>,
        }

        impl OtlpSink {
            pub(super) fn start() -> Self {
                let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
                listener.set_nonblocking(true).expect("non-blocking accept");
                let url = format!("http://{}", listener.local_addr().expect("local addr"));
                let stop = Arc::new(AtomicBool::new(false));
                let server = {
                    let stop = Arc::clone(&stop);
                    std::thread::spawn(move || serve(&listener, &stop))
                };
                Self { url, stop, server }
            }

            pub(super) fn url(&self) -> &str {
                &self.url
            }

            pub(super) fn stop(self) -> Vec<Request> {
                self.stop.store(true, Ordering::Relaxed);
                self.server.join().expect("sink thread")
            }
        }

        fn serve(listener: &TcpListener, stop: &AtomicBool) -> Vec<Request> {
            let mut requests = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => requests.extend(answer(stream)),
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
            requests
        }

        fn answer(stream: TcpStream) -> Option<Request> {
            stream.set_nonblocking(false).ok()?;
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .ok()?;
            let mut reader = BufReader::new(stream);
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).ok()? == 0 {
                    return None;
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).ok()?;
            reader
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .ok()?;
            Some(Request { head, body })
        }
    }
}
