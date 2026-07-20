// Shared across the ingest-pipeline integration tests (RFC0003.1 /
// RFC0003.12). Each test binary compiles this module independently and
// uses only the helpers it needs, so `dead_code` here is the expected
// shared-`tests/`-module shape.
#![allow(dead_code)]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_config::MinerConfig;
use ourios_miner::cluster::MinerCluster;
use ourios_wal::{
    AppendError, FrameKind, FrameSink, RecoveryError, SyncError, Wal, WalConfig, WalOffset,
};

use ourios_ingester::receiver::{
    CommitCoordinator, IngestPipeline, Journal, ReceiveError, TenantRule,
};

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

/// Build a group-commit coordinator over `journal` with the test
/// `wal_config`'s window + segment-fill threshold. A short window keeps
/// the integration tests fast.
pub fn coordinator(journal: Box<dyn Journal>) -> Arc<CommitCoordinator> {
    let config = wal_config(Path::new("."));
    CommitCoordinator::new(
        journal,
        Duration::from_millis(config.batch_window_ms),
        config.segment_size_bytes,
    )
}

/// A pipeline over a fresh `Wal` at `root`, a default `MinerCluster`, and
/// the default `service.name` tenant rule.
pub fn open_pipeline(root: &Path) -> IngestPipeline {
    let wal = Wal::open(wal_config(root)).expect("open WAL");
    let miner = MinerCluster::new(MinerConfig::default());
    IngestPipeline::new(
        coordinator(Box::new(wal)),
        miner,
        TenantRule::service_name(),
    )
}

/// One observed `Journal` call, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalCall {
    Append,
    Sync,
}

/// Shared, ordered log of a spy `Journal`'s calls.
pub type CallLog = Arc<Mutex<Vec<JournalCall>>>;

/// A `Journal` that records each `append_batch`/`sync` call into a shared
/// log (and persists nothing), so a test can assert the WAL-before-ack
/// call sequence (RFC0003.1) and that empty batches touch the WAL not at
/// all (RFC0003.12).
struct SpyJournal {
    log: CallLog,
    byte: u64,
}

impl Journal for SpyJournal {
    fn append_batch(&mut self, _payload: &[u8]) -> Result<(), ReceiveError> {
        self.log.lock().expect("call log").push(JournalCall::Append);
        Ok(())
    }

    fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
        self.log.lock().expect("call log").push(JournalCall::Sync);
        // A synthetic, monotonically-advancing offset: the spy persists
        // nothing, but the coordinator needs a concrete durable mark.
        self.byte += 1;
        Ok(WalOffset {
            segment: uuid::Uuid::from_u128(1),
            byte: self.byte,
        })
    }

    fn unflushed_bytes(&self) -> u64 {
        0
    }
}

/// A pipeline whose `Journal` is a spy recording its calls into `log`
/// (default miner + `service.name` rule).
pub fn spy_pipeline(log: CallLog) -> IngestPipeline {
    let miner = MinerCluster::new(MinerConfig::default());
    IngestPipeline::new(
        coordinator(Box::new(SpyJournal { log, byte: 0 })),
        miner,
        TenantRule::service_name(),
    )
}

/// A `Journal` that appends fine but **fails the fsync** — a transient
/// WAL/storage failure (`ReceiveError::WalSync`). The batch is never acked
/// (§3.4); the transport must report a retryable status (RFC 0018 §3.2).
struct FailingSyncJournal;

impl Journal for FailingSyncJournal {
    fn append_batch(&mut self, _payload: &[u8]) -> Result<(), ReceiveError> {
        Ok(())
    }
    fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
        Err(ReceiveError::WalSync(SyncError::Io {
            op: "fdatasync",
            source: std::io::Error::other("injected fsync failure"),
        }))
    }
    fn unflushed_bytes(&self) -> u64 {
        0
    }
}

