//! RFC0052.10 — No acknowledged record is lost across the whole cycle.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! The kill leg **extends** `rfc0014_5_crash_no_loss.rs` (RFC 0052 §6)
//! rather than adding a parallel crash test: a real `SIGKILL` of a
//! fixture child, with reclamation configured on a short cadence so the
//! kill lands in the regime this RFC introduces. The existing test stays
//! as it is. The #791 refuse-then-resume regression tests move with the
//! bound to RFC 0053.
//!
//! The republication and audit legs drive the production barrier in
//! process and stop the node without a shutdown write — what a kill
//! leaves on disk — then run startup recovery over it.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_config::MinerConfig;
use ourios_core::audit::{AuditEvent, SharedAuditSink};
use ourios_core::clock::TestClock;
use ourios_core::otlp::OtlpLogRecord;
use ourios_core::record::{MinedRecord, SharedRecordSink};
use ourios_core::tenant::TenantId;
use ourios_ingester::barrier::CutOutcome;
use ourios_ingester::receiver::tenant::assign;
use ourios_ingester::record_sink::{ParquetRecordSink, SharedParquetSink};
use ourios_ingester::recovery::{self, RecoveryReport};
use ourios_ingester::snapshot_store;
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::{Reader, Store};
use ourios_wal::{FrameKind, FrameSink, RecoveryError, TenantBatch, Wal, WalConfig, WalOffset};
use prost::Message;

use crate::ingest_support::{request, resource_logs};
use crate::rfc0052_barrier_support::{BarrierRig, never_flush, wal_config};

const TENANT: &str = "checkout";

/// Scenario RFC0052.10 — SIGKILL with reclamation and rotation retry live.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.10 stub — implemented in the crash-and-soak green slice F (rfc0014_5 fixture on a short reclamation cadence)"]
fn rfc0052_10_every_acked_record_survives_a_kill_during_reclamation() {
    todo!(
        "RFC0052.10 — a node killed with SIGKILL mid-batch while \
         reclamation and rotation retry are both live; it restarts and \
         recovery completes: every acknowledged record is present in \
         Parquet, including those whose segments were candidates for \
         reclamation at the moment of the kill"
    );
}

/// Scenario RFC0052.10 — recovery republishes nothing at or below `max(X, S)`.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
///
/// First the `S < X` shape: the snapshot lags an advanced checkpoint, so
/// replay must feed `(S, X]` to the miner and never to the record sink.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_10_replay_below_the_mark_feeds_the_miner_but_not_the_record_sink() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = lagging_snapshot_node(tmp.path(), Leftover::Lagging).await;
    assert_eq!(
        stamps(&rows(&node.data_root)),
        [1, 2],
        "the two cuts published records 1 and 2 before the kill",
    );

    let report = recover_into_store(&node);

    assert_eq!(
        stamps(&rows(&node.data_root)),
        [1, 2, 3],
        "every acknowledged record is in Parquet exactly once: nothing in (S, X] \
         was republished, and record 3 above X was",
    );
    assert_eq!(report.parquet_horizon, Some(node.checkpoint));
    assert_eq!(
        report.records_fed_to_miner, 2,
        "the miner rebuilds from S: records 2 and 3"
    );
    assert_eq!(
        report.records_suppressed_for_parquet, 1,
        "record 2 is at or below X, so it is mined and withheld",
    );
}

/// Scenario RFC0052.10 — the same gate with no `S` at all: a discarded
/// snapshot makes replay re-mine every frame, and none at or below `X` is
/// republished.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_10_a_discarded_snapshot_republishes_nothing_at_or_below_the_checkpoint() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = lagging_snapshot_node(tmp.path(), Leftover::Undecodable).await;

    let report = recover_into_store(&node);

    assert_eq!(
        stamps(&rows(&node.data_root)),
        [1, 2, 3],
        "a full replay publishes only what lies above X",
    );
    assert_eq!(
        report.records_fed_to_miner, 3,
        "the miner rebuilds from scratch"
    );
    assert_eq!(report.records_suppressed_for_parquet, 2);
}

