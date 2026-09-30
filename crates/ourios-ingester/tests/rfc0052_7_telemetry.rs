//! RFC0052.7 — The WAL's state is exported.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Its own test binary, like `perf_metrics.rs` (RFC0028.2 exempt list
//! in `README.md`): it installs the **global** in-memory meter provider
//! and a global `tracing` subscriber bridged onto an in-memory log
//! exporter, and two global-installing tests in one binary would race.
//! The three legs here share both installs, so they run one at a time.

#[path = "it/ingest_support/mod.rs"]
mod ingest_support;
#[path = "it/rfc0052_barrier_support.rs"]
mod rfc0052_barrier_support;

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use opentelemetry::logs::AnyValue;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLogRecord, SdkLoggerProvider};
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use ourios_ingester::barrier::CutOutcome;
use ourios_ingester::housekeeping::{Housekeeper, HousekeepingTick};
use ourios_ingester::receiver::{CommitCoordinator, Journal, ReceiveError};
use ourios_semconv as semconv;
use ourios_telemetry::TelemetryGuard;
use ourios_wal::{RotationFault, RotationSite, RotationState, WalOffset};
use tracing_subscriber::layer::SubscriberExt;

use crate::rfc0052_barrier_support::{BarrierRig, JournalFaults, RigSpec, wal_config};

const CAP: usize = 128;

/// The RFC 0052 log events, every one of which the live-check leg must
/// see emitted.
const RFC0052_EVENTS: [&str; 8] = [
    semconv::EVENT_OURIOS_RECEIVER_WAL_CHECKPOINT_ERROR,
    semconv::EVENT_OURIOS_RECEIVER_WAL_IDLE_ROTATION_ERROR,
    semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_RETRYING,
    semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_RECOVERED,
    semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_TERMINAL,
    semconv::EVENT_OURIOS_RECEIVER_WAL_RETAIN_FLOOR_PINNED,
    semconv::EVENT_OURIOS_RECEIVER_WAL_RETAIN_FLOOR_LIFTED,
    semconv::EVENT_OURIOS_RECEIVER_BARRIER_LATCHED,
];

/// Every RFC0052.7 instrument: the WAL state and the barrier's.
const INSTRUMENTS: [&str; 19] = [
    semconv::OURIOS_WAL_SIZE,
    semconv::OURIOS_WAL_UNFLUSHED_SIZE,
    semconv::OURIOS_WAL_SEGMENT_COUNT,
    semconv::OURIOS_WAL_UNRECLAIMED_SIZE,
    semconv::OURIOS_WAL_UNRECLAIMED_AGE,
    semconv::OURIOS_WAL_RETAIN_FLOOR_STATUS,
    semconv::OURIOS_WAL_RETAIN_FLOOR_PINNED_TENANT_COUNT,
    semconv::OURIOS_WAL_RETAIN_FLOOR_LAG_SIZE,
    semconv::OURIOS_WAL_RETAIN_FLOOR_LAG_SEGMENT_COUNT,
    semconv::OURIOS_WAL_ROTATION_STATUS,
    semconv::OURIOS_WAL_ROTATION_CONSECUTIVE_FAILURES,
    semconv::OURIOS_WAL_HOUSEKEEPING_HORIZON_REMAINING,
    semconv::OURIOS_WAL_HOUSEKEEPING_UNLINK_REMAINING,
    semconv::OURIOS_INGEST_BARRIER_CUTS,
    semconv::OURIOS_INGEST_BARRIER_CAPTURES,
    semconv::OURIOS_INGEST_BARRIER_CHECKPOINT_WRITES,
    semconv::OURIOS_INGEST_BARRIER_SNAPSHOT_WRITES,
    semconv::OURIOS_INGEST_BARRIER_EPOCH,
    semconv::OURIOS_INGEST_BARRIER_FAILED_EPOCH,
];

/// The process-global installs every leg reads through.
struct Harness {
    metrics_guard: TelemetryGuard,
    metrics: InMemoryMetricExporter,
    logs: InMemoryLogExporter,
    _logger: SdkLoggerProvider,
}

