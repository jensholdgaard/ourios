//! RFC 0001 §3.5.3 / §3.5.4 — snapshot restore + tail replay through
//! the RFC 0008 §6.6 recovery driver.
//! See `docs/rfcs/0001-template-miner.md` §3.5 and
//! `docs/rfcs/0008-wal.md` RFC0008.10.
//!
//! Three arms: restore-equivalence (a snapshot at `S` plus replay of
//! only the frames above `S` equals a from-scratch rebuild, §3.5.3),
//! corrupt-version discard + full-replay equivalence (§3.5.2 through
//! the driver) with the stale-gap arm (§3.5.4 — externally truncated
//! WAL degrades loudly, not silently), and the no-snapshot cold start.

mod ingest_support;

use std::path::{Path, PathBuf};

use ingest_support::{open_pipeline, request, resource_logs, wal_config};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_core::config::MinerConfig;
use ourios_ingester::receiver::{TenantRule, fan_out};
use ourios_ingester::recovery;
use ourios_miner::cluster::MinerCluster;
use ourios_miner::snapshot::RecoveryOutcome;
use ourios_wal::{FrameKind, Wal, WalOffset};
use prost::Message;

/// Feed every record of `requests` (in order) to `miner`, returning
/// the record count.
fn ingest_all(miner: &mut MinerCluster, requests: &[ExportLogsServiceRequest]) -> u64 {
    let rule = TenantRule::service_name();
    let mut count = 0;
    for request in requests {
        for record in fan_out(request.clone(), &rule).expect("fan out") {
            miner.ingest(&record);
            count += 1;
        }
    }
    count
}

/// Assert both clusters hold the same tenants with field-identical
/// per-tenant state, compared via the §6.9 snapshot payload (both
/// sides carry `wal_high_water: None` straight off the cluster).
fn assert_equivalent(recovered: &MinerCluster, control: &MinerCluster) {
    assert_eq!(recovered.tenant_ids(), control.tenant_ids());
    for tenant in control.tenant_ids() {
        assert_eq!(
            recovered.snapshot_state(&tenant),
            control.snapshot_state(&tenant),
            "tenant {:?} diverges from the from-scratch control",
            tenant.as_str(),
        );
    }
}

/// Scenario §3.5.3 — Known-version restore + tail replay is
/// equivalent to a full rebuild.
/// See `docs/rfcs/0001-template-miner.md` §3.5.
#[test]
fn rfc0001_3_5_3_restore_plus_tail_replay_equals_full_rebuild() {
    // Arrange: ingest two batches through the live pipeline, snapshot
    // at the durable mark S, then ingest two more above S.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let snapshots_root = root.join("snapshots");
    let rule = TenantRule::service_name();

    let pre = [
        request(vec![
            resource_logs("checkout", &["user 1 logged in", "user 2 logged in"]),
            resource_logs("billing", &["charge 9 EUR accepted"]),
        ]),
        request(vec![resource_logs("checkout", &["user 1 logged out"])]),
    ];
    let post = [
        request(vec![resource_logs(
            "checkout",
            &["user 3 logged in", "user 3 viewed cart"],
        )]),
        request(vec![resource_logs("billing", &["charge 12 EUR accepted"])]),
    ];

    let mut pipeline = open_pipeline(root);
    for r in &pre {
        pipeline.ingest(r.clone()).expect("ingest pre-S batch");
    }
    let s = pipeline
        .last_durable()
        .expect("a synced batch yields the durable mark");
    recovery::write_snapshots(&snapshots_root, pipeline.miner(), Some(s)).expect("snapshot at S");
    for r in &post {
        pipeline.ingest(r.clone()).expect("ingest post-S batch");
    }
    drop(pipeline);

    let mut control = MinerCluster::new(MinerConfig::default());
    let pre_records = ingest_all(&mut control, &pre);
    let post_records = ingest_all(&mut control, &post);

    // Act: recover into a fresh miner over the same WAL + snapshots.
    let mut wal = Wal::open(wal_config(root)).expect("reopen WAL");
    let mut recovered = MinerCluster::new(MinerConfig::default());
    let report =
        recovery::recover(&mut wal, &snapshots_root, &mut recovered, &rule).expect("recover");

    // Assert (a): restored + tail-replayed state equals the
    // from-scratch control, per tenant.
    assert_equivalent(&recovered, &control);

    // Assert (b): no frame at or below S reached the miner — every
    // pre-S record was suppressed, every post-S record fed.
    assert!(report.records_suppressed_for_miner > 0);
    assert_eq!(report.records_suppressed_for_miner, pre_records);
    assert_eq!(report.records_fed_to_miner, post_records);
    assert_eq!(report.frames_delivered, (pre.len() + post.len()) as u64);

    // Assert (c): both tenants restored, no stale gap.
    assert_eq!(report.tenants.len(), 2);
    for tenant in &report.tenants {
        assert_eq!(tenant.outcome, RecoveryOutcome::Restored);
        assert!(!tenant.stale_gap, "{:?}", tenant.tenant_id.as_str());
    }
}

