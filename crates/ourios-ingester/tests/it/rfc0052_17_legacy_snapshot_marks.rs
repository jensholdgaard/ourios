//! RFC0052.17's legacy stale-gap belt, fed from version-1 snapshots at
//! startup. See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md`
//! §3.2.
//!
//! A version-1 artefact restores nothing: its miner state is discarded
//! and the tenant full-replays. Its global `wal_high_water` is still
//! decoded for this one check, and the check runs inside recovery,
//! before the post-recovery write replaces the artefact at version 2
//! and destroys the evidence.

use std::path::Path;

use ourios_config::MinerConfig;
use ourios_core::tenant::TenantId;
use ourios_ingester::recovery::{self, RecoveryDriverError};
use ourios_ingester::snapshot_store;
use ourios_miner::cluster::MinerCluster;
use ourios_miner::snapshot::SNAPSHOT_VERSION;
use ourios_wal::{FrameKind, RotationKind, TenantBatch, Wal, WalOffset};
use prost::Message;

use crate::ingest_support::{request, resource_logs, template_ids, wal_config};

/// A pre-RFC root reclaimed past its version-1 snapshot fails closed,
/// naming the tenant.
#[test]
fn a_version_1_mark_below_the_oldest_surviving_frame_refuses_startup() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let [first, _] = legacy_root(tmp.path(), Reclaimed::FirstSegment);
    write_v1_snapshot(tmp.path(), "alpha", Some(first));

    let refused = recover(tmp.path())
        .err()
        .expect("the stale gap must fail closed");

    assert!(
        matches!(refused, RecoveryDriverError::LegacyStaleGap(_)),
        "{refused}"
    );
    assert!(format!("{refused}").contains("alpha"), "{refused}");
}

/// The ordinary pre-RFC root: its version-1 mark explains the oldest
/// surviving frame, so it boots, rebuilds the tenant from every frame,
/// and the next write is at the current version.
#[test]
fn a_version_1_mark_at_the_oldest_frame_boots_and_rewrites_at_the_current_version() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let [first, last] = legacy_root(tmp.path(), Reclaimed::Nothing);
    write_v1_snapshot(tmp.path(), "alpha", Some(first));

    let (report, miner) = recover(tmp.path()).expect("the root boots");
    assert_eq!(report.records_suppressed_for_miner, 0, "nothing restored");
    assert_eq!(report.records_fed_to_miner, 2, "every frame replayed");

    let installed = recovery::write_folded_snapshots(&snapshots(tmp.path()), &miner)
        .expect("post-recovery write");
    assert_eq!(installed, vec![(TenantId::new("alpha"), last)]);
    let bytes = std::fs::read(snapshots(tmp.path()).join("alpha.snap")).expect("artefact");
    assert_eq!(
        bytes[0], SNAPSHOT_VERSION,
        "rewritten at the current version"
    );
}

/// A version-1 artefact that does not decode even for its mark has no
/// horizon to compare, and fails closed rather than reading as "nothing
/// reclaimed".
#[test]
fn an_undecodable_version_1_artefact_refuses_startup() {
    let tmp = tempfile::TempDir::new().expect("temp");
    legacy_root(tmp.path(), Reclaimed::Nothing);
    write_undecodable_v1(tmp.path(), "alpha");

    let refused = recover(tmp.path()).err().expect("no mark must fail closed");

    assert!(format!("{refused}").contains("alpha"), "{refused}");
}

/// The ledger cannot name a tenant with no surviving frame, so an
/// undecodable version-1 artefact for one must still refuse startup:
/// booting would silently discard the only record of its templates.
#[test]
fn an_undecodable_version_1_artefact_refuses_startup_without_any_frame() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let [first, _] = legacy_root(tmp.path(), Reclaimed::Nothing);
    write_v1_snapshot(tmp.path(), "alpha", Some(first));
    write_undecodable_v1(tmp.path(), "beta");

    let refused = recover(tmp.path()).err().expect("no mark must fail closed");

    assert!(
        matches!(&refused, RecoveryDriverError::LegacyMarkUnreadable(t) if t.as_str() == "beta"),
        "{refused}"
    );
}

/// A readable version-1 mark for a tenant with no surviving frame is the
/// documented upgrade consequence, not a gap: the node boots.
#[test]
fn a_version_1_mark_for_a_tenant_without_any_frame_boots() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let [first, _] = legacy_root(tmp.path(), Reclaimed::Nothing);
    write_v1_snapshot(tmp.path(), "alpha", Some(first));
    write_v1_snapshot(tmp.path(), "beta", Some(first));

    recover(tmp.path()).expect("the root boots");
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reclaimed {
    Nothing,
    FirstSegment,
}

