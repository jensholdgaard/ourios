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

// The shared-`tests/` module shape: each `it` module compiles the whole
// rig and uses only the part its criterion needs.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use ourios_config::MinerConfig;
use ourios_ingester::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use ourios_ingester::barrier::Barrier;
use ourios_ingester::cadence::BarrierEpochs;
use ourios_ingester::encode_pool::EncodePool;
use ourios_ingester::publish::PublishCoordinator;
use ourios_ingester::receiver::{
    CommitCoordinator, IngestPipeline, Journal, ReceiveError, SharedPipeline,
};
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::Store;
use ourios_wal::{
    HousekeepingProgress, PassId, ReclaimError, ReclaimOutcome, ReclaimPlan, ReclaimState,
    RotationKind, SnapshotHorizons, Wal, WalConfig, WalOffset,
};

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

/// How to build a rig. A value rather than six parameters: the flush
/// policy, the barrier's ceiling and the rotation hook are independent
/// dials and most legs move one of them.
pub struct RigSpec {
    pub flush: FlushConfig,
    pub workers: usize,
    pub wal: WalConfig,
    /// Replaces the sink's audit-flush barrier, which is the seam an
    /// encode worker really runs inside `emit_concurrent`.
    pub poison: Option<Box<dyn FnMut() -> bool + Send>>,
    /// The barrier's coalescing ceiling (§3.1 uses the sink's own).
    pub ceiling_bytes: usize,
    /// Install the production capture-only rotation hook, so a segment
    /// change inside `ingest` hands a cut to the barrier.
    pub rotation_capture: bool,
    /// Hold the data store's PUTs at this gate — the publisher's write.
    pub held_puts: Option<Gate>,
    /// Hold the audit store's PUTs at this gate, and wire the record
    /// sink's inline barrier the way the receiver does (`settled`).
    pub held_audit_puts: Option<Gate>,
    /// Panics armed on the journal the coordinator owns.
    pub journal_faults: Option<Arc<JournalFaults>>,
}

impl RigSpec {
    pub fn new(wal: WalConfig) -> Self {
        Self {
            flush: never_flush(),
            workers: 2,
            wal,
            poison: None,
            ceiling_bytes: usize::MAX,
            rotation_capture: false,
            held_puts: None,
            held_audit_puts: None,
            journal_faults: None,
        }
    }
}

impl BarrierRig {
    /// A rig under `tmp` with the default never-flush policy.
    pub fn new(tmp: &Path) -> Self {
        Self::build(tmp, RigSpec::new(wal_config(&tmp.join("wal"))))
    }

    /// A rig whose rotation hook is the production capture-only one, with
    /// an explicit barrier ceiling — the rotation-capture legs need both.
    pub fn with_rotation_capture(tmp: &Path, wal: WalConfig, ceiling_bytes: usize) -> Self {
        Self::build(
            tmp,
            RigSpec {
                ceiling_bytes,
                rotation_capture: true,
                ..RigSpec::new(wal)
            },
        )
    }

    /// A rig whose encode worker panics on its first emit: the sink's
    /// inline audit barrier is the seam a worker really runs inside
    /// `emit_concurrent`, and a one-byte size target reaches it on every
    /// record. Everything upstream — the WAL append, its fsync, the ack,
    /// the miner — is the production path, so the batch is genuinely
    /// acknowledged and genuinely replayable.
    pub fn with_panicking_encode(tmp: &Path) -> Self {
        Self::build(
            tmp,
            RigSpec {
                flush: FlushConfig {
                    target_bytes: 1,
                    max_buffer_age: Duration::from_secs(86_400),
                    ceiling_bytes: usize::MAX,
                },
                workers: 1,
                poison: Some(Box::new(|| panic!("injected encode-worker panic"))),
                ..RigSpec::new(wal_config(&tmp.join("wal")))
            },
        )
    }

    /// A rig with an explicit flush policy, worker count and WAL config
    /// — the coalescing and idle-rotation legs need all three.
    pub fn with(tmp: &Path, flush: FlushConfig, workers: usize, wal: WalConfig) -> Self {
        Self::build(
            tmp,
            RigSpec {
                flush,
                workers,
                ..RigSpec::new(wal)
            },
        )
    }