fn harness() -> &'static Harness {
    static HARNESS: OnceLock<Harness> = OnceLock::new();
    HARNESS.get_or_init(|| {
        let (metrics_guard, metrics) = ourios_telemetry::init_in_memory("ourios-rfc0052-7");
        let logs = InMemoryLogExporter::default();
        let logger = SdkLoggerProvider::builder()
            .with_simple_exporter(logs.clone())
            .build();
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(OpenTelemetryTracingBridge::new(&logger)),
        )
        .expect("the only subscriber this binary installs");
        Harness {
            metrics_guard,
            metrics,
            logs,
            _logger: logger,
        }
    })
}

/// One leg at a time: every leg resets and reads the shared exporters.
async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let guard = SERIAL.lock().await;
    let harness = harness();
    harness.logs.reset();
    guard
}

impl Harness {
    /// One collection, and only that one.
    fn collect(&self) -> Vec<ResourceMetrics> {
        self.metrics.reset();
        self.metrics_guard.force_flush().expect("force_flush");
        self.metrics
            .get_finished_metrics()
            .expect("metrics exported")
    }

    /// The named events emitted since the leg began, in order.
    fn events(&self) -> Vec<Event> {
        self.logs
            .get_emitted_logs()
            .expect("logs exported")
            .iter()
            .filter_map(|log| Event::of(&log.record))
            .collect()
    }
}

/// One exported log record that carries an event name.
#[derive(Debug, Clone)]
struct Event {
    name: &'static str,
    severity: i32,
    attributes: BTreeMap<String, String>,
}

impl Event {
    fn of(record: &SdkLogRecord) -> Option<Self> {
        let name = record.event_name()?;
        let attributes = record
            .attributes_iter()
            .map(|(key, value)| (key.as_str().to_owned(), render(value)))
            .collect();
        Some(Self {
            name,
            severity: record.severity_number().map_or(0, |s| s as i32),
            attributes,
        })
    }

    fn error_type(&self) -> Option<&str> {
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

/// A datapoint, typed as the SDK aggregated it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Value {
    I(i64),
    U(u64),
    F(f64),
}

/// Every datapoint of `name`, keyed by its attributes.
fn points(rms: &[ResourceMetrics], name: &str) -> Vec<(BTreeMap<String, String>, Value)> {
    fn attrs<'a>(
        kvs: impl Iterator<Item = &'a opentelemetry::KeyValue>,
    ) -> BTreeMap<String, String> {
        kvs.map(|kv| (kv.key.as_str().to_owned(), kv.value.as_str().into_owned()))
            .collect()
    }
    let mut out = Vec::new();
    for metric in rms
        .iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .filter(|m| m.name() == name)
    {
        match metric.data() {
            AggregatedMetrics::I64(MetricData::Sum(sum)) => {
                out.extend(
                    sum.data_points()
                        .map(|p| (attrs(p.attributes()), Value::I(p.value()))),
                );
            }
            AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                out.extend(
                    sum.data_points()
                        .map(|p| (attrs(p.attributes()), Value::U(p.value()))),
                );
            }
            AggregatedMetrics::U64(MetricData::Gauge(gauge)) => {
                out.extend(
                    gauge
                        .data_points()
                        .map(|p| (attrs(p.attributes()), Value::U(p.value()))),
                );
            }
            AggregatedMetrics::F64(MetricData::Gauge(gauge)) => {
                out.extend(
                    gauge
                        .data_points()
                        .map(|p| (attrs(p.attributes()), Value::F(p.value()))),
                );
            }
            other => panic!("{name}: unexpected aggregation {other:?}"),
        }
    }
    out
}

/// The single attribute-free value of `name`.
fn value(rms: &[ResourceMetrics], name: &str) -> Value {
    match points(rms, name).as_slice() {
        [(attributes, value)] if attributes.is_empty() => *value,
        other => panic!("{name}: expected one attribute-free point, got {other:?}"),
    }
}

/// A state metric's series, by its state attribute.
fn states(rms: &[ResourceMetrics], name: &str, key: &str) -> BTreeMap<String, i64> {
    points(rms, name)
        .into_iter()
        .map(|(attributes, value)| {
            let Value::I(v) = value else {
                panic!("{name} is an i64 UpDownCounter");
            };
            (
                attributes.get(key).cloned().expect("the state attribute"),
                v,
            )
        })
        .collect()
}