/// Scenario RFC0052.10 — the `S > X` shape: the snapshot landed and the
/// checkpoint write then failed, so nothing in `(X, S]` is republished.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_10_a_snapshot_ahead_of_a_failed_checkpoint_republishes_nothing_up_to_it() {
    // Given cut 1 stamped (X = S = frame 1), then cut 2's snapshot
    // installed with its checkpoint write failing (S = frame 2 > X).
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    let first = ingest(&rig, 1, "user 1 logged in").await;
    assert_eq!(rig.barrier.tick(&rig.pipeline, false), CutOutcome::Stamped);
    let checkpoint_bytes = std::fs::read(rig.wal_root.join("CHECKPOINT")).expect("CHECKPOINT");
    rig.sabotage_checkpoint();
    let second = ingest(&rig, 2, "user 2 logged in").await;
    assert_eq!(rig.barrier.tick(&rig.pipeline, false), CutOutcome::Stamped);
    assert_eq!(
        rig.commits.last_checkpoint(),
        Some(first),
        "X stayed at frame 1"
    );
    ingest(&rig, 3, "order 3 shipped").await;
    let node = Node::stop(rig, first);
    // The sabotage replaced the sidecar with a directory; a failed write
    // of the real rename leaves the previous file, which is what the
    // restart must read.
    std::fs::remove_dir_all(node.wal_root.join("CHECKPOINT")).expect("undo the sabotage");
    std::fs::write(node.wal_root.join("CHECKPOINT"), checkpoint_bytes).expect("previous mark");
    assert_eq!(stamps(&rows(&node.data_root)), [1, 2]);

    let report = recover_into_store(&node);

    assert_eq!(report.parquet_horizon, Some(first));
    assert_eq!(
        report.accepted_horizons(),
        [(TenantId::new(TENANT), second)]
    );
    assert_eq!(report.records_suppressed_for_miner, 2, "(X, S] is folded");
    assert_eq!(report.records_suppressed_for_parquet, 0);
    assert_eq!(
        stamps(&rows(&node.data_root)),
        [1, 2, 3],
        "record 2 in (X, S] is not republished; record 3 above S is",
    );
}

/// Scenario RFC0052.10 — the audit stream is gated the same way.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0052_10_audit_events_above_the_mark_are_forwarded_exactly_once_in_frame_order() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = lagging_snapshot_node(tmp.path(), Leftover::Lagging).await;
    // A stored AuditEvent frame above X: regeneration is the only source,
    // so it must not surface.
    let mut wal = Wal::open(node.wal()).expect("reopen");
    wal.append(FrameKind::AuditEvent, b"stored event")
        .expect("append");
    wal.sync().expect("sync");
    drop(wal);
    let expected = reference_events(&node);

    let events = SharedAuditSink::new();
    let records = SharedRecordSink::new();
    let mut miner = pinned_miner(&events).with_record_sink(Box::new(records.clone()));
    let mut wal = Wal::open(node.wal()).expect("reopen");
    let report = recovery::recover(&mut wal, &node.snapshots_root, &mut miner).expect("recover");

    let forwarded = events.drain();
    assert!(
        !expected.withheld.is_empty() && !expected.forwarded.is_empty(),
        "both sides of X regenerate events, so the gate is exercised: {} / {}",
        expected.withheld.len(),
        expected.forwarded.len(),
    );
    assert_eq!(
        forwarded, expected.forwarded,
        "the forwarded events are exactly the reference's for (X, tail], in frame order",
    );
    assert_eq!(
        report.audit_events_suppressed,
        expected.withheld.len() as u64,
        "the (S, X] events were regenerated, withheld and counted",
    );
    assert_eq!(
        report.frames_delivered, 4,
        "the stored event frame was replayed"
    );
    assert_eq!(
        stamps(&records.drain()),
        [3],
        "and the record sink saw only the frame above X",
    );
}

/// A stopped node's roots and the checkpoint it stamped.
struct Node {
    wal_root: PathBuf,
    data_root: PathBuf,
    snapshots_root: PathBuf,
    checkpoint: WalOffset,
}

impl Node {
    /// Stop `rig` with no shutdown write — the on-disk state a kill leaves.
    fn stop(rig: BarrierRig, checkpoint: WalOffset) -> Self {
        let node = Self {
            wal_root: rig.wal_root.clone(),
            data_root: rig.data_root.clone(),
            snapshots_root: rig.snapshots_root.clone(),
            checkpoint,
        };
        drop(rig);
        node
    }

    fn wal(&self) -> WalConfig {
        wal_config(&self.wal_root)
    }

    fn artefact(&self) -> PathBuf {
        self.snapshots_root.join(format!("{TENANT}.snap"))
    }
}

/// What the kill left where the tenant's snapshot artefact belongs.
#[derive(Clone, Copy)]
enum Leftover {
    /// Cut 1's artefact (S = frame 1): cut 2's write is replaced by
    /// rename, so its failure leaves the previous artefact in place.
    Lagging,
    /// An artefact recovery discards, so the tenant has no `S` at all.
    Undecodable,
}

/// Records 1 and 2 published by two stamped cuts (X = frame 2), the
/// snapshot artefact as `leftover` says, and record 3 acknowledged but
/// only buffered.
///
/// §3.1 lets a cut stamp over a failed snapshot write, but this barrier
/// retains instead (RFC0052.7 pins that), so the lagging artefact is put
/// back after the second cut rather than produced by a failed one.
async fn lagging_snapshot_node(tmp: &Path, leftover: Leftover) -> Node {
    let rig = BarrierRig::new(tmp);
    ingest(&rig, 1, "user alice logged in").await;
    assert_eq!(rig.barrier.tick(&rig.pipeline, false), CutOutcome::Stamped);
    let lagging = std::fs::read(rig.snapshots_root.join(format!("{TENANT}.snap"))).expect("S");
    let second = ingest(&rig, 2, "user bob logged in").await;
    assert_eq!(rig.barrier.tick(&rig.pipeline, false), CutOutcome::Stamped);
    assert_eq!(rig.commits.last_checkpoint(), Some(second), "X is frame 2");
    ingest(&rig, 3, "order 3 shipped").await;
    let node = Node::stop(rig, second);
    let bytes = match leftover {
        Leftover::Lagging => lagging,
        Leftover::Undecodable => b"not a snapshot".to_vec(),
    };
    std::fs::write(node.artefact(), bytes).expect("the leftover artefact");
    node
}