/// A pre-RFC root: one `alpha` frame in each of two segments, both
/// segment headers and the `CHECKPOINT` at version 1, and no `RECLAIM`
/// record. `FirstSegment` removes the first segment, as reclamation
/// under a version-1 checkpoint would have. Returns the two frames.
fn legacy_root(root: &Path, reclaimed: Reclaimed) -> [WalOffset; 2] {
    let mut wal = Wal::open(wal_config(root)).expect("open");
    let first = append(&mut wal, "user 1 logged in");
    wal.rotate(RotationKind::Owed).expect("rotate");
    let last = append(&mut wal, "user 2 logged in");
    drop(wal);
    if reclaimed == Reclaimed::FirstSegment {
        std::fs::remove_file(root.join(format!("{}.wal", first.segment))).expect("reclaim");
    }
    std::fs::remove_file(root.join("RECLAIM")).expect("a pre-RFC root has no record");
    downgrade_segments(root);
    write_legacy_checkpoint(root, last);
    [first, last]
}

fn append(wal: &mut Wal, line: &str) -> WalOffset {
    let request = request(vec![resource_logs("alpha", &[line])]);
    let frame = TenantBatch::encode("alpha", &request.encode_to_vec()).expect("frame");
    let offset = wal
        .append(FrameKind::TenantOtlpBatch, &frame)
        .expect("append");
    wal.sync().expect("sync");
    offset
}

/// `tenant`'s artefact as the pre-RFC writer left it: format version 1,
/// its global mark in `wal_high_water`.
pub(crate) fn write_v1_snapshot(root: &Path, tenant: &str, mark: Option<WalOffset>) {
    let tenant = TenantId::new(tenant);
    let mut state = MinerCluster::new(MinerConfig::default()).snapshot_state(&tenant);
    state.wal_high_water = mark.map(snapshot_store::high_water);
    snapshot_store::write(&snapshots(root), &tenant, &state).expect("write");
    let path = snapshots(root).join(format!("{}.snap", tenant.as_str()));
    let mut bytes = std::fs::read(&path).expect("read");
    bytes[0] = 1;
    std::fs::write(&path, bytes).expect("rewrite as version 1");
}

/// A version-1 artefact for `tenant` that does not decode even for its
/// mark.
fn write_undecodable_v1(root: &Path, tenant: &str) {
    std::fs::create_dir_all(snapshots(root)).expect("snapshots dir");
    std::fs::write(
        snapshots(root).join(format!("{tenant}.snap")),
        [1, 0x7B, 0x21],
    )
    .expect("an undecodable version-1 artefact");
}

fn recover(root: &Path) -> Result<(recovery::RecoveryReport, MinerCluster), RecoveryDriverError> {
    let mut wal = Wal::open(wal_config(root)).expect("reopen the legacy root");
    let mut miner = MinerCluster::new(MinerConfig::default());
    let report = recovery::recover(&mut wal, &snapshots(root), &mut miner, &template_ids(root))?;
    Ok((report, miner))
}

fn snapshots(root: &Path) -> std::path::PathBuf {
    root.join("snapshots")
}

/// A version-1 `CHECKPOINT`, as `ourios-wal`'s RFC 0052 test support
/// writes it: magic, version, then the offset.
pub(crate) fn write_legacy_checkpoint(root: &Path, offset: WalOffset) {
    let mut out = vec![0u8; 32];
    out[0..4].copy_from_slice(b"OWCK");
    out[4..6].copy_from_slice(&1u16.to_le_bytes());
    out[8..24].copy_from_slice(offset.segment.as_bytes());
    out[24..32].copy_from_slice(&offset.byte.to_le_bytes());
    std::fs::write(root.join("CHECKPOINT"), &out).expect("write a version-1 CHECKPOINT");
}

/// Every segment header's version field back to 1: a root whose
/// segments all predate RFC 0052.
pub(crate) fn downgrade_segments(root: &Path) {
    for entry in std::fs::read_dir(root).expect("read root") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|ext| ext == "wal") {
            let mut bytes = std::fs::read(&path).expect("read segment");
            bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
            std::fs::write(&path, &bytes).expect("rewrite segment header");
        }
    }
}
