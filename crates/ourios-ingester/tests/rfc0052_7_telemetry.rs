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

use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use ourios_ingester::barrier::CutOutcome;
use ourios_ingester::cadence;
use ourios_ingester::housekeeping::{Housekeeper, HousekeepingTick};
use ourios_ingester::receiver::{CommitCoordinator, Journal, ReceiveError};
use ourios_semconv as semconv;
use ourios_telemetry::TelemetryGuard;
use ourios_telemetry::live_check::{self, Checked, Event, EventCapture};
use ourios_wal::{RotationFault, RotationSite, RotationState, WalOffset};

use crate::rfc0052_barrier_support::{BarrierRig, JournalFaults, RigSpec, wal_config};

const CAP: usize = 128;

/// The RFC 0052 log events, every one of which the live-check leg must
/// see emitted.
const RFC0052_EVENTS: [&str; 11] = [
    semconv::EVENT_OURIOS_RECEIVER_WAL_CHECKPOINT_ERROR,
    semconv::EVENT_OURIOS_RECEIVER_WAL_IDLE_ROTATION_ERROR,
    semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_RETRYING,
    semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_RECOVERED,
    semconv::EVENT_OURIOS_RECEIVER_WAL_ROTATION_TERMINAL,
    semconv::EVENT_OURIOS_RECEIVER_WAL_RETAIN_FLOOR_PINNED,
    semconv::EVENT_OURIOS_RECEIVER_WAL_RETAIN_FLOOR_LIFTED,
    semconv::EVENT_OURIOS_RECEIVER_BARRIER_LATCHED,
    semconv::EVENT_OURIOS_RECEIVER_WAL_HOUSEKEEPING_ERROR,
    semconv::EVENT_OURIOS_RECEIVER_CADENCE_JOIN_ERROR,
    semconv::EVENT_OURIOS_RECEIVER_PUBLISH_HELD,
];

/// The only attributes those events declare.
const EVENT_ATTRIBUTES: [&str; 2] = ["error.type", semconv::OURIOS_SINK_FLUSH_TRIGGER];

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
    events: &'static EventCapture,
}

fn harness() -> &'static Harness {
    static HARNESS: OnceLock<Harness> = OnceLock::new();
    HARNESS.get_or_init(|| {
        let (metrics_guard, metrics) = ourios_telemetry::init_in_memory("ourios-rfc0052-7");
        Harness {
            metrics_guard,
            metrics,
            events: live_check::event_capture().expect("the only subscriber this binary installs"),
        }
    })
}

/// One leg at a time: every leg resets and reads the shared exporters.
async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let guard = SERIAL.lock().await;
    harness().events.reset();
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
        self.events.events()
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

fn housekeeper_of(rig: &BarrierRig) -> Arc<Housekeeper> {
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
    let housekeeper = housekeeper_of(&rig);

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
    harness.events.reset();
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = Arc::new(BarrierRig::new(tmp.path()));
    let housekeeper = housekeeper_of(&rig);
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
    let housekeeper = housekeeper_of(&rig);
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

    // A housekeeping pass that unwinds after the WAL took its plan.
    faults.panic_after_prepare.store(true, Ordering::Release);
    assert!(matches!(
        tick(&housekeeper_of(&rig)).await,
        HousekeepingTick::Panicked
    ));

    // A cadence task whose join is a panic, read at shutdown.
    let joined = tokio::spawn(async { panic!("injected cadence-task panic") })
        .await
        .map(drop);
    assert!(cadence::read_join(&rig.epochs, "barrier", joined));

    // A later drain held behind an earlier one whose template events are
    // still unwritten.
    rig.ingest("checkout", &["cache warmed in 5 ms"]).await;
    let earlier = rig.publish.drain_all();
    rig.ingest("checkout", &["cache warmed in 5 ms"]).await;
    let later = rig.publish.drain_all();
    assert!(
        !rig.publish.write_ordered(later, "age"),
        "the later drain is held"
    );
    assert!(rig.publish.write_ordered(earlier, "age"));
}

/// Scenario RFC0052.7 — `weaver registry live-check` sees every new event emitted.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
///
/// Every emitted RFC 0052 event is checked against its registry name and
/// the attributes the registry declares for it — so a tracing field left
/// over from before the naming (`error = %e`) fails here as weaver would
/// fail it — and, where CI's `live-check` job configures weaver
/// (`OURIOS_LIVE_CHECK_WEAVER`, `OURIOS_LIVE_CHECK_REGISTRY`), through
/// `weaver registry live-check` itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0052_7_live_check_covers_every_new_log_event() {
    let _serial = serial().await;
    emit_every_event().await;
    let events = harness().events();
    let checked = live_check::live_check(&events, &RFC0052_EVENTS, &EVENT_ATTRIBUTES)
        .expect("every RFC 0052 event is emitted and registry-conformant");

    // The failure events carry the error.type values the registry lists.
    for (name, class) in [
        (semconv::EVENT_OURIOS_RECEIVER_WAL_IDLE_ROTATION_ERROR, "io"),
        (semconv::EVENT_OURIOS_RECEIVER_WAL_CHECKPOINT_ERROR, "io"),
        (
            semconv::EVENT_OURIOS_RECEIVER_WAL_HOUSEKEEPING_ERROR,
            "cadence_panic",
        ),
        (semconv::EVENT_OURIOS_RECEIVER_CADENCE_JOIN_ERROR, "panic"),
    ] {
        assert!(
            events
                .iter()
                .any(|e| e.name == name && e.error_type() == Some(class)),
            "{name} with error.type {class}"
        );
    }
    if checked == Checked::NamesOnly {
        eprintln!("RFC0052.7: weaver is not configured here; checked names and attributes only");
    }
}
