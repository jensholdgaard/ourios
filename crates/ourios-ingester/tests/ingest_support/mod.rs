// Shared across the ingest-pipeline integration tests (RFC0003.1 /
// RFC0003.12). Each test binary compiles this module independently and
// uses only the helpers it needs, so `dead_code` here is the expected
// shared-`tests/`-module shape.
#![allow(dead_code)]

use std::path::Path;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_core::config::MinerConfig;
use ourios_miner::cluster::MinerCluster;
use ourios_wal::{FrameKind, FrameSink, RecoveryError, Wal, WalConfig};

use ourios_ingester::receiver::{IngestPipeline, TenantRule};

pub fn wal_config(root: &Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    }
}

/// A pipeline over a fresh `Wal` at `root`, a default `MinerCluster`, and
/// the default `service.name` tenant rule.
pub fn open_pipeline(root: &Path) -> IngestPipeline {
    let wal = Wal::open(wal_config(root)).expect("open WAL");
    let miner = MinerCluster::new(MinerConfig::default());
    IngestPipeline::new(wal, miner, TenantRule::service_name())
}

/// Reopen the WAL at `root` and return its recovered frames. (Call after
/// dropping the pipeline so its writer handle is released.)
pub fn replay_frames(root: &Path) -> Vec<(FrameKind, Vec<u8>)> {
    #[derive(Default)]
    struct CollectingSink(Vec<(FrameKind, Vec<u8>)>);
    impl FrameSink for CollectingSink {
        fn consume(&mut self, kind: FrameKind, payload: &[u8]) -> Result<(), RecoveryError> {
            self.0.push((kind, payload.to_vec()));
            Ok(())
        }
    }
    let mut sink = CollectingSink::default();
    Wal::open(wal_config(root))
        .expect("reopen WAL")
        .replay(&mut sink)
        .expect("replay");
    sink.0
}

pub fn string_value(s: &str) -> AnyValue {
    AnyValue {
        value: Some(Value::StringValue(s.to_owned())),
    }
}

/// A `ResourceLogs` for `service` carrying one string-body record per
/// entry in `bodies` (empty `bodies` → a scope with zero records).
pub fn resource_logs(service: &str, bodies: &[&str]) -> ResourceLogs {
    ResourceLogs {
        resource: Some(Resource {
            attributes: vec![KeyValue {
                key: "service.name".to_owned(),
                value: Some(string_value(service)),
                ..Default::default()
            }],
            ..Default::default()
        }),
        scope_logs: vec![ScopeLogs {
            log_records: bodies
                .iter()
                .map(|body| LogRecord {
                    body: Some(string_value(body)),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// A `ResourceLogs` for `service` with **no** `ScopeLogs` at all.
pub fn resource_logs_without_scopes(service: &str) -> ResourceLogs {
    ResourceLogs {
        resource: Some(Resource {
            attributes: vec![KeyValue {
                key: "service.name".to_owned(),
                value: Some(string_value(service)),
                ..Default::default()
            }],
            ..Default::default()
        }),
        scope_logs: vec![],
        ..Default::default()
    }
}

pub fn request(resource_logs: Vec<ResourceLogs>) -> ExportLogsServiceRequest {
    ExportLogsServiceRequest { resource_logs }
}