/// A counter's total over the points whose `key` is `want`.
fn counted(rms: &[ResourceMetrics], name: &str, key: &str, want: Option<&str>) -> u64 {
    points(rms, name)
        .into_iter()
        .filter(|(attributes, _)| attributes.get(key).map(String::as_str) == want)
        .map(|(_, value)| match value {
            Value::U(v) => v,
            other => panic!("{name} is a u64 counter, got {other:?}"),
        })
        .sum()
}

/// The barrier counters carry their outcomes, and successes carry no
/// error.type. Counters are cumulative for the process, so one cut is
/// read as the difference between the collections either side of it.
fn assert_one_cut_counted(before: &[ResourceMetrics], after: &[ResourceMetrics]) {
    for (name, key, want) in [
        (
            semconv::OURIOS_INGEST_BARRIER_CUTS,
            semconv::OURIOS_INGEST_BARRIER_CUT_OUTCOME,
            Some("stamped"),
        ),
        (
            semconv::OURIOS_INGEST_BARRIER_CAPTURES,
            semconv::OURIOS_INGEST_BARRIER_CAPTURE_OUTCOME,
            Some("filled"),
        ),
        (
            semconv::OURIOS_INGEST_BARRIER_CHECKPOINT_WRITES,
            "error.type",
            None,
        ),
        (
            semconv::OURIOS_INGEST_BARRIER_SNAPSHOT_WRITES,
            "error.type",
            None,
        ),
    ] {
        assert_eq!(
            counted(after, name, key, want) - counted(before, name, key, want),
            1,
            "{name} {key}={want:?} counts the one cut",
        );
    }
}

fn housekeeper(rig: &BarrierRig) -> Arc<Housekeeper> {
    Arc::new(Housekeeper::new(
        Arc::clone(&rig.commits),
        Arc::clone(&rig.barrier),
        rig.publish.clone(),
        CAP,
    ))
}

async fn tick(housekeeper: &Arc<Housekeeper>) -> HousekeepingTick {
    let housekeeper = Arc::clone(housekeeper);
    tokio::task::spawn_blocking(move || housekeeper.tick())
        .await
        .expect("the tick catches its own unwind")
}

async fn cut(rig: &Arc<BarrierRig>, rotate_when_idle: bool) -> CutOutcome {
    let rig = Arc::clone(rig);
    tokio::task::spawn_blocking(move || rig.barrier.tick(&rig.pipeline, rotate_when_idle))
        .await
        .expect("the barrier tick catches its own unwind")
}

fn rig_with(tmp: &Path, faults: &Arc<JournalFaults>) -> Arc<BarrierRig> {
    Arc::new(BarrierRig::build(
        tmp,
        RigSpec {
            journal_faults: Some(Arc::clone(faults)),
            ..RigSpec::new(wal_config(&tmp.join("wal")))
        },
    ))
}

/// A journal whose rotation state follows a script, one step per
/// append: the append path the coordinator emits the rotation edges on,
/// driven through states a real disk cannot be made to fail into on
/// command.
struct ScriptedRotation {
    script: VecDeque<RotationState>,
    state: RotationState,
    byte: u64,
}

impl Journal for ScriptedRotation {
    fn append_batch(&mut self, _payload: &[u8]) -> Result<WalOffset, ReceiveError> {
        if let Some(next) = self.script.pop_front() {
            self.state = next;
        }
        self.byte += 1;
        Ok(self.offset())
    }

    fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
        Ok(self.offset())
    }

    fn unflushed_bytes(&self) -> u64 {
        0
    }

    fn rotation_state(&self) -> RotationState {
        self.state.clone()
    }
}

impl ScriptedRotation {
    fn offset(&self) -> WalOffset {
        WalOffset {
            segment: uuid::Uuid::nil(),
            byte: self.byte,
        }
    }
}

fn fault(site: RotationSite, attempts: u32) -> RotationFault {
    RotationFault::new(site, &std::io::Error::other("injected"), attempts, 3)
}