    /// A rig whose every record crosses the size target, with the data
    /// store's PUTs held at `gate` — the publisher's write, reached with
    /// no sink lock held.
    pub fn with_held_puts(tmp: &Path, gate: &Gate) -> Self {
        Self::build(
            tmp,
            RigSpec {
                flush: FlushConfig {
                    target_bytes: 1,
                    max_buffer_age: Duration::from_secs(86_400),
                    ceiling_bytes: usize::MAX,
                },
                held_puts: Some(gate.clone()),
                ..RigSpec::new(wal_config(&tmp.join("wal")))
            },
        )
    }

    /// A rig wired like the receiver — the record sink's inline barrier
    /// is the audit sink's `settled`, which does no store I/O — with every
    /// record crossing the size target and the audit store's PUTs held at
    /// `gate`.
    pub fn with_held_audit_puts(tmp: &Path, gate: &Gate) -> Self {
        Self::build(
            tmp,
            RigSpec {
                flush: FlushConfig {
                    target_bytes: 1,
                    max_buffer_age: Duration::from_secs(86_400),
                    ceiling_bytes: usize::MAX,
                },
                held_audit_puts: Some(gate.clone()),
                ..RigSpec::new(wal_config(&tmp.join("wal")))
            },
        )
    }

    /// A rig whose single encode worker is **held** inside the sink's
    /// inline audit barrier until the returned handle releases it — the
    /// same seam `with_panicking_encode` panics in, and the same
    /// one-byte size target that reaches it on every record.
    ///
    /// The hold is what makes "an encode is still pending" an observed
    /// state rather than a hoped-for interleaving: a leg that only
    /// ingests and sleeps cannot tell a barrier that waits for the
    /// encode phase from one that does not.
    ///
    /// The held barrier reports success without flushing the audit sink.
    /// The legs that use it assert on the checkpoint, and an audit flush
    /// under the hold would only add a second lock to reason about.
    pub fn with_held_encode(tmp: &Path) -> (Self, HeldEncode) {
        let entered = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&entered);
        let gate = Arc::clone(&release);
        let rig = Self::build(
            tmp,
            RigSpec {
                flush: FlushConfig {
                    target_bytes: 1,
                    max_buffer_age: Duration::from_secs(86_400),
                    ceiling_bytes: usize::MAX,
                },
                workers: 1,
                poison: Some(Box::new(move || {
                    seen.fetch_add(1, Ordering::AcqRel);
                    while !gate.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                    true
                })),
                ..RigSpec::new(wal_config(&tmp.join("wal")))
            },
        );
        (rig, HeldEncode { entered, release })
    }

    /// A rig built from an explicit spec.
    pub fn build(tmp: &Path, spec: RigSpec) -> Self {
        let RigSpec {
            flush,
            workers,
            wal,
            poison,
            ceiling_bytes,
            rotation_capture,
            held_puts: put_gate,
            held_audit_puts: audit_gate,
            journal_faults,
        } = spec;
        let wal_root = wal.root.clone();
        let data_root = tmp.join("data");
        let audit_root = tmp.join("audit");
        let snapshots_root = wal_root.join("snapshots");
        for dir in [&wal_root, &data_root, &audit_root] {
            std::fs::create_dir_all(dir).expect("rig directory");
        }

        let journal = Wal::open(wal).expect("open WAL");
        let (audit, poison) = rig_audit(&audit_root, audit_gate.as_ref(), poison);
        let sink = rig_sink(&data_root, flush, &audit, poison, put_gate.as_ref());
        let miner = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(audit.clone()))
            .with_record_sink(Box::new(sink.clone()));

        let journal: Box<dyn Journal> = match journal_faults {
            Some(faults) => Box::new(FaultyJournal {
                wal: journal,
                faults,
            }),
            None => Box::new(journal),
        };
        let commits = CommitCoordinator::new(
            journal,
            Duration::from_millis(20),
            ourios_wal::MIN_SEGMENT_SIZE_BYTES,
        );
        // The barrier is built before the pipeline, exactly as `serve`
        // does it: the capture-only rotation hook the pipeline installs
        // holds the barrier.
        let publish = PublishCoordinator::new(sink.clone(), audit.clone());
        let barrier = Arc::new(Barrier::new(
            publish.clone(),
            Arc::clone(&commits),
            snapshots_root.clone(),
            ceiling_bytes,
        ));
        let mut building = IngestPipeline::new(Arc::clone(&commits), miner)
            .with_encode_pool(EncodePool::with_publisher(publish.publisher(), workers));
        if rotation_capture {
            let hook_barrier = Arc::clone(&barrier);
            building = building.with_rotation_hook(Box::new(move |miner, mark| {
                hook_barrier.capture_rotation(miner, mark);
            }));
        }
        let pipeline: SharedPipeline = Arc::new(building);
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

