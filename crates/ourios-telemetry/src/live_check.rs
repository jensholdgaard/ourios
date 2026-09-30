//! Emission-time checks of named log events, for tests.
//!
//! The CI `live-check` job runs the real binary against
//! `weaver registry live-check`, but an event that fires only when a disk
//! fails or a certificate goes bad never fires there, so the live-check
//! never sees it. A test that drives such an event captures what the
//! `tracing` bridge exported and hands it to [`live_check`]: always
//! against the caller's per-event specification of the registry (each
//! event's required and optional attributes), and through weaver as well
//! when `OURIOS_LIVE_CHECK_WEAVER` (the binary) and
//! `OURIOS_LIVE_CHECK_REGISTRY` (the pinned registry's `registry/`
//! directory) are set. Only the weaver mode checks against the registry
//! itself; the other checks against what the caller says it declares.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use opentelemetry::logs::AnyValue;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLogRecord, SdkLoggerProvider};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// One exported log record that carries an event name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub name: &'static str,
    pub severity: i32,
    pub attributes: BTreeMap<String, String>,
}

impl Event {
    fn of(record: &SdkLogRecord) -> Option<Self> {
        Some(Self {
            name: record.event_name()?,
            severity: record.severity_number().map_or(0, |s| s as i32),
            attributes: record
                .attributes_iter()
                .map(|(key, value)| (key.as_str().to_owned(), render(value)))
                .collect(),
        })
    }

    /// The event's `error.type`, if it carries one.
    #[must_use]
    pub fn error_type(&self) -> Option<&str> {
        self.attributes.get("error.type").map(String::as_str)
    }
}

fn render(value: &AnyValue) -> String {
    match value {
        AnyValue::String(s) => s.as_str().to_owned(),
        AnyValue::Int(i) => i.to_string(),
        AnyValue::Boolean(b) => b.to_string(),
        other => format!("{other:?}"),
    }
}

/// The process-global `tracing` subscriber bridged onto an in-memory
/// log exporter — the same appender the server's logs pipeline uses.
pub struct EventCapture {
    logs: InMemoryLogExporter,
    _provider: SdkLoggerProvider,
}

impl EventCapture {
    /// Forget everything captured so far.
    pub fn reset(&self) {
        self.logs.reset();
    }

    /// The named events captured since the last reset, in order.
    #[must_use]
    pub fn events(&self) -> Vec<Event> {
        self.logs
            .get_emitted_logs()
            .unwrap_or_default()
            .iter()
            .filter_map(|log| Event::of(&log.record))
            .collect()
    }
}

/// Install the capture as the global subscriber, once per process.
///
/// # Errors
///
/// When another global subscriber was installed first: events would
/// then go there and the capture would read none.
pub fn event_capture() -> Result<&'static EventCapture, String> {
    static CAPTURE: OnceLock<Result<EventCapture, String>> = OnceLock::new();
    CAPTURE
        .get_or_init(|| {
            let logs = InMemoryLogExporter::default();
            let provider = SdkLoggerProvider::builder()
                .with_simple_exporter(logs.clone())
                .build();
            tracing_subscriber::registry()
                .with(OpenTelemetryTracingBridge::new(&provider))
                .try_init()
                .map_err(|e| format!("a global subscriber is already installed: {e}"))?;
            Ok(EventCapture {
                logs,
                _provider: provider,
            })
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// What the registry declares for one event, as the caller states it.
#[derive(Debug, Clone, Copy)]
pub struct EventSpec {
    pub name: &'static str,
    /// Attributes every emission must carry.
    pub required: &'static [&'static str],
    /// Attributes an emission may carry.
    pub optional: &'static [&'static str],
}

/// How far a [`live_check`] went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checked {
    /// Every emission against its [`EventSpec`], then through weaver
    /// against the registry itself.
    Weaver,
    /// Every emission against its [`EventSpec`] only: weaver is not
    /// configured here, so the registry itself was not consulted.
    SpecOnly,
}

/// Check that every event in `specs` was emitted, and that every
/// emission of it carries each required attribute and nothing outside
/// its required and optional ones; then, when configured, that
/// `weaver registry live-check` exits cleanly with no violation for them.
///
/// # Errors
///
/// The first failed check, described.
pub fn live_check(events: &[Event], specs: &[EventSpec]) -> Result<Checked, String> {
    let mut emitted = Vec::new();
    for spec in specs {
        let mine: Vec<&Event> = events.iter().filter(|e| e.name == spec.name).collect();
        if mine.is_empty() {
            return Err(format!(
                "{} was never emitted, so no live-check would ever check it",
                spec.name
            ));
        }
        for event in &mine {
            check_attributes(event, spec)?;
        }
        emitted.extend(mine);
    }
    let (Some(weaver), Some(registry)) = (
        std::env::var_os("OURIOS_LIVE_CHECK_WEAVER"),
        std::env::var_os("OURIOS_LIVE_CHECK_REGISTRY"),
    ) else {
        return Ok(Checked::SpecOnly);
    };
    let names: Vec<&str> = specs.iter().map(|spec| spec.name).collect();
    weaver_live_check(
        &PathBuf::from(weaver),
        &PathBuf::from(registry),
        &emitted,
        &names,
    )?;
    Ok(Checked::Weaver)
}

