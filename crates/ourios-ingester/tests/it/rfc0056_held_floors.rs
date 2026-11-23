//! RFC0056.4–.7 and .9 — the checkpoint records where each held tenant
//! stands, and recovery gates it there. See
//! `docs/rfcs/0056-audit-durability.md` §3.4 and §5.
//!
//! A node here is the barrier rig on the local backend; a restart drops
//! it and builds a rig over the same roots that recovers first, as
//! `serve` does, then publishes what recovery replayed. A restart with
//! nothing held is the repaired fault. The rig holds tenants the way the
//! receiver does; the tests hold one directly, since how a hold starts
//! is RFC0056.1's.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use ourios_config::MinerConfig;
use ourios_core::audit::{AuditEvent, AuditPayload, AuditSink};
use ourios_core::record::RecordSink;
use ourios_core::tenant::TenantId;
use ourios_ingester::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use ourios_ingester::barrier::CutOutcome;
use ourios_ingester::housekeeping::{Housekeeper, HousekeepingTick};
use ourios_ingester::publish::PublishCoordinator;
use ourios_ingester::receiver::tenant::assign;
use ourios_ingester::record_sink::{ParquetRecordSink, SharedParquetSink};
use ourios_ingester::recovery::{self, RecoveryDriverError, RecoveryReport};
use ourios_ingester::snapshot_store;
use ourios_ingester::template_ids::TemplateIds;
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::Store;
use ourios_wal::{FrameKind, HeldFloor, HeldFloors, TenantBatch, Wal, WalConfig, WalOffset};
use prost::Message;

use crate::ingest_support::{request, resource_logs};
use crate::rfc0052_barrier_support::{
    BarrierRig, RigSpec, Startup, audit_events, never_flush, rows, wal_config,
};

const A: &str = "acme";
const B: &str = "globex";

fn spec(tmp: &Path, recover: bool) -> RigSpec {
    RigSpec {
        holding: true,
        startup: recover.then(|| recovery(tmp)),
        ..RigSpec::new(WalConfig {
            segment_age_secs: 1,
            ..wal_config(&tmp.join("wal"))
        })
    }
}

/// Startup recovery over the node's roots, as `serve` runs it before the
/// pipeline exists.
fn recovery(tmp: &Path) -> Box<Startup> {
    let (snapshots, audit) = (tmp.join("wal").join("snapshots"), tmp.join("audit"));
    Box::new(move |wal, miner, _| {
        let ids = TemplateIds::new(Store::local(&audit).expect("audit store"))
            .with_bootstrap_allowed(true);
        recovery::recover(wal, &snapshots, miner, &ids)
            .expect("recover")
            .accepted_horizons()
    })
}

fn node(tmp: &Path) -> BarrierRig {
    BarrierRig::build(tmp, spec(tmp, false))
}

/// Kill `rig` and start a node over its roots, with the fault repaired.
fn restart(rig: BarrierRig, tmp: &Path) -> BarrierRig {
    drop(rig);
    recovered_node(tmp)
}

/// A node over `tmp`'s roots that recovers first and publishes what
/// recovery replayed.
fn recovered_node(tmp: &Path) -> BarrierRig {
    let rig = BarrierRig::build(tmp, spec(tmp, true));
    assert!(
        rig.publish
            .write_ordered(rig.publish.drain_all(), "shutdown"),
        "what recovery replayed publishes"
    );
    rig
}

fn hold(rig: &BarrierRig, tenant: &str) {
    assert!(rig.audit.held().hold(&TenantId::new(tenant)));
}

/// Ingest one record of `tenant` stamped `n` and return its frame.
async fn ingest_at(rig: &BarrierRig, tenant: &str, n: u64, body: &str) -> WalOffset {
    ingest_each(rig, tenant, n, &[body]).await
}

/// Ingest one frame of `tenant`'s records, stamped `first` onward, and
/// return it.
async fn ingest_each(rig: &BarrierRig, tenant: &str, first: u64, bodies: &[&str]) -> WalOffset {
    ingest_observed(rig, tenant, (first, None), bodies).await
}

