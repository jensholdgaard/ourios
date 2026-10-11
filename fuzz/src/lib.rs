//! Shared support for the fuzz targets.
//!
//! [`init_telemetry`] is what every target passes to `fuzz_target!(init: …)`.
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

/// The env var that switches export on; unset or empty leaves the target
/// exactly as it was, with no provider, no subscriber and no threads.
const ENDPOINT_VAR: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// The harness exports over OTLP/HTTP only: the gRPC exporter needs a tokio
/// runtime the targets do not have.
const PROTOCOL_VAR: &str = "OTEL_EXPORTER_OTLP_PROTOCOL";
const HTTP_PROTOBUF: &str = "http/protobuf";

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

/// Install the `OTel` providers once, when `OTEL_EXPORTER_OTLP_ENDPOINT` is
/// set. Reads only the environment, never fuzz input, and an export failure is
/// logged by the SDK and dropped, so it cannot change what a target does.
pub fn init_telemetry() {
    TELEMETRY.get_or_init(|| {
        install(
            std::env::var(ENDPOINT_VAR).ok().as_deref(),
            std::env::var(PROTOCOL_VAR).ok().as_deref(),
        )
    });
}

fn install(endpoint: Option<&str>, protocol: Option<&str>) -> Option<TelemetryGuard> {
    if endpoint.is_none_or(|endpoint| endpoint.trim().is_empty()) {
        return None;
    }
    if protocol.map(str::trim) != Some(HTTP_PROTOBUF) {
        eprintln!("ourios-fuzz: telemetry off, {PROTOCOL_VAR} must be {HTTP_PROTOBUF}");
        return None;
    }
    // `service_version` stays unset: it falls back to the workspace release
    // (this crate's own version is a placeholder 0.0.0).
    let config = TelemetryConfig {
        // One span per iteration would swamp the exporter and the backend.
        traces_enabled: false,
        // Nothing on the fuzzed paths logs at warn or error, and the batch
        // log processor's thread would still wake every second, adding
        // background coverage (see the module docs).
        // Warnings stay on stderr through the `fmt` layer.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_endpoint_installs_nothing() {
        assert!(install(None, Some(HTTP_PROTOBUF)).is_none());
        assert!(install(Some(""), Some(HTTP_PROTOBUF)).is_none());
        assert!(install(Some("  "), Some(HTTP_PROTOBUF)).is_none());
    }

    #[test]
    fn grpc_or_unset_protocol_installs_nothing() {
        let endpoint = Some("http://127.0.0.1:4318");
        assert!(install(endpoint, None).is_none());
        assert!(install(endpoint, Some("grpc")).is_none());
    }
}
