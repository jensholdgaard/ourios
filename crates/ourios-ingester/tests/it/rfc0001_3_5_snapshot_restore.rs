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

use std::path::{Path, PathBuf};

use crate::ingest_support::{
    open_pipeline, request, resource_logs, template_ids, tenant_for, wal_config, write_snapshots_at,
};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_config::MinerConfig;

use ourios_ingester::recovery;
use ourios_ingester::template_ids::TemplateIds;
use ourios_miner::cluster::MinerCluster;
use ourios_miner::snapshot::RecoveryOutcome;
use ourios_wal::{FrameKind, Wal, WalOffset};
use prost::Message;

/// Feed every record of `requests` (in order) to `miner`, returning
/// the record count.
fn ingest_all(miner: &mut MinerCluster, requests: &[ExportLogsServiceRequest]) -> u64 {
    let mut count = 0;
    for request in requests {
        for record in ourios_ingester::receiver::assign(request.clone(), &tenant_for(request)) {
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

/// Two single-line batches for one tenant.
fn two_checkout_batches() -> [ExportLogsServiceRequest; 2] {
    [
        request(vec![resource_logs("checkout", &["user 1 logged in"])]),
        request(vec![resource_logs("checkout", &["user 2 logged in"])]),
    ]
}

/// A live pipeline over `root` that has ingested `batches` in order.
async fn ingested(
    root: &Path,
    batches: &[ExportLogsServiceRequest],
) -> ourios_ingester::receiver::IngestPipeline {
    let pipeline = open_pipeline(root);
    for r in batches {
        pipeline
            .ingest(r.clone(), tenant_for(r))
            .await
            .expect("ingest");
    }
    pipeline
}

/// Recover `root` into a fresh miner, and assert its one artefact was
/// discarded and every record full-replayed to the control's state.
fn assert_discarded_and_full_replayed(
    root: &Path,
    control: &MinerCluster,
    total_records: u64,
) -> recovery::RecoveryReport {
    let mut wal = Wal::open(wal_config(root)).expect("reopen WAL");
    let mut recovered = MinerCluster::new(MinerConfig::default());
    let report = recovery::recover(
        &mut wal,
        &root.join("snapshots"),
        &mut recovered,
        &template_ids(root),
    )
    .expect("recover");
    assert_eq!(report.tenants.len(), 1);
    assert_eq!(
        report.tenants[0].outcome(),
        RecoveryOutcome::UnknownOrCorruptDiscarded,
    );
    assert_eq!(report.records_suppressed_for_miner, 0);
    assert_eq!(report.records_fed_to_miner, total_records);
    assert_equivalent(&recovered, control);
    report
}

/// One batch per `(tenant, lines)`.
fn batches(spec: [(&str, &[&str]); 3]) -> [ExportLogsServiceRequest; 3] {
    spec.map(|(tenant, lines)| request(vec![resource_logs(tenant, lines)]))
}

/// The §3.5.3 batches the live node ingests at or below its snapshot's
/// mark `S`.
fn batches_below_s() -> [ExportLogsServiceRequest; 3] {
    batches([
        ("checkout", &["user 1 logged in", "user 2 logged in"]),
        ("billing", &["charge 9 EUR accepted"]),
        ("checkout", &["user 1 logged out"]),
    ])
}

/// The §3.5.3 batches above `S`, the last a shape the tail mints.
fn batches_above_s() -> [ExportLogsServiceRequest; 3] {
    batches([
        ("checkout", &["user 3 logged in", "user 3 viewed cart"]),
        ("billing", &["charge 12 EUR accepted"]),
        ("checkout", &["disk sda1 is 91 percent full"]),
    ])
}

/// The live node's template-id state: a high-water at `high_water`, and
/// a root that seated before it snapshotted.
fn seated_live_node(root: &Path, snapshots_root: &Path, high_water: u64) -> TemplateIds {
    let ids = template_ids(root);
    ids.store()
        .put_blocking(
            ourios_ingester::template_ids::HIGH_WATER_KEY,
            format!(r#"{{"reserved_through": {high_water}}}"#).into_bytes(),
        )
        .expect("the live node's high-water");
    ourios_ingester::template_ids::mark_seated(snapshots_root, 0)
        .expect("the live node seated before it snapshotted");
    ids
}

/// Scenario §3.5.3 — Known-version restore + tail replay is
/// equivalent to a full rebuild, up to RFC 0059's renaming of the ids
/// the tail replay first mints (Scenario RFC0059.9, the §3.5.3 narrowing
/// the maintainer approved on 2026-10-05).
/// See `docs/rfcs/0001-template-miner.md` §3.5 and
/// `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0001_3_5_3_restore_plus_tail_replay_equals_full_rebuild() {
    // Arrange: ingest two batches through the live pipeline, snapshot
    // at the durable mark S, then ingest two more above S.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let snapshots_root = root.join("snapshots");
    let (pre, post) = (batches_below_s(), batches_above_s());

    let pipeline = open_pipeline(root);
    for r in &pre {
        pipeline
            .ingest(r.clone(), tenant_for(r))
            .await
            .expect("ingest pre-S batch");
    }
    let s = pipeline
        .last_durable()
        .expect("a synced batch yields the durable mark");
    pipeline.with_miner(|m| write_snapshots_at(&snapshots_root, m, Some(s)));
    for r in &post {
        pipeline
            .ingest(r.clone(), tenant_for(r))
            .await
            .expect("ingest post-S batch");
    }
    drop(pipeline);

    let mut control = MinerCluster::new(MinerConfig::default());
    let pre_records = ingest_all(&mut control, &pre);
    let post_records = ingest_all(&mut control, &post);

    // The live node reserved ahead of what it issued, as RFC 0059 §3.2
    // requires: the store's high-water is above every id the control
    // mints.
    let issued = control.highest_allocated();
    let high_water = issued + 37;
    let ids = seated_live_node(root, &snapshots_root, high_water);

    // Act: recover into a fresh miner over the same WAL + snapshots.
    let mut wal = Wal::open(wal_config(root)).expect("reopen WAL");
    let mut recovered = MinerCluster::new(MinerConfig::default());
    let report =
        recovery::recover(&mut wal, &snapshots_root, &mut recovered, &ids).expect("recover");

    // Assert (a): restored + tail-replayed state equals the
    // from-scratch control, per tenant, up to an injective renaming of
    // the ids first minted in the tail, which all lie above the
    // high-water and so collide with no id issued before the restart.
    let renamed =
        crate::rfc0059_support::assert_equivalent_up_to_renaming(&recovered, &control, high_water);
    assert!(
        !renamed.is_empty(),
        "the tail minted templates, so the renaming is exercised"
    );
    assert!(renamed.iter().all(|id| *id > issued), "{renamed:?}");

    // Assert (b): no frame at or below S reached the miner — every
    // pre-S record was suppressed, every post-S record fed.
    assert!(report.records_suppressed_for_miner > 0);
    assert_eq!(report.records_suppressed_for_miner, pre_records);
    assert_eq!(report.records_fed_to_miner, post_records);
    assert_eq!(report.frames_delivered, (pre.len() + post.len()) as u64);

    // Assert (c): both tenants restored, no stale gap.
    assert_eq!(report.tenants.len(), 2);
    for tenant in &report.tenants {
        assert_eq!(tenant.outcome(), RecoveryOutcome::Restored);
        assert!(!tenant.stale_gap, "{:?}", tenant.tenant_id.as_str());
    }
}

/// Scenario §3.5.2 (through the driver) — a corrupt-version artefact
/// is discarded and that tenant full-replays to the same state as a
/// from-scratch rebuild.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0001_3_5_2_corrupt_version_discards_and_full_replays() {
    // Arrange: a WAL with two batches and a snapshot artefact whose
    // version byte is unknown.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let snapshots_root = root.join("snapshots");

    let batches = two_checkout_batches();
    let pipeline = ingested(root, &batches).await;
    drop(pipeline);

    std::fs::create_dir_all(&snapshots_root).expect("snapshots dir");
    std::fs::write(snapshots_root.join("checkout.snap"), [0xFF, 0x01, 0x02])
        .expect("write corrupt artefact");

    let mut control = MinerCluster::new(MinerConfig::default());
    let total_records = ingest_all(&mut control, &batches);

    // Act + Assert: artefact discarded, nothing suppressed, full-replay
    // state equals the control.
    let report = assert_discarded_and_full_replayed(root, &control, total_records);
    assert!(!report.tenants[0].stale_gap);
}

/// A known-version artefact with no recorded high-water mark is
/// discarded, not restored: a restore without a horizon cannot
/// suppress, so replay would re-feed every frame the snapshot already
/// folded (the v1 double-apply hazard; §6.9 maps it to the discard
/// class). The tenant full-replays to the from-scratch state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0001_3_5_snapshot_without_a_horizon_discards_and_full_replays() {
    // Arrange: a WAL with two batches and a snapshot written without
    // a high-water mark (degraded-shutdown shape).
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let snapshots_root = root.join("snapshots");

    let batches = two_checkout_batches();
    let pipeline = ingested(root, &batches).await;
    pipeline.with_miner(|m| write_snapshots_at(&snapshots_root, m, None));
    drop(pipeline);

    let mut control = MinerCluster::new(MinerConfig::default());
    let total_records = ingest_all(&mut control, &batches);

    // Act + Assert: discarded (not restored without suppression),
    // nothing suppressed, full-replay state equals the control.
    assert_discarded_and_full_replayed(root, &control, total_records);
}

/// RFC 0052 §3.1: a version-1 artefact carries the old global mark, so
/// it is never restored as a version-2 horizon. It is discarded like any
/// unknown version, and the tenant rebuilds from the WAL.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0001_3_5_a_version_1_artefact_discards_and_full_replays() {
    // Arrange: a WAL with two batches and a well-formed snapshot of
    // them, horizon and all, written under version 1.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    let snapshots_root = root.join("snapshots");

    let batches = two_checkout_batches();
    let pipeline = ingested(root, &batches).await;
    let mark = pipeline.last_durable();
    pipeline.with_miner(|m| write_snapshots_at(&snapshots_root, m, mark));
    drop(pipeline);
    let artefact = snapshots_root.join("checkout.snap");
    let mut bytes = std::fs::read(&artefact).expect("the artefact");
    bytes[0] = 1;
    std::fs::write(&artefact, bytes).expect("rewrite as version 1");

    let mut control = MinerCluster::new(MinerConfig::default());
    let total_records = ingest_all(&mut control, &batches);

    // Act + Assert: discarded, no horizon seeded, and the replay rebuilt
    // the tenant from every frame.
    let report = assert_discarded_and_full_replayed(root, &control, total_records);
    assert!(report.accepted_horizons().is_empty());
}