/// `tenant`'s records of `bodies`, stamped `first` onward and observed at
/// `observed` when given.
fn stamped(
    tenant: &str,
    (first, observed): (u64, Option<u64>),
    bodies: &[&str],
) -> opentelemetry_proto::tonic::logs::v1::ResourceLogs {
    let mut logs = resource_logs(tenant, bodies);
    for (n, record) in (first..).zip(&mut logs.scope_logs[0].log_records) {
        record.time_unix_nano = n;
        if let Some(observed) = observed {
            record.observed_time_unix_nano = observed;
        }
    }
    logs
}

/// [`ingest_each`], each record also observed at `observed` when given.
async fn ingest_observed(
    rig: &BarrierRig,
    tenant: &str,
    (first, observed): (u64, Option<u64>),
    bodies: &[&str],
) -> WalOffset {
    let logs = stamped(tenant, (first, observed), bodies);
    rig.pipeline
        .ingest(request(vec![logs]), TenantId::new(tenant))
        .await
        .expect("the batch acks");
    rig.pipeline.last_durable().expect("a durable mark")
}

fn cut(rig: &BarrierRig) {
    assert_eq!(rig.barrier.tick(&rig.pipeline, false), CutOutcome::Stamped);
}

/// Let the open segment age out, then cut, sealing it.
async fn seal_and_cut(rig: &BarrierRig) {
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(rig.barrier.tick(&rig.pipeline, true), CutOutcome::Stamped);
}

/// The stamps of `tenant`'s published rows, sorted.
fn stamps(rig: &BarrierRig, tenant: &str) -> Vec<u64> {
    let mut stamps: Vec<u64> = rows(&rig.data_root)
        .iter()
        .filter(|r| r.tenant_id.as_str() == tenant)
        .map(|r| r.time_unix_nano)
        .collect();
    stamps.sort_unstable();
    stamps
}

/// Every published row of `tenant` has a template event in the store.
fn described(rig: &BarrierRig, tenant: &str) -> bool {
    let events: BTreeSet<u64> = audit_events(&rig.audit_root)
        .iter()
        .filter(|e| e.tenant_id.as_str() == tenant)
        .filter_map(|e| match e.payload {
            AuditPayload::Template { template_id, .. } => Some(template_id),
            _ => None,
        })
        .collect();
    rows(&rig.data_root)
        .iter()
        .filter(|r| r.tenant_id.as_str() == tenant)
        .all(|r| events.contains(&r.template_id))
}

/// The most bytes the record sink buffers for any one of `tenant`'s
/// published rows.
fn largest_record(rig: &BarrierRig, tenant: &str) -> usize {
    let scratch = tempfile::TempDir::new().expect("temp");
    rows(&rig.data_root)
        .into_iter()
        .filter(|r| r.tenant_id.as_str() == tenant)
        .map(|row| {
            let sink = SharedParquetSink::new(ParquetRecordSink::new(
                Store::local(scratch.path()).expect("scratch store"),
                never_flush(),
            ));
            sink.clone().emit(row);
            sink.buffered_bytes()
        })
        .max()
        .unwrap_or(0)
}

fn events_of(rig: &BarrierRig, tenant: &str) -> usize {
    audit_events(&rig.audit_root)
        .iter()
        .filter(|e| e.tenant_id.as_str() == tenant)
        .count()
}

/// The horizon `tenant`'s installed artefact carries.
fn disk_horizon(rig: &BarrierRig, tenant: &str) -> Option<WalOffset> {
    let bytes = std::fs::read(rig.snapshots_root.join(format!("{tenant}.snap"))).ok()?;
    let (state, _) = ourios_miner::snapshot::recover(Some(&bytes));
    snapshot_store::offset_of(state.expect("decodes").wal_high_water.as_ref()?)
}

fn floors(entries: &[(&str, HeldFloor)]) -> HeldFloors {
    entries
        .iter()
        .map(|(tenant, floor)| (TenantId::new(*tenant), *floor))
        .collect()
}

fn segments(rig: &BarrierRig) -> BTreeSet<uuid::Uuid> {
    std::fs::read_dir(&rig.wal_root)
        .expect("wal root")
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.strip_suffix(".wal")?.parse().ok()
        })
        .collect()
}