fn check_attributes(event: &Event, spec: &EventSpec) -> Result<(), String> {
    if let Some(missing) = spec
        .required
        .iter()
        .find(|key| !event.attributes.contains_key(**key))
    {
        return Err(format!(
            "{}: the required attribute `{missing}` is missing",
            event.name
        ));
    }
    match event.attributes.keys().find(|key| {
        !spec.required.contains(&key.as_str()) && !spec.optional.contains(&key.as_str())
    }) {
        Some(key) => Err(format!(
            "{}: `{key}` is not an attribute the registry declares for it",
            event.name
        )),
        None => Ok(()),
    }
}

fn weaver_live_check(
    weaver: &Path,
    registry: &Path,
    events: &[&Event],
    expected: &[&str],
) -> Result<(), String> {
    let dir = std::env::temp_dir().join(format!("ourios-live-check-{}", std::process::id()));
    let report_dir = dir.join("report");
    std::fs::create_dir_all(&report_dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let samples = dir.join("samples.json");
    std::fs::write(&samples, samples_json(events).to_string())
        .map_err(|e| format!("write {}: {e}", samples.display()))?;
    let status = std::process::Command::new(weaver)
        .args(["registry", "live-check", "--future", "-r"])
        .arg(registry)
        .arg("--input-source")
        .arg(&samples)
        .args(["--input-format", "json", "--format", "json", "--no-stream"])
        .arg("--output")
        .arg(&report_dir)
        .status()
        .map_err(|e| format!("run {}: {e}", weaver.display()))?;
    let report = std::fs::read(report_dir.join("live_check.json"))
        .map_err(|e| format!("weaver (exit {status}) wrote no report: {e}"));
    // Best-effort: a leftover scratch directory fails nothing.
    let _ = std::fs::remove_dir_all(&dir);
    let report = report?;
    let report: serde_json::Value =
        serde_json::from_slice(&report).map_err(|e| format!("weaver report: {e}"))?;
    let mut violations = Vec::new();
    collect_violations(&report, &mut violations);
    if !violations.is_empty() {
        return Err(format!(
            "weaver live-check (exit {status}) found violations: {violations:#?}"
        ));
    }
    if !status.success() {
        return Err(format!(
            "weaver live-check failed (exit {status}) without reporting a violation"
        ));
    }
    let seen: Vec<&str> = report["samples"]
        .as_array()
        .map(|samples| {
            samples
                .iter()
                .filter_map(|s| s["log"]["event_name"].as_str())
                .collect()
        })
        .unwrap_or_default();
    match expected.iter().find(|name| !seen.contains(name)) {
        Some(unseen) => Err(format!("weaver never saw {unseen}")),
        None => Ok(()),
    }
}

/// weaver's JSON sample shape for a log record.
fn samples_json(events: &[&Event]) -> serde_json::Value {
    serde_json::Value::Array(
        events
            .iter()
            .map(|event| {
                let attributes: Vec<serde_json::Value> = event
                    .attributes
                    .iter()
                    .map(|(name, value)| serde_json::json!({ "name": name, "value": value }))
                    .collect();
                serde_json::json!({ "log": {
                    "event_name": event.name,
                    "severity_number": event.severity,
                    "attributes": attributes,
                }})
            })
            .collect(),
    )
}

fn collect_violations(value: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("type").and_then(serde_json::Value::as_str) == Some("PolicyFinding")
                && map.get("level").and_then(serde_json::Value::as_str) == Some("violation")
            {
                out.push(value.clone());
            }
            map.values().for_each(|v| collect_violations(v, out));
        }
        serde_json::Value::Array(items) => items.iter().for_each(|v| collect_violations(v, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{Event, EventSpec, check_attributes};

    const SPEC: EventSpec = EventSpec {
        name: "ourios.test.event",
        required: &["error.type"],
        optional: &["ourios.test.optional"],
    };

    fn event(attributes: &[&str]) -> Event {
        Event {
            name: SPEC.name,
            severity: 13,
            attributes: attributes
                .iter()
                .map(|key| ((*key).to_owned(), "x".to_owned()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn an_emission_is_held_to_its_own_spec() {
        assert!(check_attributes(&event(&["error.type"]), &SPEC).is_ok());
        assert!(check_attributes(&event(&["error.type", "ourios.test.optional"]), &SPEC).is_ok());
        assert!(
            check_attributes(&event(&[]), &SPEC).is_err(),
            "a missing required attribute fails"
        );
        assert!(
            check_attributes(&event(&["error.type", "error"]), &SPEC).is_err(),
            "an undeclared attribute fails"
        );
    }
}