/// Scenario §3.5.2 (through the driver) — a corrupt-version artefact
/// is discarded and that tenant full-replays to the same state as a
/// from-scratch rebuild.
#[test]
fn rfc0001_3_5_2_corrupt_version_discards_and_full_replays() {
    // Arrange: a WAL with two batches and a snapshot artefact whose
    // version byte is unknown.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let snapshots_root = root.join("snapshots");
    let rule = TenantRule::service_name();

    let batches = [
        request(vec![resource_logs("checkout", &["user 1 logged in"])]),
        request(vec![resource_logs("checkout", &["user 2 logged in"])]),
    ];
    let mut pipeline = open_pipeline(root);
    for r in &batches {
        pipeline.ingest(r.clone()).expect("ingest");
    }
    drop(pipeline);

    std::fs::create_dir_all(&snapshots_root).expect("snapshots dir");
    std::fs::write(snapshots_root.join("checkout.snap"), [0xFF, 0x01, 0x02])
        .expect("write corrupt artefact");

    let mut control = MinerCluster::new(MinerConfig::default());
    let total_records = ingest_all(&mut control, &batches);

    // Act
    let mut wal = Wal::open(wal_config(root)).expect("reopen WAL");
    let mut recovered = MinerCluster::new(MinerConfig::default());
    let report =
        recovery::recover(&mut wal, &snapshots_root, &mut recovered, &rule).expect("recover");

    // Assert: artefact discarded, nothing suppressed, full-replay
    // state equals the control.
    assert_eq!(report.tenants.len(), 1);
    assert_eq!(
        report.tenants[0].outcome,
        RecoveryOutcome::UnknownOrCorruptDiscarded,
    );
    assert!(!report.tenants[0].stale_gap);
    assert_eq!(report.records_suppressed_for_miner, 0);
    assert_eq!(report.records_fed_to_miner, total_records);
    assert_equivalent(&recovered, &control);
}

/// A known-version artefact with no recorded high-water mark is
/// discarded, not restored: a restore without a horizon cannot
/// suppress, so replay would re-feed every frame the snapshot already
/// folded (the v1 double-apply hazard; §6.9 maps it to the discard
/// class). The tenant full-replays to the from-scratch state.
#[test]
fn rfc0001_3_5_snapshot_without_a_horizon_discards_and_full_replays() {
    // Arrange: a WAL with two batches and a snapshot written without
    // a high-water mark (degraded-shutdown shape).
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let snapshots_root = root.join("snapshots");
    let rule = TenantRule::service_name();

    let batches = [
        request(vec![resource_logs("checkout", &["user 1 logged in"])]),
        request(vec![resource_logs("checkout", &["user 2 logged in"])]),
    ];
    let mut pipeline = open_pipeline(root);
    for r in &batches {
        pipeline.ingest(r.clone()).expect("ingest");
    }
    recovery::write_snapshots(&snapshots_root, pipeline.miner(), None)
        .expect("snapshot without a high-water mark");
    drop(pipeline);

    let mut control = MinerCluster::new(MinerConfig::default());
    let total_records = ingest_all(&mut control, &batches);

    // Act
    let mut wal = Wal::open(wal_config(root)).expect("reopen WAL");
    let mut recovered = MinerCluster::new(MinerConfig::default());
    let report =
        recovery::recover(&mut wal, &snapshots_root, &mut recovered, &rule).expect("recover");

    // Assert: discarded (not restored without suppression), nothing
    // suppressed, full-replay state equals the control.
    assert_eq!(report.tenants.len(), 1);
    assert_eq!(
        report.tenants[0].outcome,
        RecoveryOutcome::UnknownOrCorruptDiscarded,
    );
    assert_eq!(report.records_suppressed_for_miner, 0);
    assert_eq!(report.records_fed_to_miner, total_records);
    assert_equivalent(&recovered, &control);
}

/// Mint a closed segment holding one `OtlpBatch` frame per request:
/// build it in a scratch root through the public API, then move the
/// file into `dest_root` (rotation, RFC0008.6, is not implemented
/// yet — same construction as ourios-wal's checkpoint tests).
/// Returns the per-frame append offsets.
fn build_closed_segment(
    dest_root: &Path,
    requests: &[&ExportLogsServiceRequest],
) -> Vec<WalOffset> {
    let scratch = tempfile::TempDir::new().expect("scratch root");
    let mut wal = Wal::open(wal_config(scratch.path())).expect("open scratch");
    let offsets = requests
        .iter()
        .map(|r| {
            wal.append(FrameKind::OtlpBatch, &r.encode_to_vec())
                .expect("append")
        })
        .collect();
    wal.sync().expect("sync");
    drop(wal);
    let seg = segment_files(scratch.path())
        .into_iter()
        .next()
        .expect("scratch holds one segment");
    std::fs::create_dir_all(dest_root).expect("dest root");
    let dest = dest_root.join(seg.file_name().expect("segment file name"));
    std::fs::rename(&seg, &dest).expect("move segment into dest root");
    offsets
}