fn housekeeping_pass(rig: &BarrierRig) -> ourios_wal::HousekeepingProgress {
    let housekeeper = Housekeeper::new(
        Arc::clone(&rig.commits),
        Arc::clone(&rig.barrier),
        rig.publish.clone(),
        usize::try_from(ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS).expect("the cap fits"),
    );
    let HousekeepingTick::Completed(pass) = housekeeper.tick() else {
        panic!("the pass runs");
    };
    pass
}

/// The marks of [`held_after_x0`]'s node.
struct Held {
    /// A's frame at or below `X0`, where its snapshot stands.
    a1: WalOffset,
    x0: WalOffset,
    /// A's frame above `X0`, in a segment the cuts sealed.
    a2: WalOffset,
    x1: WalOffset,
}

/// RFC0056.5's node: A held after `X0`, then cuts stamping `X1` across
/// three sealed segments — both tenants at or below `X0`, both above it,
/// and B alone.
async fn held_after_x0(tmp: &Path) -> (BarrierRig, Held) {
    let rig = node(tmp);
    let a1 = ingest_at(&rig, A, 1, "user 1 logged in").await;
    let x0 = ingest_at(&rig, B, 11, "user 11 logged in").await;
    seal_and_cut(&rig).await;
    assert_eq!(rig.commits.last_checkpoint(), Some(x0));

    hold(&rig, A);
    let a2 = ingest_at(&rig, A, 2, "order 2 shipped").await;
    ingest_at(&rig, B, 12, "user 12 logged in").await;
    seal_and_cut(&rig).await;
    let x1 = ingest_at(&rig, B, 13, "user 13 logged in").await;
    seal_and_cut(&rig).await;
    (rig, Held { a1, x0, a2, x1 })
}

/// Scenario RFC0056.4 — a snapshot installed by a cut that did not stamp.
/// See `docs/rfcs/0056-audit-durability.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0056_4_a_snapshot_installed_by_a_cut_that_did_not_stamp() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = node(tmp.path());
    ingest_at(&rig, A, 1, "user 1 logged in").await;
    let x0 = ingest_at(&rig, B, 11, "user 11 logged in").await;
    cut(&rig);
    let checkpoint = std::fs::read(rig.wal_root.join("CHECKPOINT")).expect("CHECKPOINT");
    rig.sabotage_checkpoint();
    let s = ingest_at(&rig, A, 2, "order 2 shipped").await;
    cut(&rig);
    assert_eq!(rig.commits.last_checkpoint(), Some(x0), "the stamp failed");
    assert_eq!(disk_horizon(&rig, A), Some(s), "the install landed");

    hold(&rig, A);
    std::fs::remove_dir_all(rig.wal_root.join("CHECKPOINT")).expect("undo the sabotage");
    std::fs::write(rig.wal_root.join("CHECKPOINT"), checkpoint).expect("the X0 sidecar");
    ingest_at(&rig, A, 3, "cart 3 emptied").await;
    let x1 = ingest_at(&rig, B, 12, "user 12 logged in").await;
    cut(&rig);
    assert_eq!(rig.commits.last_checkpoint(), Some(x1));
    assert_eq!(
        rig.commits.held_floors(),
        floors(&[(A, HeldFloor::At(s))]),
        "A's floor is its installed snapshot, not the earlier stamp"
    );
    assert_eq!(stamps(&rig, A), [1, 2]);

    let rig = restart(rig, tmp.path());
    assert_eq!(
        stamps(&rig, A),
        [1, 2, 3],
        "nothing of A's at or below S publishes again; above it does"
    );
    assert!(described(&rig, A), "under regenerated events");
}

/// Scenario RFC0056.5 — the checkpoint advances and records the floor.
/// See `docs/rfcs/0056-audit-durability.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0056_5_the_checkpoint_advances_and_records_the_floor() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (rig, held) = held_after_x0(tmp.path()).await;

    assert_eq!(rig.commits.last_checkpoint(), Some(held.x1));
    assert_eq!(
        rig.commits.held_floors(),
        floors(&[(A, HeldFloor::At(held.x0))])
    );
    assert_eq!(
        disk_horizon(&rig, A),
        Some(held.a1),
        "A's snapshot is not installed past X0"
    );

    let pass = housekeeping_pass(&rig);
    assert_eq!(
        pass.removed_segments, 2,
        "the segment at or below X0 and B's alone go: {pass:?}"
    );
    assert!(
        segments(&rig).contains(&held.a2.segment),
        "the segment holding A's frame above X0 stays"
    );
}