/// A pipeline whose fsync always fails — drives the transient-failure
/// (`WalSync`) path for the RFC 0018 §3.2 retryable-status mapping.
pub fn failing_sync_pipeline() -> IngestPipeline {
    let miner = MinerCluster::new(MinerConfig::default());
    IngestPipeline::new(
        coordinator(Box::new(FailingSyncJournal)),
        miner,
        TenantRule::service_name(),
    )
}

/// A `Journal` whose **append** fails with a configurable [`AppendError`] —
/// distinguishes a transient append I/O failure (retryable) from an
/// oversize payload (`TooLarge`, a permanent client error) for the RFC 0018
/// §3.2 mapping.
struct FailingAppendJournal {
    error: fn() -> AppendError,
}

impl Journal for FailingAppendJournal {
    fn append_batch(&mut self, _payload: &[u8]) -> Result<(), ReceiveError> {
        Err(ReceiveError::WalAppend((self.error)()))
    }
    fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
        // Unreachable in practice (the append fails first), but the trait
        // requires it.
        Ok(WalOffset {
            segment: uuid::Uuid::from_u128(1),
            byte: 0,
        })
    }
    fn unflushed_bytes(&self) -> u64 {
        0
    }
}

fn failing_append_pipeline(error: fn() -> AppendError) -> IngestPipeline {
    let miner = MinerCluster::new(MinerConfig::default());
    IngestPipeline::new(
        coordinator(Box::new(FailingAppendJournal { error })),
        miner,
        TenantRule::service_name(),
    )
}

/// A pipeline whose append fails with a transient I/O error
/// (`AppendError::Io`) — the retryable-WAL path (`UNAVAILABLE` / 503).
pub fn failing_append_pipeline_transient() -> IngestPipeline {
    failing_append_pipeline(|| AppendError::Io {
        op: "write",
        source: std::io::Error::other("injected append failure"),
    })
}

/// A pipeline whose append fails with `AppendError::TooLarge` — a permanent
/// client sizing error (`INVALID_ARGUMENT` / 413), never retryable.
pub fn oversize_append_pipeline() -> IngestPipeline {
    failing_append_pipeline(|| AppendError::TooLarge {
        len: 32 * 1024 * 1024,
        limit: 16 * 1024 * 1024,
    })
}

