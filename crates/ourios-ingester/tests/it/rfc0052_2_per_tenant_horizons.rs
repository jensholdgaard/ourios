//! RFC0052.2 at the barrier: every snapshot carries its tenant's own
//! folded horizon (§3.1), never the cut's mark, so an idle tenant keeps
//! its older horizon across later cuts and the floor housekeeping
//! reclaims against is the minimum over what each tenant folded.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §3.1, §3.2.

use std::sync::Arc;
use std::time::Duration;

use ourios_core::tenant::TenantId;
use ourios_ingester::barrier::CutOutcome;
use ourios_ingester::housekeeping::{Housekeeper, HousekeepingTick};
use ourios_ingester::snapshot_store;
use ourios_wal::{RetainFloor, SnapshotHorizons, WalConfig, WalOffset};

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
    let housekeeper = Housekeeper::new(
        Arc::clone(&rig.commits),
        Arc::clone(&rig.barrier),
        rig.publish.clone(),
        usize::try_from(ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS).expect("the cap fits"),
    );
    let HousekeepingTick::Completed(pass) = housekeeper.tick() else {
        panic!("the pass runs");
    };
    assert_eq!(pass.floor, RetainFloor::Min(idle), "{pass:?}");
    assert_eq!(pass.removed_segments, 2, "{pass:?}");
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