/// Scenario RFC0056.6 — a restart after repair publishes the held records
/// under their events. See `docs/rfcs/0056-audit-durability.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0056_6_a_restart_after_repair_publishes_the_held_records_under_their_events() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (rig, _) = held_after_x0(tmp.path()).await;
    assert_eq!(stamps(&rig, A), [1]);
    let b_events = events_of(&rig, B);

    let rig = restart(rig, tmp.path());
    assert_eq!(stamps(&rig, A), [1, 2], "A's record above X0 publishes");
    assert!(described(&rig, A), "after events that describe it");
    assert_eq!(stamps(&rig, B), [11, 12, 13], "B's publish nothing again");
    assert_eq!(events_of(&rig, B), b_events, "nor do B's events");

    ingest_at(&rig, B, 14, "user 14 logged in").await;
    cut(&rig);
    assert!(
        rig.commits.held_floors().is_empty(),
        "the next cut stamps with no floor for A"
    );
}

/// Scenario RFC0056.7 — a tenant held before the first checkpoint.
/// See `docs/rfcs/0056-audit-durability.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0056_7_a_tenant_held_before_the_first_checkpoint() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = node(tmp.path());
    hold(&rig, A);
    ingest_at(&rig, A, 1, "user 1 logged in").await;
    let x1 = ingest_at(&rig, B, 11, "user 11 logged in").await;
    seal_and_cut(&rig).await;

    assert_eq!(rig.commits.last_checkpoint(), Some(x1));
    assert_eq!(
        rig.commits.held_floors(),
        floors(&[(A, HeldFloor::StartOfLog)])
    );
    assert_eq!(disk_horizon(&rig, A), None, "no snapshot of A");
    let pass = housekeeping_pass(&rig);
    assert_eq!(pass.removed_segments, 0, "A's segment stays: {pass:?}");

    let rig = restart(rig, tmp.path());
    assert_eq!(stamps(&rig, A), [1], "all of A's records publish");
    assert!(described(&rig, A), "with their template events");
    assert_eq!(stamps(&rig, B), [11], "none of B's through X1 again");
}

/// Scenario RFC0056.9 — a snapshot past the floor after a repaired
/// restart. See `docs/rfcs/0056-audit-durability.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0056_9_a_snapshot_past_the_floor_after_a_repaired_restart() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (rig, held) = held_after_x0(tmp.path()).await;
    let rig = restart(rig, tmp.path());
    let checkpoint = std::fs::read(rig.wal_root.join("CHECKPOINT")).expect("CHECKPOINT");
    rig.sabotage_checkpoint();
    let s = ingest_at(&rig, A, 4, "user 4 logged in").await;
    cut(&rig);
    assert_eq!(disk_horizon(&rig, A), Some(s), "A installed past X0");
    assert!(s > held.x0);
    ingest_at(&rig, A, 5, "order 5 shipped").await;
    let path = rig.wal_root.join("CHECKPOINT");
    drop(rig);
    std::fs::remove_dir_all(&path).expect("undo the sabotage");
    std::fs::write(&path, checkpoint).expect("the sidecar with A's floor");
    assert_eq!(
        ourios_wal::Wal::open(wal_config(&tmp.path().join("wal")))
            .expect("reopen")
            .held_floors(),
        &floors(&[(A, HeldFloor::At(held.x0))]),
        "the failed stamp left the floor"
    );

    let rig = recovered_node(tmp.path());
    assert_eq!(
        stamps(&rig, A),
        [1, 2, 4, 5],
        "A routes by S: nothing at or below it again, above it publishes"
    );
    ingest_at(&rig, B, 14, "user 14 logged in").await;
    cut(&rig);
    assert!(
        rig.commits.held_floors().is_empty(),
        "the next stamp writes no floor for A"
    );
}

/// A's template events stamped before the epoch, on their way to the
/// audit sink: the fault a restart does not repair, since no write of
/// them ever succeeds.
struct PreEpochFor(Box<dyn AuditSink>);