/// Ingest one record stamped `n` (its `time_unix_nano`, the identity the
/// assertions read back) and return its frame's offset.
async fn ingest(rig: &BarrierRig, n: u64, body: &str) -> WalOffset {
    let mut logs = resource_logs(TENANT, &[body]);
    logs.scope_logs[0].log_records[0].time_unix_nano = n;
    rig.pipeline
        .ingest(request(vec![logs]), TenantId::new(TENANT))
        .await
        .expect("the batch acks");
    rig.pipeline.last_durable().expect("a durable mark")
}

/// Restart over `node` with the record sink on its store, as `serve`
/// wires it, and flush what recovery published.
fn recover_into_store(node: &Node) -> RecoveryReport {
    let store = Store::local(&node.data_root).expect("store");
    let sink = SharedParquetSink::new(ParquetRecordSink::new(store, never_flush()));
    let mut miner =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    let mut wal = Wal::open(node.wal()).expect("reopen");
    let report = recovery::recover(&mut wal, &node.snapshots_root, &mut miner).expect("recover");
    sink.flush_all();
    report
}

/// A miner whose events go to `events` and whose clock is pinned, so a
/// reference mine and a recovery mine stamp identical events.
fn pinned_miner(events: &SharedAuditSink) -> MinerCluster {
    MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(events.clone()))
        .with_clock(Box::new(TestClock::new(SystemTime::UNIX_EPOCH)))
}

/// The reference mine's events, split at X.
struct Reference {
    withheld: Vec<AuditEvent>,
    forwarded: Vec<AuditEvent>,
}

/// Mine every tenant frame above `S` from the same snapshot with the same
/// clock, and split the events it emits at the checkpoint.
fn reference_events(node: &Node) -> Reference {
    let artefacts = snapshot_store::load_all(&node.snapshots_root).expect("artefacts");
    let (tenant, bytes) = artefacts.into_iter().next().expect("one artefact");
    let (Some(state), _) = ourios_miner::snapshot::recover(Some(&bytes)) else {
        panic!("the lagging artefact decodes");
    };
    let horizon = state
        .wal_high_water
        .as_ref()
        .and_then(snapshot_store::offset_of)
        .expect("S");
    let events = SharedAuditSink::new();
    let mut miner = pinned_miner(&events);
    miner.restore_tenant(&tenant, &state).expect("restore");
    let mut withheld = Vec::new();
    for (offset, records) in frames(node).into_iter().filter(|(o, _)| *o > horizon) {
        for record in &records {
            miner.ingest(record);
        }
        if offset <= node.checkpoint {
            withheld.extend(events.drain());
        }
    }
    Reference {
        withheld,
        forwarded: events.drain(),
    }
}

/// Every tenant frame in the WAL, in order, fanned out to its records.
fn frames(node: &Node) -> Vec<(WalOffset, Vec<OtlpLogRecord>)> {
    struct Collect(Vec<(WalOffset, Vec<OtlpLogRecord>)>);
    impl FrameSink for Collect {
        fn consume(
            &mut self,
            offset: WalOffset,
            kind: FrameKind,
            payload: &[u8],
        ) -> Result<(), RecoveryError> {
            if kind == FrameKind::TenantOtlpBatch {
                let batch = TenantBatch::decode(payload).expect("tenant frame");
                let tenant = TenantId::new(batch.tenant);
                let export = ExportLogsServiceRequest::decode(batch.protobuf).expect("export");
                self.0.push((offset, assign(export, &tenant)));
            }
            Ok(())
        }
    }
    let mut wal = Wal::open(node.wal()).expect("reopen");
    let mut collect = Collect(Vec::new());
    wal.replay(&mut collect).expect("replay");
    collect.0
}

/// Every mined row in the Parquet files under `root`.
fn rows(root: &Path) -> Vec<MinedRecord> {
    crate::rfc0052_barrier_support::parquet_files(root)
        .iter()
        .flat_map(|path| {
            Reader::open_file(path)
                .expect("open_file")
                .read_all()
                .expect("read_all")
        })
        .collect()
}

/// Each record's stamp, sorted.
fn stamps(records: &[MinedRecord]) -> Vec<u64> {
    let mut stamps: Vec<u64> = records.iter().map(|r| r.time_unix_nano).collect();
    stamps.sort_unstable();
    stamps
}
