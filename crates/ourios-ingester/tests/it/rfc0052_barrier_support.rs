//! The production ingest shape plus RFC 0052 §3.1's barrier, wired the
//! way `ourios-server` wires it: one `Wal` behind the group-commit
//! coordinator, a real record sink and audit sink on local stores, an
//! encode pool, and a [`Barrier`] built from the same coordinator the
//! pipeline holds.
//!
//! The legs drive [`Barrier::tick`] directly rather than waiting out a
//! wall-clock interval: the interval is the server's, the barrier is
//! what the criteria are about, and a test that slept for five minutes
//! would prove nothing a direct call does not.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ourios_config::MinerConfig;
use ourios_ingester::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use ourios_ingester::barrier::Barrier;
use ourios_ingester::cadence::BarrierEpochs;
use ourios_ingester::encode_pool::EncodePool;
use ourios_ingester::publish::PublishCoordinator;
use ourios_ingester::receiver::{CommitCoordinator, IngestPipeline, SharedPipeline};
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::Store;
use ourios_wal::{Wal, WalConfig, WalOffset};

use crate::ingest_support::{request, resource_logs};

/// A whole receiver, minus the listeners.
pub struct BarrierRig {
    pub wal_root: PathBuf,
    pub data_root: PathBuf,
    pub audit_root: PathBuf,
    pub snapshots_root: PathBuf,
    pub pipeline: SharedPipeline,
    pub sink: SharedParquetSink,
    pub audit: SharedParquetAuditSink,
    pub publish: PublishCoordinator,
    pub barrier: Arc<Barrier>,
    pub commits: Arc<CommitCoordinator>,
    pub epochs: Arc<BarrierEpochs>,
}

/// Nothing flushes on its own: every partition stays buffered until a
/// cut drains it, so a leg observes exactly what the barrier did.
pub fn never_flush() -> FlushConfig {
    FlushConfig {
        target_bytes: usize::MAX,
        max_buffer_age: Duration::from_secs(86_400),
        ceiling_bytes: usize::MAX,
    }
}

pub fn wal_config(root: &Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 20,
        segment_size_bytes: ourios_wal::MIN_SEGMENT_SIZE_BYTES,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
        rotation_retry_attempts: ourios_wal::DEFAULT_ROTATION_RETRY_ATTEMPTS,
        macos_full_fsync: false,
    }
}

impl BarrierRig {
    /// A rig under `tmp` with the default never-flush policy.
    pub fn new(tmp: &Path) -> Self {
        Self::with(tmp, never_flush(), 2, wal_config(&tmp.join("wal")))
    }

    /// A rig with an explicit flush policy, worker count and WAL config
    /// — the coalescing and idle-rotation legs need all three.
    pub fn with(tmp: &Path, flush: FlushConfig, workers: usize, wal: WalConfig) -> Self {
        let wal_root = wal.root.clone();
        let data_root = tmp.join("data");
        let audit_root = tmp.join("audit");
        let snapshots_root = wal_root.join("snapshots");
        for dir in [&wal_root, &data_root, &audit_root] {
            std::fs::create_dir_all(dir).expect("rig directory");
        }

        let journal = Wal::open(wal).expect("open WAL");
        let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(
            Store::local(&audit_root).expect("audit store"),
            100_000,
        ));
        let barrier_audit = audit.clone();
        let sink = SharedParquetSink::new(
            ParquetRecordSink::new(Store::local(&data_root).expect("data store"), flush)
                .with_audit_barrier(Box::new(move || barrier_audit.flush())),
        );
        let miner = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(audit.clone()))
            .with_record_sink(Box::new(sink.clone()));

        let commits = CommitCoordinator::new(
            Box::new(journal),
            Duration::from_millis(20),
            ourios_wal::MIN_SEGMENT_SIZE_BYTES,
        );
        let pipeline: SharedPipeline = Arc::new(
            IngestPipeline::new(Arc::clone(&commits), miner)
                .with_encode_pool(EncodePool::new(&sink, workers)),
        );
        let publish = PublishCoordinator::new(sink.clone(), audit.clone());
        let barrier = Arc::new(Barrier::new(
            publish.clone(),
            Arc::clone(&commits),
            snapshots_root.clone(),
            usize::MAX,
        ));
        let epochs = sink.epochs();
        Self {
            wal_root,
            data_root,
            audit_root,
            snapshots_root,
            pipeline,
            sink,
            audit,
            publish,
            barrier,
            commits,
            epochs,
        }
    }

    /// Ingest one batch for `tenant` and return the turn's own frame
    /// offset — the mark a cut taken now would use.
    pub async fn ingest(&self, tenant: &str, bodies: &[&str]) -> WalOffset {
        self.pipeline
            .ingest(
                request(vec![resource_logs(tenant, bodies)]),
                ourios_core::tenant::TenantId::new(tenant),
            )
            .await
            .expect("the batch acks");
        self.pipeline.last_durable().expect("a durable mark")
    }

    /// Every `*.parquet` under the data store.
    pub fn data_files(&self) -> Vec<PathBuf> {
        parquet_files(&self.data_root)
    }

    /// Every `*.snap` artefact the barrier has installed.
    pub fn snapshots(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.snapshots_root) else {
            return Vec::new();
        };
        let mut out: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "snap"))
            .collect();
        out.sort();
        out
    }

    /// Break the data store so every partition write fails, while the
    /// audit store stays healthy — the "failing store" arm.
    pub fn sabotage_data_store(&self) {
        std::fs::remove_dir_all(&self.data_root).expect("remove data root");
        std::fs::write(&self.data_root, b"not a directory").expect("sabotage data store");
    }

    /// Break the `CHECKPOINT` sidecar write by putting a directory where
    /// its file belongs: the rename that installs the sidecar then fails,
    /// and so does the fsync that would follow it.
    pub fn sabotage_checkpoint(&self) {
        let path = self.wal_root.join("CHECKPOINT");
        drop(std::fs::remove_file(&path));
        std::fs::create_dir(&path).expect("a directory where the sidecar belongs");
        // A non-empty directory cannot be replaced by a rename on any
        // platform, which is what makes the failure deterministic.
        std::fs::write(path.join("occupied"), b"x").expect("occupy it");
    }
}

pub fn parquet_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "parquet") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}