/// Retrying, a second failed attempt (no edge), recovered, retrying
/// again, terminal, and appends past terminal (no edge, no leave).
async fn drive_rotation_edges() {
    let script = VecDeque::from([
        RotationState::Retrying(fault(RotationSite::Create, 1)),
        RotationState::Retrying(fault(RotationSite::Create, 2)),
        RotationState::Healthy,
        RotationState::Retrying(fault(RotationSite::Rename, 1)),
        RotationState::Terminal(fault(RotationSite::Rename, 3)),
        RotationState::Terminal(fault(RotationSite::Rename, 3)),
        RotationState::Terminal(fault(RotationSite::Rename, 3)),
    ]);
    let commits = CommitCoordinator::new(
        Box::new(ScriptedRotation {
            script,
            state: RotationState::Healthy,
            byte: 0,
        }),
        Duration::from_millis(5),
        u64::MAX,
    );
    for _ in 0..7 {
        let outcome = commits.commit(b"frame").await;
        assert!(outcome.result.is_ok(), "the scripted journal acks");
    }
}

/// Pinned (once a checkpoint lets a pass plan, a tenant with frames and
/// no snapshot), lifted (a cut installs its snapshot), and the latch set
/// — each followed by a tick that must not repeat the edge.
async fn drive_floor_and_latch_edges(rig: &Arc<BarrierRig>, housekeeper: &Arc<Housekeeper>) {
    rig.ingest("checkout", &["user 1 logged in"]).await;
    assert_eq!(cut(rig, false).await, CutOutcome::Stamped);
    rig.ingest("billing", &["invoice 7 sent"]).await;
    tick(housekeeper).await;
    tick(housekeeper).await;
    assert_eq!(cut(rig, false).await, CutOutcome::Stamped);
    tick(housekeeper).await;
    tick(housekeeper).await;
    rig.epochs.report(rig.epochs.current());
    tick(housekeeper).await;
    tick(housekeeper).await;
}

/// The unreclaimed bytes still grow with no checkpoint: the figure is
/// every unreclaimed frame, not the below-checkpoint one, which would read
/// flat through exactly this outage.
fn assert_unreclaimed_grows(first: &[ResourceMetrics], second: &[ResourceMetrics]) {
    let (Value::I(before), Value::I(after)) = (
        value(first, semconv::OURIOS_WAL_UNRECLAIMED_SIZE),
        value(second, semconv::OURIOS_WAL_UNRECLAIMED_SIZE),
    ) else {
        panic!("ourios.wal.unreclaimed.size is an i64 UpDownCounter");
    };
    assert!(
        after > before && before > 0,
        "unreclaimed bytes grow with no checkpoint: {before} -> {after}"
    );
    let Value::F(age) = value(second, semconv::OURIOS_WAL_UNRECLAIMED_AGE) else {
        panic!("ourios.wal.unreclaimed.age is an f64 gauge");
    };
    assert!(age >= 0.0, "the oldest unreclaimed frame has an age");
}

/// The state metrics report every member, 1 for the current state and 0
/// for the others, and the latch is the failed epoch beside the barrier
/// epoch.
fn assert_state_metrics(rms: &[ResourceMetrics]) {
    assert_eq!(
        states(
            rms,
            semconv::OURIOS_WAL_RETAIN_FLOOR_STATUS,
            semconv::OURIOS_WAL_RETAIN_FLOOR_STATE
        ),
        BTreeMap::from([
            ("min".to_owned(), 1),
            ("none".to_owned(), 0),
            ("pinned".to_owned(), 0),
            ("unknown".to_owned(), 0),
        ]),
        "the cut installed the tenant's snapshot, so the floor is its minimum",
    );
    assert_eq!(
        states(
            rms,
            semconv::OURIOS_WAL_ROTATION_STATUS,
            semconv::OURIOS_WAL_ROTATION_STATE
        ),
        BTreeMap::from([
            ("healthy".to_owned(), 1),
            ("retrying".to_owned(), 0),
            ("terminal".to_owned(), 0),
        ]),
    );

    // The latch is exported as the failed epoch beside the barrier epoch.
    let (Value::U(failed), Value::U(epoch)) = (
        value(rms, semconv::OURIOS_INGEST_BARRIER_FAILED_EPOCH),
        value(rms, semconv::OURIOS_INGEST_BARRIER_EPOCH),
    ) else {
        panic!("the epoch gauges are u64");
    };
    assert!(
        failed <= epoch,
        "the failed epoch {failed} is at or below {epoch}"
    );
}

