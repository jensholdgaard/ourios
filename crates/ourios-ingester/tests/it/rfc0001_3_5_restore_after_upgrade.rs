//! RFC 0001 §3.5.3 through the production paths (issue #892): a snapshot
//! the node itself wrote — by a barrier cut, at the post-recovery point
//! or at shutdown — restores on the next start. The workload is two
//! tenants of string lines and structured events with `event_name`, wide
//! enough that a prefix node fills and RFC 0023 §3.1 routes further lines
//! through its wildcard child, where they widen at a path position.
//! See `docs/rfcs/0001-template-miner.md` §3.5 and §6.9.

use std::path::Path;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{
    AnyValue, InstrumentationScope, KeyValue, KeyValueList,
};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use ourios_config::MinerConfig;
use ourios_core::tenant::TenantId;
use ourios_ingester::barrier::CutOutcome;
use ourios_ingester::receiver::IngestPipeline;
use ourios_ingester::recovery::{self, DiscardReason, RecoveryReport, SnapshotFate};
use ourios_miner::cluster::MinerCluster;
use ourios_wal::{FrameKind, TenantBatch, Wal, WalOffset};
use prost::Message;

use crate::ingest_support::{coordinator, string_value, wal_config};
use crate::rfc0052_17_legacy_snapshot_marks::{
    downgrade_segments, write_legacy_checkpoint, write_v1_snapshot,
};
use crate::rfc0052_barrier_support::BarrierRig;

const TENANTS: [&str; 2] = ["eq-perses", "nocturnal"];

/// Lines per round: five rounds put 200 distinct names under one prefix
/// node, past the default 100 keyed children.
const NAMES_PER_ROUND: usize = 40;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_barrier_installed_snapshot_restores_and_so_does_the_post_recovery_one() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = BarrierRig::new(tmp.path());
    for round in 0..5 {
        for tenant in TENANTS {
            rig.pipeline
                .ingest(workload(tenant, round), TenantId::new(tenant))
                .await
                .expect("the batch acks");
        }
    }
    assert_eq!(rig.barrier.tick(&rig.pipeline, false), CutOutcome::Stamped);
    let (wal_root, snapshots_root) = (rig.wal_root.clone(), rig.snapshots_root.clone());
    drop(rig);

    let (report, miner) = start(&wal_root);
    assert_every_tenant(&report, |fate| matches!(fate, SnapshotFate::Restored(_)));

    recovery::write_folded_snapshots(&snapshots_root, &miner).expect("post-recovery write");
    drop(miner);
    let (report, _) = start(&wal_root);
    assert_every_tenant(&report, |fate| matches!(fate, SnapshotFate::Restored(_)));
}

/// The issue's sequence: a version-1 root's first start discards and
/// full-replays, writes at version 2, ingests more and writes again at
/// shutdown; the next start restores what that start wrote.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upgraded_root_restores_what_its_first_start_wrote() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path();
    legacy_root(root);
    let snapshots_root = root.join("snapshots");

    let (report, miner) = start(root);
    assert_every_tenant(&report, |fate| {
        *fate == SnapshotFate::Discarded(DiscardReason::UnknownVersion(1))
    });
    recovery::write_folded_snapshots(&snapshots_root, &miner).expect("post-recovery write");

    let wal = Wal::open(wal_config(root)).expect("reopen for serving");
    let pipeline = IngestPipeline::new(coordinator(Box::new(wal)), miner);
    for round in 3..5 {
        for tenant in TENANTS {
            pipeline
                .ingest(workload(tenant, round), TenantId::new(tenant))
                .await
                .expect("the batch acks");
        }
    }
    pipeline
        .with_miner(|miner| recovery::write_folded_snapshots(&snapshots_root, miner))
        .expect("shutdown write");
    drop(pipeline);

    let (report, _) = start(root);
    assert_every_tenant(&report, |fate| matches!(fate, SnapshotFate::Restored(_)));
}

/// Startup recovery over `wal_root` into a fresh miner.
fn start(wal_root: &Path) -> (RecoveryReport, MinerCluster) {
    let mut wal = Wal::open(wal_config(wal_root)).expect("open WAL");
    let mut miner = MinerCluster::new(MinerConfig::default());
    let report =
        recovery::recover(&mut wal, &wal_root.join("snapshots"), &mut miner).expect("recover");
    (report, miner)
}

