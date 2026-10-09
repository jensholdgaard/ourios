//! The compaction daemon (RFC 0009 §3.2): the [`Compactor`] tick loop and
//! one sweep as it runs it — the blocking pass, which records each
//! committed partition's metrics and audit event as it commits, then the
//! async graph phase (RFC 0047 §3.3).

// The parent scope is this module's import surface, as for the other
// compactor submodules.
#[allow(clippy::wildcard_imports)]
use super::*;

/// What a sweep runs against: the store, the candidate policy, the RFC 0022
/// promoted attribute set rewrites re-project under (§3.2/§3.4), and, with
/// the graph configured, its RFC 0047 §3.3 emitter.
#[derive(Debug, Clone)]
pub struct SweepTarget {
    /// The store to sweep.
    pub store: Store,
    /// Which sealed partitions are candidates.
    pub policy: CompactionPolicy,
    /// The promoted attribute set consolidated files re-project under.
    pub promoted: PromotedAttributes,
    /// The graph emitter fed from every row the sweep rewrites.
    #[cfg(feature = "openfga")]
    pub emitter: Option<Arc<GraphEmitter>>,
}

impl SweepTarget {
    /// Sweep `store` under `policy`, re-projecting under `promoted`, with no
    /// graph.
    #[must_use]
    pub fn new(store: Store, policy: CompactionPolicy, promoted: PromotedAttributes) -> Self {
        Self {
            store,
            policy,
            promoted,
            #[cfg(feature = "openfga")]
            emitter: None,
        }
    }

    /// Feed the RFC 0047 §3.3 graph from every row the sweep rewrites, and
    /// complete §3.6 erasures by deleting the conversation's tuples after
    /// the rewrite.
    #[cfg(feature = "openfga")]
    #[must_use]
    pub fn with_emitter(mut self, emitter: Arc<GraphEmitter>) -> Self {
        self.emitter = Some(emitter);
        self
    }
}

/// Background compaction daemon (RFC 0009 §3.2): sweeps the store on a
/// fixed cadence. Hosted in the ingester role so it never lands on the
/// ack-latency hot path.
pub struct Compactor {
    /// What each sweep runs against. The promoted set defaults to the
    /// implicit `service.name`-only set
    /// ([`Self::with_promoted_attributes`]).
    target: SweepTarget,
    interval: Duration,
    /// Where committed-compaction audit events go (RFC 0009 §3.6).
    /// Defaults to [`NoOpAuditSink`]; set via [`Self::with_audit_sink`]
    /// (the WAL-backed sink replaces it once `ourios-wal` lands).
    audit_sink: Box<dyn AuditSink>,
}

impl std::fmt::Debug for Compactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `AuditSink` is not `Debug`; name it without its contents.
        f.debug_struct("Compactor")
            .field("target", &self.target)
            .field("interval", &self.interval)
            .field("audit_sink", &"Box<dyn AuditSink>")
            .finish()
    }
}

impl Compactor {
    /// A compactor sweeping `store` every `interval` under `policy`,
    /// dropping audit events ([`NoOpAuditSink`]) until a sink is set via
    /// [`Self::with_audit_sink`]. The server builds the [`Store`] from the
    /// resolved [`ourios_parquet::StoreConfig`] (RFC 0019), so the same
    /// compactor targets the local filesystem or an S3 bucket.
    #[must_use]
    pub fn new(store: Store, policy: CompactionPolicy, interval: Duration) -> Self {
        Self {
            target: SweepTarget::new(store, policy, PromotedAttributes::default()),
            interval,
            audit_sink: Box::new(NoOpAuditSink::new()),
        }
    }

    /// Feed the RFC 0047 §3.3 graph from every row the sweep rewrites, and
    /// complete §3.6 erasures by deleting the conversation's tuples after
    /// the rewrite.
    #[cfg(feature = "openfga")]
    #[must_use]
    pub fn with_graph_emitter(mut self, emitter: Arc<GraphEmitter>) -> Self {
        self.target = self.target.with_emitter(emitter);
        self
    }