/// The rig's audit sink, with its PUTs held at `gate` when given — and
/// then the record sink's inline barrier wired the way the receiver
/// wires it (`settled`) in place of `poison`.
fn rig_audit(
    audit_root: &Path,
    gate: Option<&Gate>,
    poison: Option<Box<dyn FnMut() -> bool + Send>>,
) -> (
    SharedParquetAuditSink,
    Option<Box<dyn FnMut() -> bool + Send>>,
) {
    let mut store = Store::local(audit_root).expect("audit store");
    if let Some(gate) = gate {
        store = held_puts(store, gate);
    }
    let audit = SharedParquetAuditSink::new(BufferingAuditSink::new(store, 100_000));
    let barrier = match gate {
        Some(_) => {
            let settled = audit.clone();
            Some(Box::new(move || settled.settled()) as Box<dyn FnMut() -> bool + Send>)
        }
        None => poison,
    };
    (audit, barrier)
}

/// The rig's record sink: its inline audit barrier is `poison` when given,
/// else a flush of `audit`, and its PUTs are held at `put_gate` when given.
fn rig_sink(
    data_root: &Path,
    flush: FlushConfig,
    audit: &SharedParquetAuditSink,
    poison: Option<Box<dyn FnMut() -> bool + Send>>,
    put_gate: Option<&Gate>,
) -> SharedParquetSink {
    let barrier_audit = audit.clone();
    let audit_barrier = poison.unwrap_or_else(|| {
        Box::new(move || barrier_audit.flush()) as Box<dyn FnMut() -> bool + Send>
    });
    let mut data_store = Store::local(data_root).expect("data store");
    if let Some(gate) = put_gate {
        data_store = held_puts(data_store, gate);
    }
    SharedParquetSink::new(
        ParquetRecordSink::new(data_store, flush).with_audit_barrier(audit_barrier),
    )
}

/// The handle on [`BarrierRig::with_held_encode`]'s held worker.
pub struct HeldEncode {
    entered: Arc<AtomicUsize>,
    release: Arc<AtomicBool>,
}

impl HeldEncode {
    /// Block until a worker is inside the emit — the point from which
    /// the pipeline genuinely has an unfinished encode.
    pub fn await_worker_inside_the_emit(&self) {
        while self.entered.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
    }

    pub fn release(&self) {
        self.release.store(true, Ordering::Release);
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

/// A point a production seam waits at until the test opens it.
#[derive(Clone, Default)]
pub struct Gate {
    entered: Arc<AtomicUsize>,
    open: Arc<AtomicBool>,
    /// How many calls pass before the gate starts holding.
    free: usize,
}

impl Gate {
    /// A gate that lets the first `calls` through and holds the rest.
    pub fn letting_through(calls: usize) -> Self {
        Self {
            free: calls,
            ..Self::default()
        }
    }

    pub fn wait_here(&self) {
        if self.entered.fetch_add(1, Ordering::AcqRel) < self.free {
            return;
        }
        while !self.open.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
    }

    /// Block until `calls` calls have reached the gate — failing, not
    /// hanging, when a regression means they never will.
    pub fn await_entered(&self, calls: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while self.entered.load(Ordering::Acquire) < calls {
            assert!(
                std::time::Instant::now() < deadline,
                "{calls} call(s) never reached the gate",
            );
            std::thread::yield_now();
        }
    }

    pub fn open(&self) {
        self.open.store(true, Ordering::Release);
    }

    /// Open the gate when the returned value drops — so a failing
    /// assertion releases whatever is held rather than hanging the
    /// teardown that joins it. Bind it after the pool it releases.
    #[must_use]
    pub fn opened_on_drop(&self) -> OpenOnDrop {
        OpenOnDrop(self.clone())
    }
}

/// See [`Gate::opened_on_drop`].
pub struct OpenOnDrop(Gate);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// `store` with every PUT held at `gate` — the object-store call the
/// publisher's write waits on, reached with no sink lock held.
pub fn held_puts(store: Store, gate: &Gate) -> Store {
    let gate = gate.clone();
    store.wrap_backend(move |inner| Arc::new(HeldStore { inner, gate }))
}

struct HeldStore {
    inner: Arc<dyn object_store::ObjectStore>,
    gate: Gate,
}

impl std::fmt::Debug for HeldStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HeldStore({})", self.inner)
    }
}

