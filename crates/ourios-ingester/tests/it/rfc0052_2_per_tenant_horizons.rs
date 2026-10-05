//! RFC0052.2 at the barrier: every snapshot carries its tenant's own
//! folded horizon (§3.1), never the cut's mark, so an idle tenant keeps
//! its older horizon across later cuts and the floor housekeeping
//! reclaims against is the minimum over what each tenant folded. A
//! restart then reads the reclaimed idle segment as explained by the
//! `RECLAIM` record, and only an unexplained absence as a stale gap.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §3.1, §3.2.

use std::sync::Arc;
use std::time::Duration;

use ourios_config::MinerConfig;
use ourios_core::tenant::TenantId;
use ourios_ingester::barrier::CutOutcome;
use ourios_ingester::housekeeping::{Housekeeper, HousekeepingTick};
use ourios_ingester::recovery::{self, RecoveryReport};
use ourios_ingester::snapshot_store;
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::Store;
use ourios_wal::{HousekeepingProgress, RetainFloor, SnapshotHorizons, Wal, WalConfig, WalOffset};

use crate::rfc0052_barrier_support::{BarrierRig, RigSpec, wal_config};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_tenant_keeps_its_own_horizon_and_the_pass_reclaims_to_the_minimum() {
    // Given two tenants cut together in a segment the cut seals.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::build(tmp.path(), RigSpec::new(aging_wal(tmp.path())));
    let idle = rig.ingest("search", &["query 7 served"]).await;
    rig.ingest("checkout", &["user 1 logged in"]).await;
    seal_and_cut(&rig).await;

    // When only the other tenant writes, across two later cuts.
    rig.ingest("checkout", &["user 2 logged in"]).await;
    seal_and_cut(&rig).await;
    let busy = rig.ingest("checkout", &["user 3 logged in"]).await;
    assert_eq!(rig.barrier.tick(&rig.pipeline, false), CutOutcome::Stamped);

    // Then the idle tenant's artefact and ledger entry keep its own last
    // frame rather than rising to either later mark.
    assert_eq!(disk_horizon(&rig, "search"), Some(idle));
    assert_eq!(disk_horizon(&rig, "checkout"), Some(busy));
    assert_eq!(
        rig.barrier.snapshot_horizons(),
        SnapshotHorizons::restorable([
            (TenantId::new("search"), idle),
            (TenantId::new("checkout"), busy),
        ]),
        "the ledger records exactly what each artefact carries",
    );

    // And the pass reclaims against the minimum of the two: both sealed
    // segments hold only frames at or below their tenant's horizon, and
    // the floor it reports is the idle tenant's, not the checkpoint's.
    let pass = housekeeping_pass(&rig);
    assert_eq!(pass.floor, RetainFloor::Min(idle), "{pass:?}");
    assert_eq!(pass.removed_segments, 2, "{pass:?}");

    // And a restart restores the idle tenant at its horizon with no
    // stale-gap report: its segment is gone, but the `RECLAIM` entry at
    // `S` explains why.
    let report = restart(rig);
    assert_eq!(report.accepted_horizons().len(), 2, "both tenants restore");
    assert!(!stale_gap(&report, "search"), "{report:?}");
    assert!(!stale_gap(&report, "checkout"), "{report:?}");
}

/// The other half of the same witness: a horizon segment deleted from
/// outside, above what any pass reclaimed for its tenant, is still a
/// stale gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_horizon_segment_deleted_above_the_reclaimed_through_still_warns() {
    // Given a tenant a pass has already reclaimed some frames of.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::build(tmp.path(), RigSpec::new(aging_wal(tmp.path())));
    rig.ingest("search", &["query 7 served"]).await;
    rig.ingest("checkout", &["user 1 logged in"]).await;
    seal_and_cut(&rig).await;
    assert_eq!(housekeeping_pass(&rig).removed_segments, 1);

    // And a later horizon of its in a sealed segment no pass reached,
    // under a checkpoint above it.
    let horizon = rig.ingest("search", &["query 8 served"]).await;
    seal_and_cut(&rig).await;
    rig.ingest("checkout", &["user 2 logged in"]).await;
    seal_and_cut(&rig).await;
    assert_eq!(disk_horizon(&rig, "search"), Some(horizon));

    // When that segment is deleted from outside the WAL, and the node
    // restarts.
    std::fs::remove_file(rig.wal_root.join(format!("{}.wal", horizon.segment)))
        .expect("delete the horizon segment");
    let report = restart(rig);

    // Then the gap is reported for that tenant alone.
    assert!(stale_gap(&report, "search"), "{report:?}");
    assert!(!stale_gap(&report, "checkout"), "{report:?}");
}

/// One housekeeping pass over the rig's WAL, against its barrier's
/// ledger.
fn housekeeping_pass(rig: &BarrierRig) -> HousekeepingProgress {
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

/// Stop the rig's node and run startup recovery over what it left.
fn restart(rig: BarrierRig) -> RecoveryReport {
    let (wal_root, snapshots_root) = (rig.wal_root.clone(), rig.snapshots_root.clone());
    let audit = Store::local(&rig.audit_root).expect("audit store");
    drop(rig);
    let mut wal = Wal::open(WalConfig {
        segment_age_secs: 1,
        ..wal_config(&wal_root)
    })
    .expect("reopen");
    let mut miner = MinerCluster::new(MinerConfig::default());
    recovery::recover(&mut wal, &snapshots_root, &mut miner, &audit).expect("recover")
}

fn stale_gap(report: &RecoveryReport, tenant: &str) -> bool {
    report
        .tenants
        .iter()
        .find(|t| t.tenant_id.as_str() == tenant)
        .is_some_and(|t| t.stale_gap)
}

/// Let the open segment age out, then take a cut whose idle rotation
/// seals it.
async fn seal_and_cut(rig: &BarrierRig) {
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(rig.barrier.tick(&rig.pipeline, true), CutOutcome::Stamped);
}

/// The horizon `tenant`'s installed artefact carries.
fn disk_horizon(rig: &BarrierRig, tenant: &str) -> Option<WalOffset> {
    let bytes = std::fs::read(rig.snapshots_root.join(format!("{tenant}.snap"))).expect("artefact");
    let (state, _) = ourios_miner::snapshot::recover(Some(&bytes));
    snapshot_store::offset_of(state.expect("decodes").wal_high_water.as_ref()?)
}

/// A WAL whose segment ages out after one second, so the barrier's idle
/// rotation seals it.
fn aging_wal(tmp: &std::path::Path) -> WalConfig {
    WalConfig {
        segment_age_secs: 1,
        ..wal_config(&tmp.join("wal"))
    }
}