    /// Set the RFC 0022 promoted attribute set consolidated files re-project
    /// under (`storage.promoted_attributes`, §3.2/§3.4).
    #[must_use]
    pub fn with_promoted_attributes(mut self, promoted: PromotedAttributes) -> Self {
        self.target.promoted = promoted;
        self
    }

    /// Route committed-compaction audit events to `sink`.
    #[must_use]
    pub fn with_audit_sink(mut self, sink: Box<dyn AuditSink>) -> Self {
        self.audit_sink = sink;
        self
    }

    /// Run sweeps forever, one per `interval` tick. Each sweep runs on
    /// the blocking pool (compaction is blocking I/O) as of the current
    /// wall clock; its [`SweepReport`]/[`IngestError`] result is handed
    /// to `on_sweep` for logging — so one failing sweep is observed,
    /// not fatal, and the loop keeps ticking. RFC 0009 §3.6 metrics are
    /// recorded via the `ourios.compaction` meter (instruments built and
    /// seeded once here, before the loop): the partition, file, row and IO
    /// counters as each partition commits, the sweep outcome and backlog
    /// once the sweep ends. Does not return.
    ///
    /// # Panics
    ///
    /// Panics only if a sweep task itself panics — `run_sweep` returns
    /// errors rather than panicking, so this signals a bug, surfaced
    /// loudly rather than silently stalling the daemon.
    pub async fn run<F>(self, mut on_sweep: F)
    where
        F: FnMut(Result<SweepReport, IngestError>),
    {
        let Self {
            target,
            interval,
            mut audit_sink,
        } = self;
        // Built (and zero-seeded) once, before the loop, so the metric
        // set is visible to the exporter even before the first sweep.
        let metrics = Arc::new(CompactionMetrics::new());
        let mut ticker = tokio::time::interval(interval);
        // A maintenance sweep that overruns `interval` must not make
        // the next ticks fire back-to-back (the default `Burst`) —
        // that would pile sustained compaction load after any slow
        // pass. `Delay` keeps a full `interval` gap between sweeps.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let (result, elapsed, sink) =
                sweep_recorded(target.clone(), audit_sink, Some(Arc::clone(&metrics))).await;
            audit_sink = sink;
            metrics.record_sweep_outcome(&result, elapsed);
            on_sweep(result);
        }
    }
}

/// One full sweep as the daemon runs it: the blocking pass (consolidation,
/// erasure rewrites) on the blocking pool, emitting each committed
/// partition's compaction audit event at commit time — best-effort: a
/// crash between a commit and its emit, or a sink that suppresses an
/// error, can still drop that one event; then, with
/// an emitter, the async graph phase (RFC 0047 §3.3: write the tuples the
/// pass derived; delete the tuples of every erasure whose rows are gone;
/// then, back on the blocking pool, the `conversation_erased` audit event
/// and the marker removal). Returns the report, the wall-clock spent, and
/// the audit sink handed back. Runs the same way whether called by
/// [`Compactor::run`] or a test.
///
/// # Panics
///
/// If a blocking task panics — `run_sweep` returns errors rather than
/// panicking, so this signals a bug, surfaced loudly rather than silently
/// stalling the daemon.
pub async fn sweep_once(
    target: SweepTarget,
    audit_sink: Box<dyn AuditSink>,
) -> (
    Result<SweepReport, IngestError>,
    Duration,
    Box<dyn AuditSink>,
) {
    sweep_recorded(target, audit_sink, None).await
}