/// Reopen the WAL at `root` and return its recovered frames. (Call after
/// dropping the pipeline so its writer handle is released.)
pub fn replay_frames(root: &Path) -> Vec<(FrameKind, Vec<u8>)> {
    #[derive(Default)]
    struct CollectingSink(Vec<(FrameKind, Vec<u8>)>);
    impl FrameSink for CollectingSink {
        fn consume(
            &mut self,
            _offset: WalOffset,
            kind: FrameKind,
            payload: &[u8],
        ) -> Result<(), RecoveryError> {
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

// ----- HTTP-listener test support (RFC0003.11/.13/.14) -----

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ourios_ingester::receiver::SharedPipeline;
use tower::ServiceExt;

/// The `OtlpBatch` payloads a [`capturing_pipeline`] appended, in order.
pub type Captured = Arc<Mutex<Vec<Vec<u8>>>>;

/// A `Journal` that records appended payloads (and persists nothing,
/// reporting a synthetic durable offset), so a test can recover what the
/// pipeline ingested without a real WAL.
struct CapturingJournal {
    captured: Captured,
    byte: u64,
}

impl Journal for CapturingJournal {
    fn append_batch(&mut self, payload: &[u8]) -> Result<(), ReceiveError> {
        self.captured
            .lock()
            .expect("captured")
            .push(payload.to_vec());
        Ok(())
    }

    fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
        self.byte += 1;
        Ok(WalOffset {
            segment: uuid::Uuid::from_u128(1),
            byte: self.byte,
        })
    }

    fn unflushed_bytes(&self) -> u64 {
        0
    }
}

/// A shared pipeline over a *real* `Wal` at `root` (for concurrency +
/// durability assertions; drop all clones before `replay_frames`).
pub fn shared_wal_pipeline(root: &Path) -> SharedPipeline {
    Arc::new(open_pipeline(root))
}

use ourios_ingester::encode_pool::EncodePool;
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_parquet::Store;
use std::time::Duration as StdDuration;

/// A never-flush [`FlushConfig`]: no size/age/ceiling trigger fires, so
/// emitted records stay observable in the buffer.
pub fn never_flush() -> FlushConfig {
    FlushConfig {
        target_bytes: usize::MAX,
        max_buffer_age: StdDuration::from_secs(86_400),
        ceiling_bytes: usize::MAX,
    }
}

/// The RFC 0035 production ingest shape over a *real* `Wal` at `root`:
/// the miner emits into a shared Parquet sink on a local store at
/// `store_root`, and the pipeline runs the ordered/concurrent split with
/// an encode pool of `workers` threads over that same sink. Returns the
/// sink handle for quiesce-then-inspect assertions.
pub fn pooled_wal_pipeline(
    root: &Path,
    store_root: &Path,
    workers: usize,
) -> (SharedPipeline, SharedParquetSink) {
    std::fs::create_dir_all(store_root).expect("create store root");
    let wal = Wal::open(wal_config(root)).expect("open WAL");
    let sink = SharedParquetSink::new(ParquetRecordSink::new(
        Store::local(store_root).expect("local store"),
        never_flush(),
    ));
    let miner = MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    let pipeline = IngestPipeline::new(
        coordinator(Box::new(wal)),
        miner,
        TenantRule::service_name(),
    )
    .with_encode_pool(EncodePool::new(&sink, workers));
    (Arc::new(pipeline), sink)
}

/// [`capturing_pipeline`] with an RFC 0026 denial audit sink attached.
pub fn capturing_pipeline_with_denial_audit(
    sink: Box<dyn ourios_core::audit::AuditSink + Send>,
) -> (SharedPipeline, Captured) {
    let captured = Captured::default();
    let miner = MinerCluster::new(MinerConfig::default());
    let pipeline = IngestPipeline::new(
        coordinator(Box::new(CapturingJournal {
            captured: captured.clone(),
            byte: 0,
        })),
        miner,
        TenantRule::service_name(),
    )
    .with_denial_audit_sink(sink);
    (Arc::new(pipeline), captured)
}

/// A shared pipeline whose `Journal` captures appended payloads, plus the
/// capture handle.
pub fn capturing_pipeline() -> (SharedPipeline, Captured) {
    let captured = Captured::default();
    let miner = MinerCluster::new(MinerConfig::default());
    let pipeline = IngestPipeline::new(
        coordinator(Box::new(CapturingJournal {
            captured: captured.clone(),
            byte: 0,
        })),
        miner,
        TenantRule::service_name(),
    );
    (Arc::new(pipeline), captured)
}

/// Build a `POST` request with optional `Content-Type`/`Content-Encoding`.
pub fn post_request(
    path: &str,
    content_type: Option<&str>,
    content_encoding: Option<&str>,
    body: Vec<u8>,
) -> Request<Body> {
    let mut builder = Request::builder().method("POST").uri(path);
    if let Some(value) = content_type {
        builder = builder.header(header::CONTENT_TYPE, value);
    }
    if let Some(value) = content_encoding {
        builder = builder.header(header::CONTENT_ENCODING, value);
    }
    builder.body(Body::from(body)).expect("build request")
}

/// Drive `router` with `request` in-process (no socket) and return the
/// response status + body bytes.
pub async fn send(router: Router, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = router.oneshot(request).await.expect("oneshot");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read response body")
        .to_vec();
    (status, bytes)
}

/// gzip-compress `bytes` (for the `Content-Encoding: gzip` arm).
pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).expect("gzip write");
    encoder.finish().expect("gzip finish")
}