impl AuditSink for PreEpochFor {
    fn emit(&mut self, mut event: AuditEvent) {
        if event.tenant_id.as_str() == A && matches!(event.payload, AuditPayload::Template { .. }) {
            event.timestamp = UNIX_EPOCH - Duration::from_secs(1);
        }
        self.0.emit(event);
    }
}

/// How [`unrepaired_recovery`] runs: whether shutdown is asked for on the
/// first of B's failed writes, and where its outcome lands.
struct Paced {
    blocked: Blocked,
    stop_on_failure: bool,
    outcome: Arc<Mutex<Option<Result<RecoveryReport, RecoveryDriverError>>>>,
}

/// Recovery in passes bounded at one buffered byte and one buffered
/// event, with A's fault still present and B's audit writes blocked until
/// one has failed.
fn unrepaired_recovery(tmp: &Path, paced: Paced) -> Box<Startup> {
    let (snapshots, audit) = (tmp.join("wal").join("snapshots"), tmp.join("audit"));
    Box::new(move |wal, miner, publish| {
        let Paced {
            blocked,
            stop_on_failure,
            outcome,
        } = paced;
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let unblocked = blocked.lift_after_a_failure(publish.audit().clone(), move || {
            stop.store(stop_on_failure, Ordering::Release);
        });
        let real = miner.replace_audit_sink(Box::new(ourios_core::audit::NoOpAuditSink));
        let _ = miner.replace_audit_sink(Box::new(PreEpochFor(real)));
        let ids = TemplateIds::new(Store::local(&audit).expect("audit store"))
            .with_bootstrap_allowed(true);
        let passes = recovery::Passes {
            publish,
            record_bytes: 1,
            audit_events: 1,
            stopping: &stopping,
        };
        let recovered = passes.recover((wal, miner), &snapshots, &ids);
        unblocked.join().expect("B's writes recovered");
        let horizons = recovered
            .as_ref()
            .map(RecoveryReport::accepted_horizons)
            .unwrap_or_default();
        *outcome.lock().expect("outcome") = Some(recovered);
        horizons
    })
}

/// B's audit partition, refusing every write: a file sits where its
/// directory belongs, so each write fails transiently.
struct Blocked {
    partition: std::path::PathBuf,
    aside: std::path::PathBuf,
}

impl Blocked {
    fn b_audit_writes(tmp: &Path) -> Self {
        let partition = tmp
            .join("audit")
            .join("audit")
            .join(format!("tenant_id={B}"));
        let aside = partition.with_extension("aside");
        std::fs::rename(&partition, &aside).expect("move B's partition aside");
        std::fs::write(&partition, b"not a directory").expect("block it");
        Self { partition, aside }
    }

    /// Lift the block once `audit` has seen a write fail transiently, so
    /// recovery has met the failure before it clears; `first` runs before
    /// the lift.
    fn lift_after_a_failure(
        self,
        audit: SharedParquetAuditSink,
        first: impl FnOnce() + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while audit.flush_errors_transient() == 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "no write of B's failed"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            first();
            std::fs::remove_file(&self.partition).expect("unblock");
            std::fs::rename(&self.aside, &self.partition).expect("put B's partition back");
        })
    }
}

/// RFC0056.5's node with an unpublished tail of B's in one frame, killed.
async fn held_with_a_tail_of_b(tmp: &Path) -> Held {
    let (rig, held) = held_after_x0(tmp).await;
    ingest_each(
        &rig,
        B,
        14,
        &["job 14 queued", "cache warmed in 15 ms", "disk 16 is full"],
    )
    .await;
    drop(rig);
    held
}

/// Restart over `tmp` with A's fault unrepaired and B's audit writes
/// blocked until one fails, shutdown asked for then when `stop`.
fn restart_unrepaired(
    tmp: &Path,
    stop: bool,
) -> (BarrierRig, Result<RecoveryReport, RecoveryDriverError>) {
    let outcome = Arc::new(Mutex::new(None));
    let paced = Paced {
        blocked: Blocked::b_audit_writes(tmp),
        stop_on_failure: stop,
        outcome: Arc::clone(&outcome),
    };
    let rig = BarrierRig::build(
        tmp,
        RigSpec {
            startup: Some(unrepaired_recovery(tmp, paced)),
            ..spec(tmp, false)
        },
    );
    let recovered = outcome
        .lock()
        .expect("outcome")
        .take()
        .expect("recovery ran");
    (rig, recovered)
}