impl std::fmt::Display for HeldStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HeldStore({})", self.inner)
    }
}

#[async_trait::async_trait]
impl object_store::ObjectStore for HeldStore {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        let gate = self.gate.clone();
        tokio::task::spawn_blocking(move || gate.wait_here())
            .await
            .expect("the gate wait does not panic");
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &object_store::path::Path,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &object_store::path::Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.inner.get_opts(location, options).await
    }

    async fn get_ranges(
        &self,
        location: &object_store::path::Path,
        ranges: &[std::ops::Range<u64>],
    ) -> object_store::Result<Vec<bytes::Bytes>> {
        self.inner.get_ranges(location, ranges).await
    }

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<
            'static,
            object_store::Result<object_store::path::Path>,
        >,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// One-shot panics on the journal, each at a seam the cadences reach
/// with no batch guard held.
#[derive(Default)]
pub struct JournalFaults {
    /// The barrier's idle-rotation age check, inside its capture.
    pub panic_on_age_check: AtomicBool,
    /// Housekeeping's ledger half, **after** the WAL has taken its plan:
    /// the entries are marked reclaiming and nothing commits them.
    pub panic_after_prepare: AtomicBool,
}

/// A real `Wal` behind the journal seam, with [`JournalFaults`] armed.
struct FaultyJournal {
    wal: Wal,
    faults: Arc<JournalFaults>,
}

impl Journal for FaultyJournal {
    fn append_batch(&mut self, payload: &[u8]) -> Result<WalOffset, ReceiveError> {
        Journal::append_batch(&mut self.wal, payload)
    }

    fn sync(&mut self) -> Result<WalOffset, ReceiveError> {
        Journal::sync(&mut self.wal)
    }

    fn unflushed_bytes(&self) -> u64 {
        Journal::unflushed_bytes(&self.wal)
    }

    fn checkpoint(&mut self, durable_to: WalOffset) -> Result<(), ReclaimError> {
        Journal::checkpoint(&mut self.wal, durable_to)
    }

    fn last_checkpoint(&self) -> Option<WalOffset> {
        Journal::last_checkpoint(&self.wal)
    }

    fn housekeeping_prepare(
        &mut self,
        horizons: &SnapshotHorizons,
        max_unlinks: usize,
    ) -> Result<ReclaimPlan, ReclaimError> {
        let plan = Journal::housekeeping_prepare(&mut self.wal, horizons, max_unlinks);
        assert!(
            !self
                .faults
                .panic_after_prepare
                .swap(false, Ordering::AcqRel),
            "injected housekeeping panic after the plan was taken"
        );
        plan
    }

    fn housekeeping_commit(
        &mut self,
        pass: PassId,
        outcome: ReclaimOutcome,
    ) -> Result<HousekeepingProgress, ReclaimError> {
        Journal::housekeeping_commit(&mut self.wal, pass, outcome)
    }

    fn rotate(&mut self, kind: RotationKind) -> Result<(), ReceiveError> {
        Journal::rotate(&mut self.wal, kind)
    }

    fn segment_age_exceeded(&self) -> bool {
        assert!(
            !self.faults.panic_on_age_check.swap(false, Ordering::AcqRel),
            "injected barrier-tick panic outside any batch guard"
        );
        Journal::segment_age_exceeded(&self.wal)
    }

    fn owes_rotation_fsync(&self) -> bool {
        Journal::owes_rotation_fsync(&self.wal)
    }

    fn reclaim_state(&self) -> ReclaimState {
        Journal::reclaim_state(&self.wal)
    }

    fn rotation_state(&self) -> ourios_wal::RotationState {
        Journal::rotation_state(&self.wal)
    }
}
