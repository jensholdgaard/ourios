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
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use ourios_core::audit::{AuditEvent, AuditPayload, AuditSink};
use ourios_core::tenant::TenantId;
use ourios_ingester::barrier::CutOutcome;
use ourios_ingester::housekeeping::{Housekeeper, HousekeepingTick};
use ourios_ingester::recovery::{self, RecoveryReport};
use ourios_ingester::snapshot_store;
use ourios_ingester::template_ids::TemplateIds;
use ourios_parquet::Store;
use ourios_wal::{HeldFloor, HeldFloors, WalConfig, WalOffset};

use crate::ingest_support::{request, resource_logs};
use crate::rfc0052_barrier_support::{
    BarrierRig, RigSpec, Startup, audit_events, rows, wal_config,
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
    let mut logs = resource_logs(tenant, &[body]);
    for record in &mut logs.scope_logs[0].log_records {
        record.time_unix_nano = n;
    }
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

/// Recovery in passes bounded at one buffered byte and one buffered
/// event, with A's fault still present; its report lands in `report`.
fn unrepaired_recovery(tmp: &Path, report: Arc<Mutex<Option<RecoveryReport>>>) -> Box<Startup> {
    let (snapshots, audit) = (tmp.join("wal").join("snapshots"), tmp.join("audit"));
    Box::new(move |wal, miner, publish| {
        let real = miner.replace_audit_sink(Box::new(ourios_core::audit::NoOpAuditSink));
        let _ = miner.replace_audit_sink(Box::new(PreEpochFor(real)));
        let ids = TemplateIds::new(Store::local(&audit).expect("audit store"))
            .with_bootstrap_allowed(true);
        let stopping = AtomicBool::new(false);
        let passes = recovery::Passes {
            publish,
            record_bytes: 1,
            audit_events: 1,
            stopping: &stopping,
        };
        let recovered = passes
            .recover((wal, miner), &snapshots, &ids)
            .expect("recover");
        let horizons = recovered.accepted_horizons();
        *report.lock().expect("report") = Some(recovered);
        horizons
    })
}

/// B's audit partition refuses every write until `for_ms` has passed: a
/// file sits where its directory belongs.
fn block_b_audit_writes(tmp: &Path, for_ms: u64) -> std::thread::JoinHandle<()> {
    let partition = tmp
        .join("audit")
        .join("audit")
        .join(format!("tenant_id={B}"));
    let aside = partition.with_extension("aside");
    std::fs::rename(&partition, &aside).expect("move B's partition aside");
    std::fs::write(&partition, b"not a directory").expect("block it");
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(for_ms));
        std::fs::remove_file(&partition).expect("unblock");
        std::fs::rename(&aside, &partition).expect("put B's partition back");
    })
}

/// Scenario RFC0056.10 — a restart into an unrepaired fault holds the
/// tenant again. See `docs/rfcs/0056-audit-durability.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0056_10_a_restart_into_an_unrepaired_fault_holds_the_tenant_again() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (rig, held) = held_after_x0(tmp.path()).await;
    for (n, body) in [
        (14, "job 14 queued"),
        (15, "cache warmed in 15 ms"),
        (16, "disk 16 is full"),
    ] {
        ingest_at(&rig, B, n, body).await;
    }
    drop(rig);

    let unblock = block_b_audit_writes(tmp.path(), 400);
    let report = Arc::new(Mutex::new(None));
    let rig = BarrierRig::build(
        tmp.path(),
        RigSpec {
            startup: Some(unrepaired_recovery(tmp.path(), Arc::clone(&report))),
            ..spec(tmp.path(), false)
        },
    );
    unblock.join().expect("unblocked");
    let _ = rig
        .publish
        .write_ordered(rig.publish.drain_all(), "shutdown");
    let passes = report
        .lock()
        .expect("report")
        .take()
        .expect("recovered")
        .passes;

    assert!(
        rig.audit.held().is_held(&TenantId::new(A)),
        "A is held again"
    );
    assert_eq!(
        (passes.peak_record_bytes, passes.peak_audit_events),
        (0, 0),
        "no frame is read while a sink is at its bound: {passes:?}"
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