/// Scenario RFC0052.7 — every registry name is in the exported stream.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0052_7_every_wal_instrument_is_exported_under_its_registry_name() {
    let _serial = serial().await;
    let harness = harness();
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = Arc::new(BarrierRig::new(tmp.path()));
    let housekeeper = housekeeper(&rig);

    // Given a running node whose checkpoint never advances: frames land,
    // housekeeping passes, and no cut ever stamps.
    rig.ingest("checkout", &["user 1 logged in"]).await;
    tick(&housekeeper).await;
    let first = harness.collect();
    rig.ingest("checkout", &["user 2 logged in", "user 3 logged in"])
        .await;
    tick(&housekeeper).await;
    let second = harness.collect();

    assert_eq!(rig.commits.last_checkpoint(), None, "nothing stamped");
    assert_unreclaimed_grows(&first, &second);

    // When a cut stamps and the latch is set, and housekeeping passes.
    assert_eq!(cut(&rig, false).await, CutOutcome::Stamped);
    rig.epochs.report(rig.epochs.current());
    tick(&housekeeper).await;
    let rms = harness.collect();

    // Then every WAL and barrier instrument is under its registry name.
    for name in INSTRUMENTS {
        assert!(
            !points(&rms, name).is_empty(),
            "{name} is missing from the exported stream"
        );
    }

    assert_state_metrics(&rms);

    assert_one_cut_counted(&second, &rms);
}

/// Scenario RFC0052.7 — each transition emits its named event exactly once.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0052_7_each_transition_emits_its_registry_event_once() {
    let _serial = serial().await;
    let harness = harness();

    // When the rotation state is driven through the append path.
    drive_rotation_edges().await;
    let rotation: Vec<Event> = harness
        .events()
        .into_iter()
        .filter(|e| e.name.starts_with("ourios.receiver.wal.rotation."))
        .collect();

    // Then each edge is one event with its state, and entering terminal
    // is one event with no leave event after it.
    let observed: Vec<(&str, Option<&str>)> =
        rotation.iter().map(|e| (e.name, e.error_type())).collect();
    assert_eq!(
        observed,
        [
            (
                semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_RETRYING,
                Some("create")
            ),
            (semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_RECOVERED, None),
            (
                semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_RETRYING,
                Some("rename")
            ),
            (
                semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_TERMINAL,
                Some("rename")
            ),
        ],
    );

    // When the floor is pinned and lifted, and the latch is set.
    harness.logs.reset();
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = Arc::new(BarrierRig::new(tmp.path()));
    let housekeeper = housekeeper(&rig);
    drive_floor_and_latch_edges(&rig, &housekeeper).await;

    // Then each emits exactly one event, in the order it happened.
    let names: Vec<&str> = harness
        .events()
        .into_iter()
        .map(|e| e.name)
        .filter(|name| {
            [
                semconv::EVENT_OURIOS_RECEIVER_WAL_RETAIN_FLOOR_PINNED,
                semconv::EVENT_OURIOS_RECEIVER_WAL_RETAIN_FLOOR_LIFTED,
                semconv::EVENT_OURIOS_RECEIVER_BARRIER_LATCHED,
            ]
            .contains(name)
        })
        .collect();
    assert_eq!(
        names,
        [
            semconv::EVENT_OURIOS_RECEIVER_WAL_RETAIN_FLOOR_PINNED,
            semconv::EVENT_OURIOS_RECEIVER_WAL_RETAIN_FLOOR_LIFTED,
            semconv::EVENT_OURIOS_RECEIVER_BARRIER_LATCHED,
        ],
    );
}

