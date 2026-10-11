//! Telemetry for the fuzz targets, shared as a source file (each target pulls
//! it in with `#[path]`) so the fuzz package keeps no library API (RFC 0015
//! §3.1).
//!
//! Fuzz input is an adversarial workload for the code under test, so the
//! metrics that code already records through the global `OTel` meter (the
//! miner's §3.1 instruments, the process metrics) are worth watching while it
//! runs. Nothing here describes the fuzzer itself, and no `ourios.*` names are
//! added.
//!
//! With export on, the SDK's reader and exporter threads run instrumented code
//! concurrently with the target, and libFuzzer counts that coverage against
//! whichever input is executing. Crash detection and reproduction are
//! unaffected (a reproducer runs without the env), but the working corpus can
//! gain inputs that are not interesting on their own, so only switch export on
//! where the corpus is throwaway, as in the smoke-fuzz job.

#![deny(unsafe_code)]

use std::sync::OnceLock;

use ourios_telemetry::{TelemetryConfig, TelemetryGuard};

/// One logical service for every target (each target is told apart by
/// `cicd.pipeline.task.name`). Distinct from `ourios-server`: the semantic
/// conventions treat equal `service.name`s as the same service whatever the
/// `deployment.environment.name`, and a fuzz harness is not the server.
const SERVICE_NAME: &str = "ourios-fuzz";

/// Holds the providers for the life of the process. libFuzzer ends with
/// `exit()`, which runs no Rust destructors, so the guard never flushes: the
/// periodic reader's last interval before exit (and any crash) is lost. Keep
/// `OTEL_METRIC_EXPORT_INTERVAL` short where that matters.
static TELEMETRY: OnceLock<Option<TelemetryGuard>> = OnceLock::new();

/// Install the `OTel` metrics provider once, when the standard env points
/// metrics at an OTLP/HTTP endpoint with protobuf payloads
/// ([`ourios_telemetry::metrics_http_export_configured`], unit-tested there).
/// Anything else, including no endpoint, leaves the target exactly as it is
/// without telemetry: no provider, no subscriber, no threads. gRPC is not an
/// option because its exporter needs a tokio runtime the targets do not have.
/// Reads only the environment, never fuzz input, and a failed export is
/// dropped by the SDK, so it cannot change what a target does.
pub fn init() {
    TELEMETRY.get_or_init(|| {
        if !ourios_telemetry::metrics_http_export_configured() {
            return None;
        }
        // `service_version` stays unset: it falls back to the workspace
        // release (the fuzz package's own version is a placeholder 0.0.0).
        let config = TelemetryConfig {
            // One span per iteration would swamp the exporter and the backend.
            traces_enabled: false,
            // Nothing on the fuzzed paths logs at warn or error, and the batch
            // log processor's thread would still wake every second, adding
            // background coverage. Warnings stay on stderr through the `fmt`
            // layer.
            logs_enabled: false,
            ..TelemetryConfig::new(SERVICE_NAME)
        };
        match ourios_telemetry::init(&config) {
            Ok(guard) => Some(guard),
            Err(e) => {
                eprintln!("ourios-fuzz: telemetry off, {e}");
                None
            }
        }
    });
}