/// Sorted `*.wal` paths under `root`.
fn segment_files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(root)
        .expect("read_dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "wal"))
        .collect();
    out.sort();
    out
}

/// Scenario §3.5.4 — Stale snapshot degrades loudly, not silently:
/// the WAL is externally truncated past the snapshot's high-water
/// mark `S` (segment file manually unlinked) while a checkpoint
/// `X > S` retains everything above it. Recovery restores, replays
/// the survivors, and flags the gap.
#[test]
fn rfc0001_3_5_4_externally_truncated_wal_flags_a_stale_gap() {
    // Arrange: two closed segments; snapshot at S = the end of
    // segment 1; checkpoint X inside segment 2; then unlink segment 1
    // (the external mutation — the §6.7 retain floor prevents this
    // arising internally).
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let snapshots_root = root.join("snapshots");
    let rule = TenantRule::service_name();

    let seg1_batches = [
        request(vec![resource_logs("checkout", &["user 1 logged in"])]),
        request(vec![resource_logs("checkout", &["user 2 logged in"])]),
    ];
    let seg2_batch = request(vec![resource_logs(
        "checkout",
        &["user 3 logged in", "user 3 logged out"],
    )]);

    let seg1_offsets = build_closed_segment(root, &[&seg1_batches[0], &seg1_batches[1]]);
    let seg2_offsets = build_closed_segment(root, &[&seg2_batch]);
    let s = *seg1_offsets.last().expect("segment 1 offsets");
    let x = seg2_offsets[0];

    let mut snap_miner = MinerCluster::new(MinerConfig::default());
    ingest_all(&mut snap_miner, &seg1_batches);
    recovery::write_snapshots(&snapshots_root, &snap_miner, Some(s)).expect("snapshot at S");

    {
        let mut wal = Wal::open(wal_config(root)).expect("open for checkpoint");
        wal.checkpoint(x).expect("checkpoint X > S");
    }
    let seg1_file = segment_files(root)
        .into_iter()
        .next()
        .expect("segment 1 file");
    std::fs::remove_file(&seg1_file).expect("externally unlink segment 1");

    let mut control = MinerCluster::new(MinerConfig::default());
    ingest_all(&mut control, &seg1_batches);
    ingest_all(&mut control, std::slice::from_ref(&seg2_batch));

    // Act
    let mut wal = Wal::open(wal_config(root)).expect("reopen WAL");
    let mut recovered = MinerCluster::new(MinerConfig::default());
    let report =
        recovery::recover(&mut wal, &snapshots_root, &mut recovered, &rule).expect("recover");

    // Assert: restored + flagged, surviving frames folded, no error.
    assert_eq!(report.tenants.len(), 1);
    assert_eq!(report.tenants[0].outcome, RecoveryOutcome::Restored);
    assert!(
        report.tenants[0].stale_gap,
        "the gap between S and the oldest survivor is flagged",
    );
    assert_eq!(report.parquet_horizon, Some(x));
    assert_eq!(report.records_fed_to_miner, 2, "segment 2's records fold");
    assert_eq!(report.records_suppressed_for_miner, 0);
    assert_equivalent(&recovered, &control);
}

/// No-snapshot cold start: full replay, an empty tenants list, and
/// equivalence with the from-scratch control.
#[test]
fn rfc0001_3_5_cold_start_without_snapshots_full_replays() {
    // Arrange: a WAL with batches and no snapshots dir at all.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let rule = TenantRule::service_name();

    let batches = [
        request(vec![resource_logs("checkout", &["user 1 logged in"])]),
        request(vec![resource_logs("billing", &["charge 9 EUR accepted"])]),
    ];
    let mut pipeline = open_pipeline(root);
    for r in &batches {
        pipeline.ingest(r.clone()).expect("ingest");
    }
    drop(pipeline);

    let mut control = MinerCluster::new(MinerConfig::default());
    let total_records = ingest_all(&mut control, &batches);

    // Act
    let mut wal = Wal::open(wal_config(root)).expect("reopen WAL");
    let mut recovered = MinerCluster::new(MinerConfig::default());
    let report = recovery::recover(&mut wal, &root.join("snapshots"), &mut recovered, &rule)
        .expect("recover");

    // Assert
    assert!(report.tenants.is_empty(), "no artefacts, no outcomes");
    assert_eq!(report.records_suppressed_for_miner, 0);
    assert_eq!(report.records_fed_to_miner, total_records);
    assert_eq!(report.parquet_horizon, None);
    assert_equivalent(&recovered, &control);
}