/// Every RFC 0052 event, emitted: the rotation, floor and latch edges,
/// a failed idle rotation and a failed checkpoint write.
async fn emit_every_event() {
    drive_rotation_edges().await;

    let tmp = tempfile::TempDir::new().expect("temp");
    let faults = Arc::new(JournalFaults::default());
    let rig = rig_with(tmp.path(), &faults);
    let housekeeper = housekeeper(&rig);
    drive_floor_and_latch_edges(&rig, &housekeeper).await;

    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = rig_with(tmp.path(), &faults);
    rig.ingest("checkout", &["user 1 logged in"]).await;
    faults.fail_idle_rotation.store(true, Ordering::Release);
    rig.sabotage_checkpoint();
    assert_eq!(
        cut(&rig, true).await,
        CutOutcome::Stamped,
        "a failed sidecar write keeps the previous mark usable (RFC0052.1)"
    );
}

/// The live-check leg's view of what was emitted: every event is a
/// registry name, and every attribute a registry attribute. A tracing
/// field left over from before the naming (`error = %e`) fails here as
/// weaver would fail it.
fn weaver_samples(events: &[Event]) -> serde_json::Value {
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

/// Scenario RFC0052.7 — `weaver registry live-check` sees every new event emitted.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
///
/// With `OURIOS_LIVE_CHECK_WEAVER` (the weaver binary) and
/// `OURIOS_LIVE_CHECK_REGISTRY` (the pinned registry's `registry/`
/// directory) set — CI's `live-check` job sets both — the emitted
/// records are handed to `weaver registry live-check` and any
/// `violation` fails the leg. Without them the leg still checks every
/// emitted name and attribute against the generated registry constants,
/// and says that weaver did not run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0052_7_live_check_covers_every_new_log_event() {
    let _serial = serial().await;
    let harness = harness();
    emit_every_event().await;
    let events: Vec<Event> = harness
        .events()
        .into_iter()
        .filter(|e| RFC0052_EVENTS.contains(&e.name))
        .collect();

    for name in RFC0052_EVENTS {
        assert!(
            events.iter().any(|e| e.name == name),
            "{name} was never emitted, so the live-check would never check it"
        );
    }
    for event in &events {
        for key in event.attributes.keys() {
            assert_eq!(
                key, "error.type",
                "{}: `{key}` is not a registry attribute",
                event.name
            );
        }
    }

    let (Some(weaver), Some(registry)) = (
        std::env::var_os("OURIOS_LIVE_CHECK_WEAVER"),
        std::env::var_os("OURIOS_LIVE_CHECK_REGISTRY"),
    ) else {
        eprintln!(
            "RFC0052.7: weaver not configured (OURIOS_LIVE_CHECK_WEAVER / \
             OURIOS_LIVE_CHECK_REGISTRY); checked names and attributes against the \
             generated constants only"
        );
        return;
    };
    let dir = tempfile::TempDir::new().expect("temp");
    let samples = dir.path().join("samples.json");
    std::fs::write(
        &samples,
        serde_json::to_vec(&weaver_samples(&events)).expect("serialise"),
    )
    .expect("write samples");
    let report_dir = dir.path().join("report");
    let status = std::process::Command::new(weaver)
        .args(["registry", "live-check", "--future", "-r"])
        .arg(&registry)
        .args(["--input-source"])
        .arg(&samples)
        .args(["--input-format", "json", "--format", "json", "--no-stream"])
        .arg("--output")
        .arg(&report_dir)
        .status()
        .expect("run weaver");
    let report: serde_json::Value = serde_json::from_slice(
        &std::fs::read(report_dir.join("live_check.json")).expect("the live-check report"),
    )
    .expect("a JSON report");
    let mut violations = Vec::new();
    collect_violations(&report, &mut violations);
    assert!(
        violations.is_empty(),
        "weaver live-check (exit {status}) found violations: {violations:#?}"
    );
    let seen: Vec<&str> = report["samples"]
        .as_array()
        .expect("the report lists its samples")
        .iter()
        .filter_map(|s| s["log"]["event_name"].as_str())
        .collect();
    for name in RFC0052_EVENTS {
        assert!(seen.contains(&name), "weaver never saw {name}");
    }
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