/// RFC 0056 §3.2: shutdown asked for while a pass waits stops recovery,
/// even when the store has recovered by the next retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shutdown_while_a_pass_waits_stops_recovery() {
    let tmp = tempfile::TempDir::new().expect("temp");
    held_with_a_tail_of_b(tmp.path()).await;
    let (_, recovered) = restart_unrepaired(tmp.path(), true);
    assert!(
        matches!(recovered, Err(RecoveryDriverError::Stopped)),
        "{recovered:?}"
    );
}

/// Scenario RFC0056.10 — a restart into an unrepaired fault holds the
/// tenant again. See `docs/rfcs/0056-audit-durability.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0056_10_a_restart_into_an_unrepaired_fault_holds_the_tenant_again() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let held = held_with_a_tail_of_b(tmp.path()).await;
    let (rig, recovered) = restart_unrepaired(tmp.path(), false);
    let _ = rig
        .publish
        .write_ordered(rig.publish.drain_all(), "shutdown");
    let passes = recovered.expect("recovered").passes;

    assert!(
        rig.audit.held().is_held(&TenantId::new(A)),
        "A is held again"
    );
    assert!(
        passes.peak_audit_events <= 1 && passes.peak_record_bytes <= largest_record(&rig, B),
        "neither sink holds more than the one item just handed to it: {passes:?}"
    );
    assert!(
        passes.waits > 0,
        "B's failing writes stop the reading: {passes:?}"
    );
    assert_eq!(
        stamps(&rig, A),
        [1],
        "nothing of A's publishes without its events"
    );
    assert!(described(&rig, A));
    assert_eq!(
        stamps(&rig, B),
        [11, 12, 13, 14, 15, 16],
        "B's tail publishes once its writes recover"
    );
    assert!(described(&rig, B));

    ingest_at(&rig, B, 17, "user 17 logged in").await;
    cut(&rig);
    assert!(
        rig.commits.last_checkpoint() > Some(held.x1),
        "later cuts stamp"
    );
    assert_eq!(
        rig.commits.held_floors(),
        floors(&[(A, HeldFloor::At(held.x0))]),
        "A's floor is still X0, so nothing of A's is lost"
    );
}

/// RFC0056.10's re-minted case: a replay at or below the checkpoint that
/// only re-mints template events is bounded too, and waits out B's
/// failing writes without republishing a row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replay_that_only_re_mints_events_is_bounded_too() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = node(tmp.path());
    ingest_each(
        &rig,
        B,
        11,
        &["user 11 logged in", "job 12 queued", "disk 13 is full"],
    )
    .await;
    cut(&rig);
    let snapshot = rig.snapshots_root.join(format!("{B}.snap"));
    drop(rig);
    std::fs::remove_file(snapshot).expect("B's snapshot goes, so its frame re-mints");

    let (rig, recovered) = restart_unrepaired(tmp.path(), false);
    let _ = rig
        .publish
        .write_ordered(rig.publish.drain_all(), "shutdown");
    let report = recovered.expect("recovered");
    assert_eq!(report.records_suppressed_for_parquet, 3, "{report:?}");
    assert!(
        report.passes.peak_audit_events <= 1 && report.passes.waits > 0,
        "the re-minted events are paced and wait out B's writes: {:?}",
        report.passes
    );
    assert_eq!(
        stamps(&rig, B),
        [11, 12, 13],
        "no row of B's is republished"
    );
    assert!(described(&rig, B));
}

/// Append one frame of `tenant`'s records to `wal`.
fn append(wal: &mut Wal, tenant: &str, stamps: (u64, Option<u64>), bodies: &[&str]) {
    let request = request(vec![stamped(tenant, stamps, bodies)]);
    let frame = TenantBatch::encode(tenant, &request.encode_to_vec()).expect("frame");
    wal.append(FrameKind::TenantOtlpBatch, &frame)
        .expect("append");
}

/// A WAL and the sinks a starting receiver recovers into, built as it
/// builds them: the record sink quarantines into the audit sink.
struct BareNode {
    tmp: tempfile::TempDir,
    wal: Wal,
    publish: PublishCoordinator,
    miner: MinerCluster,
}

