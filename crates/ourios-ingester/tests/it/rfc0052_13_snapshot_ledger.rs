//! RFC0052.13 — the snapshot ledger housekeeping reclaims against holds
//! only horizons a restart would honour.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §3.2.
//!
//! A horizon the next start would discard is worse than none: the pass
//! unlinks the frames it covers, and that start then needs exactly
//! those frames to rebuild the tenant.

use std::sync::Arc;
use std::time::Duration;

use ourios_config::MinerConfig;
use ourios_core::tenant::TenantId;
use ourios_ingester::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use ourios_ingester::barrier::{Barrier, CutOutcome};
use ourios_ingester::housekeeping::{Housekeeper, HousekeepingTick};
use ourios_ingester::publish::PublishCoordinator;
use ourios_ingester::receiver::CommitCoordinator;
use ourios_ingester::record_sink::{ParquetRecordSink, SharedParquetSink};
use ourios_ingester::template_ids::TemplateIds;
use ourios_ingester::{recovery, snapshot_store};
use ourios_miner::cluster::MinerCluster;
use ourios_miner::snapshot::RecoveryOutcome;
use ourios_parquet::Store;
use ourios_wal::{SnapshotHorizons, Wal, WalConfig, WalOffset};

use crate::rfc0052_barrier_support::{BarrierRig, RigSpec, never_flush, wal_config};

/// A snapshot that decodes and carries a horizon, but whose state the
/// miner refuses, is discarded by recovery — so it seeds no horizon, and
/// the first pass keeps the frames the next start will replay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_snapshot_recovery_rejects_seeds_no_horizon_and_the_pass_keeps_its_frames() {
    // Given a sealed segment the barrier stamped and snapshotted.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::build(tmp.path(), RigSpec::new(aging_wal(tmp.path())));
    let mark = rig.ingest("checkout", &["user 1 logged in"]).await;
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(rig.barrier.tick(&rig.pipeline, true), CutOutcome::Stamped);
    let sealed = rig.wal_root.join(format!("{}.wal", mark.segment));
    let snapshots_root = rig.snapshots_root.clone();
    let ids = TemplateIds::new(Store::local(&rig.audit_root).expect("audit store"));
    drop(rig);

    // And its artefact replaced by one the codec reads, horizon and all,
    // but that `restore_tenant` rejects: a template id appears twice.
    let tenant = TenantId::new("checkout");
    let bytes = std::fs::read(snapshots_root.join("checkout.snap")).expect("the artefact");
    let (Some(mut state), RecoveryOutcome::Restored) =
        ourios_miner::snapshot::recover(Some(&bytes))
    else {
        panic!("the barrier's artefact decodes");
    };
    let leaf = state.leaves.first().cloned().expect("a mined leaf");
    state.leaves.push(leaf);
    snapshot_store::write(&snapshots_root, &tenant, &state).expect("rewrite the artefact");

    // When the node restarts and seeds the ledger from recovery.
    let mut wal = Wal::open(aging_wal(tmp.path())).expect("reopen");
    let mut miner = MinerCluster::new(MinerConfig::default());
    let report = recovery::recover(&mut wal, &snapshots_root, &mut miner, &ids).expect("recover");
    assert_eq!(
        report.tenants[0].outcome(),
        RecoveryOutcome::UnknownOrCorruptDiscarded,
        "recovery discards the artefact",
    );
    assert!(
        report.accepted_horizons().is_empty(),
        "so it seeds no horizon",
    );
    let node = Node::over(wal, tmp.path());
    let seeded = node.housekeeper(report.accepted_horizons());

    // Then the first pass keeps the sealed segment: the tenant is pinned.
    let HousekeepingTick::Completed(kept) = seeded.tick() else {
        panic!("the pass runs");
    };
    assert_eq!(kept.removed_segments, 0, "{kept:?}");
    assert!(sealed.exists(), "the frames the next start replays survive");

    // And the control: the same pass seeded with the rejected artefact's
    // horizon would have unlinked them.
    let trusting = node.housekeeper(vec![(tenant, mark)]);
    let HousekeepingTick::Completed(removed) = trusting.tick() else {
        panic!("the pass runs");
    };
    assert_eq!(removed.removed_segments, 1, "{removed:?}");
    assert!(!sealed.exists());
}