fn assert_every_tenant(report: &RecoveryReport, expected: impl Fn(&SnapshotFate) -> bool) {
    let found: Vec<&str> = report
        .tenants
        .iter()
        .map(|t| t.tenant_id.as_str())
        .collect();
    assert_eq!(found, TENANTS, "one artefact per tenant: {report:?}");
    for tenant in &report.tenants {
        assert!(
            expected(&tenant.fate),
            "{:?}: {:?}",
            tenant.tenant_id,
            tenant.fate
        );
    }
}

/// Three rounds of both tenants in a pre-RFC 0052 root, each tenant's
/// version-1 mark at its own first frame.
fn legacy_root(root: &Path) {
    let mut wal = Wal::open(wal_config(root)).expect("open");
    let mut firsts: Vec<(&str, WalOffset)> = Vec::new();
    let mut last = None;
    for round in 0..3 {
        for tenant in TENANTS {
            let frame = TenantBatch::encode(tenant, &workload(tenant, round).encode_to_vec())
                .expect("frame");
            let offset = wal
                .append(FrameKind::TenantOtlpBatch, &frame)
                .expect("append");
            wal.sync().expect("sync");
            if round == 0 {
                firsts.push((tenant, offset));
            }
            last = Some(offset);
        }
    }
    drop(wal);
    std::fs::remove_file(root.join("RECLAIM")).expect("a pre-RFC root has no record");
    downgrade_segments(root);
    write_legacy_checkpoint(root, last.expect("frames"));
    for (tenant, first) in firsts {
        write_v1_snapshot(root, tenant, Some(first));
    }
}

/// One export for `tenant`: string lines that fill the `character`
/// prefix node across rounds, numeric lines, and structured events keyed
/// by `event_name` under two scopes and several severities.
fn workload(tenant: &str, round: usize) -> ExportLogsServiceRequest {
    let names = round * NAMES_PER_ROUND..(round + 1) * NAMES_PER_ROUND;
    let lines = names.clone().flat_map(|i| {
        [
            string_line(&format!("character Name{i} entered zone Freeport"), 9),
            string_line(&format!("player {i} looted item {}", i * 7), 13),
        ]
    });
    let events = names.map(|i| {
        let (event, severity) = match i % 3 {
            0 => ("everquest.character.profile", 9),
            1 => ("everquest.zone.entered", 9),
            _ => ("everquest.character.profile", 17),
        };
        structured_event(event, severity, i)
    });
    let group = ResourceLogs {
        resource: Some(opentelemetry_proto::tonic::resource::v1::Resource {
            attributes: vec![kv("service.name", string_value(tenant))],
            ..Default::default()
        }),
        scope_logs: vec![
            scope("eq.chat", lines.collect()),
            scope("eq.profile", events.collect()),
        ],
        ..Default::default()
    };
    ExportLogsServiceRequest {
        resource_logs: vec![group],
    }
}

fn scope(name: &str, log_records: Vec<LogRecord>) -> ScopeLogs {
    ScopeLogs {
        scope: Some(InstrumentationScope {
            name: name.to_owned(),
            ..Default::default()
        }),
        log_records,
        ..Default::default()
    }
}

fn string_line(body: &str, severity: i32) -> LogRecord {
    LogRecord {
        severity_number: severity,
        body: Some(string_value(body)),
        ..Default::default()
    }
}

fn structured_event(event: &str, severity: i32, level: usize) -> LogRecord {
    let level = i64::try_from(level).expect("small level");
    LogRecord {
        severity_number: severity,
        event_name: event.to_owned(),
        body: Some(AnyValue {
            value: Some(Value::KvlistValue(KeyValueList {
                values: vec![kv(
                    "level",
                    AnyValue {
                        value: Some(Value::IntValue(level)),
                    },
                )],
            })),
        }),
        ..Default::default()
    }
}

fn kv(key: &str, value: AnyValue) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(value),
        ..Default::default()
    }
}