impl BareNode {
    fn new() -> Self {
        let tmp = tempfile::TempDir::new().expect("temp");
        for dir in ["data", "audit"] {
            std::fs::create_dir_all(tmp.path().join(dir)).expect("root");
        }
        let wal = Wal::open(wal_config(&tmp.path().join("wal"))).expect("open");
        let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(
            Store::local(tmp.path().join("audit")).expect("audit store"),
            100_000,
        ));
        let record = SharedParquetSink::new(
            ParquetRecordSink::new(
                Store::local(tmp.path().join("data")).expect("data store"),
                never_flush(),
            )
            .with_audit_sink(Box::new(audit.clone())),
        );
        let publish = PublishCoordinator::new(record.clone(), audit.clone());
        let miner = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(audit))
            .with_record_sink(Box::new(record));
        Self {
            tmp,
            wal,
            publish,
            miner,
        }
    }

    /// Recover in passes bounded at `audit_events`, with no record bound,
    /// then publish what is left as the post-recovery flush does, and the
    /// quarantine events that publish made.
    fn recover(&mut self, audit_events: usize) -> recovery::PassReport {
        let ids = TemplateIds::new(Store::local(self.tmp.path().join("audit")).expect("store"))
            .with_bootstrap_allowed(true);
        let stopping = AtomicBool::new(false);
        let passes = recovery::Passes {
            publish: &self.publish,
            record_bytes: usize::MAX,
            audit_events,
            stopping: &stopping,
        };
        let snapshots = self.tmp.path().join("snapshots");
        let report = passes
            .recover((&mut self.wal, &mut self.miner), &snapshots, &ids)
            .expect("recover")
            .passes;
        let _ = self
            .publish
            .write_ordered(self.publish.drain_all(), "shutdown");
        // That publish's own quarantine events, as the next sweep would.
        assert!(self.publish.audit().flush());
        report
    }

    fn quarantined(&self) -> usize {
        audit_events(&self.tmp.path().join("audit"))
            .iter()
            .filter(|e| matches!(e.payload, AuditPayload::RecordQuarantined { .. }))
            .count()
    }

    fn published(&self) -> Vec<u64> {
        rows(&self.tmp.path().join("data"))
            .iter()
            .map(|r| r.time_unix_nano)
            .collect()
    }
}

/// #1407: a pass that quarantines rows the store rejects for good emits
/// one event per row into the audit sink while it runs (RFC 0025 §3.3).
/// A pass never drains more records than the audit sink has room for, so
/// that never takes it past its bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pass_that_quarantines_rows_stays_within_the_audit_bound() {
    let mut node = BareNode::new();
    let poisoned = [
        "user 11 logged in",
        "user 12 logged in",
        "user 13 logged in",
    ];
    append(&mut node.wal, B, (11, Some(u64::MAX)), &poisoned);
    append(&mut node.wal, B, (14, None), &["job 14 queued"]);
    node.wal.sync().expect("sync");

    let report = node.recover(2);

    assert!(
        report.peak_audit_events <= 2,
        "quarantine never takes the audit sink past its bound: {report:?}"
    );
    assert_eq!(node.quarantined(), 3, "every rejected row is quarantined");
    assert_eq!(
        node.published(),
        [14],
        "and only the encodable row publishes"
    );
}

/// Records already in the record sink when recovery starts count against
/// the audit sink's room as well: they drain with the first pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn records_buffered_before_recovery_count_against_the_audit_room() {
    let mut node = BareNode::new();
    let rejected = stamped(
        B,
        (11, Some(u64::MAX)),
        &["user 11 logged in", "user 12 logged in"],
    );
    for record in assign(request(vec![rejected]), &TenantId::new(B)) {
        node.miner.ingest(&record);
    }
    append(
        &mut node.wal,
        B,
        (13, Some(u64::MAX)),
        &["user 13 logged in"],
    );
    node.wal.sync().expect("sync");

    let report = node.recover(2);

    assert!(
        report.peak_audit_events <= 2,
        "the buffered rows drain within the audit bound: {report:?}"
    );
    assert_eq!(node.quarantined(), 3, "every rejected row is quarantined");
}