/// [`sweep_once`], recording each committed partition into `metrics` as it
/// commits.
pub(crate) async fn sweep_recorded(
    target: SweepTarget,
    audit_sink: Box<dyn AuditSink>,
    metrics: Option<Arc<CompactionMetrics>>,
) -> (
    Result<SweepReport, IngestError>,
    Duration,
    Box<dyn AuditSink>,
) {
    let start = Instant::now();
    let blocking_target = target.clone();
    // The audit sink moves into the blocking task and back out: its `emit`
    // performs Parquet `put`s through the store — S3 network I/O
    // (RFC 0019) — so it must run on the blocking pool, never on the async
    // task where slow S3 would stall the runtime.
    #[cfg_attr(not(feature = "openfga"), allow(unused_mut))]
    let (mut result, mut audit_sink, tuples) = tokio::task::spawn_blocking(move || {
        blocking_pass(&blocking_target, audit_sink, metrics.as_deref())
    })
    .await
    .expect("compaction sweep task should not panic");

    #[cfg(feature = "openfga")]
    if let Some(emitter) = &target.emitter {
        let graph = GraphPhase {
            store: &target.store,
            emitter,
        };
        match &mut result {
            Ok(report) => graph.run(report, &mut audit_sink, tuples).await,
            Err(e) => graph.after_failed_sweep(&tuples, e).await,
        }
    }
    #[cfg(not(feature = "openfga"))]
    let GraphTuples = tuples;
    (result, start.elapsed(), audit_sink)
}

/// The sweep's blocking pass: records each committed partition into
/// `metrics` and emits its audit event at commit time, so a crash later in
/// the sweep loses no event for a partition committed before it. The emit
/// is best-effort: a crash inside its own window, between the commit and
/// the sink's write, or a sink that suppresses an error, can still drop
/// that one event.
fn blocking_pass(
    target: &SweepTarget,
    mut audit_sink: Box<dyn AuditSink>,
    metrics: Option<&CompactionMetrics>,
) -> (
    Result<SweepReport, IngestError>,
    Box<dyn AuditSink>,
    GraphTuples,
) {
    let mut on_commit = |commit: &PartitionCommitted<'_>| {
        if let Some(metrics) = metrics {
            metrics.record_partition(commit);
        }
        audit_sink.emit(commit.event.clone());
    };
    let (result, tuples) = sweep_deriving(target, &mut on_commit);
    (result, audit_sink, tuples)
}

/// [`run_sweep_committing`] over `target`, deriving the RFC 0047 §3.3
/// tuples of every row it rewrites (for the graph phase to write after the
/// pass) and matching erasures through the emitter.
#[cfg(feature = "openfga")]
fn sweep_deriving(
    target: &SweepTarget,
    on_commit: &mut CommitObserver<'_>,
) -> (Result<SweepReport, IngestError>, GraphTuples) {
    let mut tuples = GraphTuples::default();
    let emitter = target.emitter.as_deref();
    let mut observe = emitter.map(|emitter| {
        let tuples = &mut tuples;
        move |tenant: &str, rows: &[MinedRecord]| {
            tuples.extend(emitter.derive(tenant, rows));
            tuples.extend(GraphEmitter::tool_tuples(tenant));
        }
    });
    let erasure_match = emitter.map(|emitter| {
        move |record: &MinedRecord, id: &str| emitter.conversation_matches(record, id)
    });
    let mut hooks = SweepHooks {
        observe: observe.as_mut().map(|f| f as &mut SweepObserver<'_>),
        erasure_match: erasure_match.as_ref().map(|f| f as &ErasureMatch<'_>),
    };
    let result = run_sweep_committing(
        &target.store,
        SweepClock::sealed_at(now_unix_nanos()),
        &target.policy,
        &target.promoted,
        &mut hooks,
        on_commit,
    );
    (result, tuples)
}

/// [`run_sweep_committing`] over `target`; without the graph there is
/// nothing to derive.
#[cfg(not(feature = "openfga"))]
fn sweep_deriving(
    target: &SweepTarget,
    on_commit: &mut CommitObserver<'_>,
) -> (Result<SweepReport, IngestError>, GraphTuples) {
    let result = run_sweep_committing(
        &target.store,
        SweepClock::sealed_at(now_unix_nanos()),
        &target.policy,
        &target.promoted,
        &mut SweepHooks::default(),
        on_commit,
    );
    (result, GraphTuples)
}