/// Mint a closed segment holding one `TenantOtlpBatch` frame per request:
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
            wal.append(
                FrameKind::TenantOtlpBatch,
                &ourios_wal::TenantBatch::encode("checkout", &r.encode_to_vec()).expect("frame"),
            )
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
    // RFC 0052 §3.2: a root holding a version-2 segment beside no
    // sidecars is fail-closed, so the scratch root's `RECLAIM` moves
    // with the segment it belongs to. Without it this hand-built root
    // is a shape no node can produce.
    let record = dest_root.join("RECLAIM");
    if !record.exists() {
        std::fs::copy(scratch.path().join("RECLAIM"), &record).expect("bring the record along");
    }
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
    write_snapshots_at(&snapshots_root, &snap_miner, Some(s));

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
    let report = recovery::recover(
        &mut wal,
        &snapshots_root,
        &mut recovered,
        &template_ids(root),
    )
    .expect("recover");

    // Assert: restored + flagged, surviving frames folded, no error.
    assert_eq!(report.tenants.len(), 1);
    assert_eq!(report.tenants[0].outcome(), RecoveryOutcome::Restored);
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0001_3_5_cold_start_without_snapshots_full_replays() {
    // Arrange: a WAL with batches and no snapshots dir at all.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();

    let batches = [
        request(vec![resource_logs("checkout", &["user 1 logged in"])]),
        request(vec![resource_logs("billing", &["charge 9 EUR accepted"])]),
    ];
    let pipeline = open_pipeline(root);
    for r in &batches {
        pipeline
            .ingest(r.clone(), tenant_for(r))
            .await
            .expect("ingest");
    }
    drop(pipeline);

    let mut control = MinerCluster::new(MinerConfig::default());
    let total_records = ingest_all(&mut control, &batches);

    // Act
    let mut wal = Wal::open(wal_config(root)).expect("reopen WAL");
    let mut recovered = MinerCluster::new(MinerConfig::default());
    let report = recovery::recover(
        &mut wal,
        &root.join("snapshots"),
        &mut recovered,
        &template_ids(root),
    )
    .expect("recover");

    // Assert
    assert!(report.tenants.is_empty(), "no artefacts, no outcomes");
    assert_eq!(report.records_suppressed_for_miner, 0);
    assert_eq!(report.records_fed_to_miner, total_records);
    assert_eq!(report.parquet_horizon, None);
    assert_equivalent(&recovered, &control);
}