/// What housekeeping reclaims against is, tenant by tenant, exactly the
/// horizon the next start restores — including a tenant that stayed
/// idle across a later cut.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_ledger_holds_exactly_the_horizons_a_restart_restores() {
    // Given two tenants cut together, then a cut in which one is idle.
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::build(tmp.path(), RigSpec::new(aging_wal(tmp.path())));
    rig.ingest("checkout", &["user 1 logged in"]).await;
    rig.ingest("search", &["query 7 served"]).await;
    assert_eq!(rig.barrier.tick(&rig.pipeline, false), CutOutcome::Stamped);
    rig.ingest("checkout", &["user 2 logged in"]).await;
    assert_eq!(rig.barrier.tick(&rig.pipeline, false), CutOutcome::Stamped);

    // When the ledger is read, and the node restarts.
    let ledger = rig.barrier.snapshot_horizons();
    let wal_root = rig.wal_root.clone();
    let snapshots_root = rig.snapshots_root.clone();
    let ids = TemplateIds::new(Store::local(&rig.audit_root).expect("audit store"));
    drop(rig);
    let mut wal = Wal::open(wal_config(&wal_root)).expect("reopen");
    let mut miner = MinerCluster::new(MinerConfig::default());
    let report = recovery::recover(&mut wal, &snapshots_root, &mut miner, &ids).expect("recover");

    // Then both name the same horizon for every tenant.
    assert_eq!(report.accepted_horizons().len(), 2, "both tenants restore");
    assert_eq!(
        ledger,
        SnapshotHorizons::restorable(report.accepted_horizons()),
        "the ledger is what the artefacts carry, tenant by tenant",
    );
}

/// A reopened WAL behind the journal owner, with the sinks a barrier
/// needs, so a housekeeper can be built over it with any seed.
struct Node {
    commits: Arc<CommitCoordinator>,
    publish: PublishCoordinator,
    snapshots_root: std::path::PathBuf,
}

impl Node {
    fn over(wal: Wal, tmp: &std::path::Path) -> Self {
        let data_root = tmp.join("restart-data");
        let audit_root = tmp.join("restart-audit");
        for dir in [&data_root, &audit_root] {
            std::fs::create_dir_all(dir).expect("store dir");
        }
        let sink = SharedParquetSink::new(ParquetRecordSink::new(
            Store::local(&data_root).expect("data store"),
            never_flush(),
        ));
        let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(
            Store::local(&audit_root).expect("audit store"),
            1_024,
        ));
        Self {
            commits: CommitCoordinator::new(
                Box::new(wal),
                Duration::from_millis(20),
                ourios_wal::MIN_SEGMENT_SIZE_BYTES,
            ),
            publish: PublishCoordinator::new(sink, audit),
            snapshots_root: tmp.join("wal").join("snapshots"),
        }
    }

    fn housekeeper(&self, seed: Vec<(TenantId, WalOffset)>) -> Housekeeper {
        let barrier = Barrier::new(
            self.publish.clone(),
            Arc::clone(&self.commits),
            self.snapshots_root.clone(),
            usize::MAX,
        )
        .with_durable_horizons(seed);
        Housekeeper::new(
            Arc::clone(&self.commits),
            Arc::new(barrier),
            self.publish.clone(),
            usize::try_from(ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS).expect("the cap fits"),
        )
    }
}

/// A WAL whose segment ages out after one second, so the barrier's idle
/// rotation seals it.
fn aging_wal(tmp: &std::path::Path) -> WalConfig {
    WalConfig {
        segment_age_secs: 1,
        ..wal_config(&tmp.join("wal"))
    }
}
