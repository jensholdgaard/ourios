//! RFC0008.10 *snapshot-cadence arm* — a WAL segment rotation
//! triggers a per-tenant snapshot write recording the
//! rotation-point high-water mark (RFC 0001 §6.9's primary
//! cadence). See `docs/rfcs/0008-wal.md` §5.
//!
//! Drives a real `Wal` across its age cap (the §6.9 minimum,
//! 1 s) through the live `IngestPipeline` with the same
//! rotation hook the server role installs, then asserts the
//! artefact on disk: stamped with the *old* segment's last
//! durable offset, and reflecting only the records ingested
//! before the rotating batch.

mod ingest_support;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ingest_support::{request, resource_logs, wal_config};
use ourios_ingester::receiver::{IngestPipeline, TenantRule};
use ourios_ingester::{recovery, snapshot_store};
use ourios_miner::cluster::MinerCluster;
use ourios_miner::snapshot::load_snapshot;
use ourios_wal::{Wal, WalConfig};

fn rotating_pipeline(root: &Path, snapshots_root: &Path) -> IngestPipeline {
    let wal = Wal::open(WalConfig {
        segment_age_secs: 1,
        ..wal_config(root)
    })
    .expect("open WAL");
    let hook_root = snapshots_root.to_path_buf();
    IngestPipeline::new(
        Box::new(wal),
        MinerCluster::new(ourios_core::config::MinerConfig::default()),
        TenantRule::service_name(),
    )
    .with_rotation_hook(Box::new(move |miner, mark| {
        recovery::write_snapshots(&hook_root, miner, Some(mark)).expect("snapshot write");
    }))
}

#[test]
fn rotation_writes_snapshots_at_the_rotation_point_high_water() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let snapshots_root = tmp.path().join("snapshots");
    let mut pipeline = rotating_pipeline(tmp.path(), &snapshots_root);

    // Batch A lands in the first segment; the durable mark after it
    // is the rotation-point high-water the hook must stamp.
    pipeline
        .ingest(request(vec![resource_logs("svc", &["user 1 logged in"])]))
        .expect("batch A");
    let rotation_point = pipeline.last_durable().expect("durable after batch A");
    assert!(
        snapshot_store::load_all(&snapshots_root)
            .expect("load")
            .is_empty(),
        "no rotation yet, no cadence write",
    );

    // Crossing the age cap makes batch B rotate; the hook fires
    // before B's records reach the miner. (Age is whole seconds
    // from the segment UUID's mint time and the cap is strict, so
    // > 2 s of wall time guarantees age ≥ 2 > 1.)
    std::thread::sleep(Duration::from_millis(2_200));
    pipeline
        .ingest(request(vec![resource_logs("svc", &["payment 9 settled"])]))
        .expect("batch B");

    let artefacts = snapshot_store::load_all(&snapshots_root).expect("load");
    assert_eq!(artefacts.len(), 1, "one tenant, one artefact");
    let (tenant, bytes) = &artefacts[0];
    assert_eq!(tenant.as_str(), "svc");
    let state = load_snapshot(bytes).expect("known version");
    let mark = state.wal_high_water.expect("stamped with a horizon");
    assert_eq!(
        mark.segment,
        rotation_point.segment.to_string(),
        "the artefact records the rotation-point segment (the just-closed one)",
    );
    assert_eq!(
        mark.byte, rotation_point.byte,
        "…at the old segment's last durable byte",
    );
    assert_eq!(
        state.leaves.len(),
        1,
        "the snapshot reflects only batch A — the hook ran before \
         batch B's records reached the miner",
    );

    // The post-rotation state is intact: both batches in the miner.
    assert_eq!(
        pipeline
            .miner()
            .template_count(&ourios_core::tenant::TenantId::new("svc")),
        2,
        "batch B still reached the miner after the hook",
    );
}

/// The hook only fires on a rotation — steady-state batches in one
/// segment write nothing.
#[test]
fn no_rotation_means_no_cadence_write() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let snapshots_root = tmp.path().join("snapshots");
    let hook_root = snapshots_root.clone();
    let fired = Arc::new(Mutex::new(0u32));
    let count = fired.clone();
    let wal = Wal::open(wal_config(tmp.path())).expect("open WAL");
    let mut pipeline = IngestPipeline::new(
        Box::new(wal),
        MinerCluster::new(ourios_core::config::MinerConfig::default()),
        TenantRule::service_name(),
    )
    .with_rotation_hook(Box::new(move |miner, mark| {
        *count.lock().expect("lock") += 1;
        recovery::write_snapshots(&hook_root, miner, Some(mark)).expect("snapshot write");
    }));

    for body in ["a 1", "b 2", "c 3"] {
        pipeline
            .ingest(request(vec![resource_logs("svc", &[body])]))
            .expect("ingest");
    }
    assert_eq!(*fired.lock().expect("lock"), 0, "no rotation, no firing");
    assert!(
        snapshot_store::load_all(&snapshots_root)
            .expect("load")
            .is_empty(),
    );
}
