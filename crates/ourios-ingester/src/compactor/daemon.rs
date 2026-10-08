//! The compaction daemon (RFC 0009 §3.2): the [`Compactor`] tick loop and
//! one sweep as it runs it — the blocking pass with per-commit metrics,
//! audit events and graph tuples, then the async graph phase.

// The parent scope is this module's import surface, as for the other
// compactor submodules.
#[allow(clippy::wildcard_imports)]
use super::*;

/// Background compaction daemon (RFC 0009 §3.2): sweeps the store on a
/// fixed cadence. Hosted in the ingester role so it never lands on the
/// ack-latency hot path.
pub struct Compactor {
    store: Store,
    policy: CompactionPolicy,
    interval: Duration,
    /// The RFC 0022 promoted attribute set consolidated files re-project
    /// under (`storage.promoted_attributes`, §3.2/§3.4). Defaults to the
    /// implicit `service.name`-only set; set via
    /// [`Self::with_promoted_attributes`].
    promoted: PromotedAttributes,
    /// Where committed-compaction audit events go (RFC 0009 §3.6).
    /// Defaults to [`NoOpAuditSink`]; set via [`Self::with_audit_sink`]
    /// (the WAL-backed sink replaces it once `ourios-wal` lands).
    audit_sink: Box<dyn AuditSink>,
    /// The RFC 0047 §3.3 graph emitter, when the graph is configured.
    #[cfg(feature = "openfga")]
    emitter: Option<Arc<GraphEmitter>>,
}

impl std::fmt::Debug for Compactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `AuditSink` is not `Debug`; name it without its contents.
        let mut d = f.debug_struct("Compactor");
        d.field("store", &self.store)
            .field("policy", &self.policy)
            .field("interval", &self.interval)
            .field("promoted", &self.promoted)
            .field("audit_sink", &"Box<dyn AuditSink>");
        #[cfg(feature = "openfga")]
        d.field("emitter", &self.emitter);
        d.finish()
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
            store,
            policy,
            interval,
            promoted: PromotedAttributes::default(),
            audit_sink: Box::new(NoOpAuditSink::new()),
            #[cfg(feature = "openfga")]
            emitter: None,
        }
    }

    /// Feed the RFC 0047 §3.3 graph from every row the sweep rewrites, and
    /// complete §3.6 erasures by deleting the conversation's tuples after
    /// the rewrite.
    #[cfg(feature = "openfga")]
    #[must_use]
    pub fn with_graph_emitter(mut self, emitter: Arc<GraphEmitter>) -> Self {
        self.emitter = Some(emitter);
        self
    }

    /// Set the RFC 0022 promoted attribute set consolidated files re-project
    /// under (`storage.promoted_attributes`, §3.2/§3.4).
    #[must_use]
    pub fn with_promoted_attributes(mut self, promoted: PromotedAttributes) -> Self {
        self.promoted = promoted;
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
            store,
            policy,
            interval,
            promoted,
            mut audit_sink,
            #[cfg(feature = "openfga")]
            emitter,
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
            let (result, elapsed, sink) = sweep_recorded(
                store.clone(),
                policy,
                promoted.clone(),
                audit_sink,
                #[cfg(feature = "openfga")]
                emitter.clone(),
                Some(Arc::clone(&metrics)),
            )
            .await;
            audit_sink = sink;
            metrics.record_sweep_outcome(&result, elapsed);
            on_sweep(result);
        }
    }
}

/// One full sweep as the daemon runs it: the blocking pass (consolidation,
/// erasure rewrites) on the blocking pool — each committed partition's
/// compaction audit event, and with an emitter its graph tuples, written
/// right after its manifest commit — then, with an emitter, the async graph
/// phase (write the tuples no commit flushed; delete the tuples of every
/// erasure whose rows are gone; then, back on the blocking pool, the
/// `conversation_erased` audit event and the marker removal). Returns the
/// report, the wall-clock spent, and the audit sink handed back. Runs the
/// same way whether called by [`Compactor::run`] or a test.
///
/// # Panics
///
/// If a blocking task panics — `run_sweep` returns errors rather than
/// panicking, so this signals a bug, surfaced loudly rather than silently
/// stalling the daemon.
pub async fn sweep_once(
    store: Store,
    policy: CompactionPolicy,
    promoted: PromotedAttributes,
    audit_sink: Box<dyn AuditSink>,
    #[cfg(feature = "openfga")] emitter: Option<Arc<GraphEmitter>>,
) -> (
    Result<SweepReport, IngestError>,
    Duration,
    Box<dyn AuditSink>,
) {
    sweep_recorded(
        store,
        policy,
        promoted,
        audit_sink,
        #[cfg(feature = "openfga")]
        emitter,
        None,
    )
    .await
}

/// [`sweep_once`], recording each committed partition into `metrics` as it
/// commits.
pub(crate) async fn sweep_recorded(
    store: Store,
    policy: CompactionPolicy,
    promoted: PromotedAttributes,
    audit_sink: Box<dyn AuditSink>,
    #[cfg(feature = "openfga")] emitter: Option<Arc<GraphEmitter>>,
    metrics: Option<Arc<CompactionMetrics>>,
) -> (
    Result<SweepReport, IngestError>,
    Duration,
    Box<dyn AuditSink>,
) {
    let start = Instant::now();
    #[cfg(feature = "openfga")]
    let blocking_emitter = emitter.clone();
    // The per-commit graph flush drives the emitter's async client from the
    // blocking thread.
    #[cfg(feature = "openfga")]
    let runtime = tokio::runtime::Handle::current();
    let blocking_store = store.clone();
    // `Store` is a cheap `Arc` handle; clone it into the blocking task
    // (compaction is blocking I/O). `policy` is `Copy`. The audit sink moves
    // into the task and back out: its `emit` performs Parquet `put`s through
    // the store — S3 network I/O (RFC 0019) — so it must run on the blocking
    // pool, never on the async task where slow S3 would stall the runtime.
    #[cfg_attr(not(feature = "openfga"), allow(unused_mut))]
    let (mut result, mut audit_sink, tuples) = tokio::task::spawn_blocking(move || {
        let mut audit_sink = audit_sink;
        let tuples: std::cell::RefCell<GraphTuples> = std::cell::RefCell::default();
        #[cfg(feature = "openfga")]
        let mut flushed = GraphFlush::default();
        let result = {
            #[cfg(feature = "openfga")]
            let tuples_ref = &tuples;
            #[cfg(feature = "openfga")]
            let mut observe = blocking_emitter.as_ref().map(|emitter| {
                let emitter = Arc::clone(emitter);
                move |tenant: &str, rows: &[MinedRecord]| {
                    let mut tuples = tuples_ref.borrow_mut();
                    tuples.extend(emitter.derive(tenant, rows));
                    tuples.extend(GraphEmitter::tool_tuples(tenant));
                }
            });
            #[cfg(feature = "openfga")]
            let erasure_match = blocking_emitter.as_ref().map(|emitter| {
                let emitter = Arc::clone(emitter);
                move |record: &MinedRecord, id: &str| emitter.conversation_matches(record, id)
            });
            let mut hooks = SweepHooks {
                #[cfg(feature = "openfga")]
                observe: observe.as_mut().map(|f| f as &mut SweepObserver<'_>),
                #[cfg(feature = "openfga")]
                erasure_match: erasure_match.as_ref().map(|f| f as &ErasureMatch<'_>),
                #[cfg(not(feature = "openfga"))]
                observe: None,
                #[cfg(not(feature = "openfga"))]
                erasure_match: None,
            };
            let mut on_commit = |commit: &PartitionCommitted<'_>| {
                if let Some(metrics) = &metrics {
                    metrics.record_partition(commit);
                }
                audit_sink.emit(commit.event.clone());
                #[cfg(feature = "openfga")]
                if let Some(emitter) = &blocking_emitter {
                    flushed.flush(&runtime, emitter, &tuples);
                }
            };
            run_sweep_committing(
                &blocking_store,
                now_unix_nanos(),
                &policy,
                &promoted,
                &mut hooks,
                &mut on_commit,
            )
        };
        #[cfg(feature = "openfga")]
        let (result, tuples) = flushed.settle(result, tuples.into_inner());
        #[cfg(not(feature = "openfga"))]
        let tuples = tuples.into_inner();
        (result, audit_sink, tuples)
    })
    .await
    .expect("compaction sweep task should not panic");

    #[cfg(feature = "openfga")]
    if let Some(emitter) = emitter.as_ref() {
        match &mut result {
            Ok(report) => graph_phase(&store, emitter, report, &mut audit_sink, tuples).await,
            Err(e) => emit_after_failed_sweep(emitter, &tuples, e).await,
        }
    }
    #[cfg(not(feature = "openfga"))]
    let GraphTuples = tuples;
    (result, start.elapsed(), audit_sink)
}

/// The graph tuples a sweep has written so far, flushed at each partition
/// commit (RFC 0047 §3.3) so a restart mid-sweep loses none of a committed
/// partition's tuples.
#[cfg(feature = "openfga")]
#[derive(Default)]
struct GraphFlush {
    /// Every tuple written this sweep, so a tuple derived again from a later
    /// partition (a tenant's tool tuples, a conversation spanning hours) is
    /// sent once.
    sent: GraphTuples,
    emitted: usize,
    errors: Vec<String>,
    /// A write failed this sweep: the rest wait for the end-of-sweep graph
    /// phase, so an unreachable graph costs one timeout per sweep, not one
    /// per partition.
    failed: bool,
}

#[cfg(feature = "openfga")]
impl GraphFlush {
    /// Write the derived tuples not yet sent. A failed write puts them back
    /// for the end-of-sweep graph phase to retry.
    fn flush(
        &mut self,
        runtime: &tokio::runtime::Handle,
        emitter: &GraphEmitter,
        tuples: &std::cell::RefCell<GraphTuples>,
    ) {
        if self.failed {
            return;
        }
        let mut fresh = std::mem::take(&mut *tuples.borrow_mut());
        fresh.retain(|tuple| !self.sent.contains(tuple));
        if fresh.is_empty() {
            return;
        }
        match runtime.block_on(emitter.emit(&fresh)) {
            Ok(written) => {
                self.emitted += written.tuples;
                self.sent.append(&mut fresh);
            }
            Err(e) => {
                self.errors.push(format!("graph emit: {e}"));
                self.failed = true;
                tuples.borrow_mut().append(&mut fresh);
            }
        }
    }

    /// Fold the per-commit flushes into the sweep's report, returning the
    /// derived tuples still unsent for the graph phase.
    fn settle(
        self,
        mut result: Result<SweepReport, IngestError>,
        mut remaining: GraphTuples,
    ) -> (Result<SweepReport, IngestError>, GraphTuples) {
        remaining.retain(|tuple| !self.sent.contains(tuple));
        if let Ok(report) = &mut result {
            report.graph_tuples_emitted += self.emitted;
            report.errors.extend(self.errors);
        }
        (result, remaining)
    }
}

/// Write the tuples no commit flushed when the sweep itself failed: the
/// partitions it committed before failing are no longer candidates, so no
/// later sweep derives their tuples again.
#[cfg(feature = "openfga")]
async fn emit_after_failed_sweep(
    emitter: &GraphEmitter,
    tuples: &GraphTuples,
    sweep: &IngestError,
) {
    if tuples.is_empty() {
        return;
    }
    if let Err(e) = emitter.emit(tuples).await {
        // The failed sweep's own report never reaches the daemon's logging, so
        // this per-item error is logged here, under the event the daemon uses
        // for a sweep's per-item errors.
        tracing::error!(
            name: ourios_semconv::EVENT_OURIOS_COMPACTION_SWEEP_ERROR,
            "compaction sweep error: graph emit after a failed sweep ({sweep}): {e}"
        );
    }
}

/// The async graph phase of a sweep (RFC 0047 §3.3 / §3.6).
#[cfg(feature = "openfga")]
async fn graph_phase(
    store: &Store,
    emitter: &Arc<GraphEmitter>,
    report: &mut SweepReport,
    audit_sink: &mut Box<dyn AuditSink>,
    tuples: GraphTuples,
) {
    if !tuples.is_empty() {
        match emitter.emit(&tuples).await {
            Ok(written) => report.graph_tuples_emitted += written.tuples,
            Err(e) => report.errors.push(format!("graph emit: {e}")),
        }
    }
    let mut completed: Vec<(usize, AuditEvent)> = Vec::new();
    for (index, outcome) in report.erasures.iter_mut().enumerate() {
        if outcome.phase != ErasurePhase::Tuples {
            continue;
        }
        let request = &outcome.request;
        match emitter
            .erase_conversation(&request.tenant, &request.conversation_id)
            .await
        {
            Ok(deleted) => {
                outcome.tuples_deleted = Some(deleted);
                completed.push((
                    index,
                    AuditEvent {
                        tenant_id: TenantId::new(&request.tenant),
                        timestamp: SystemTime::now(),
                        payload: AuditPayload::ConversationErased {
                            conversation_id: request.conversation_id.clone(),
                            partitions_rewritten: outcome.partitions_rewritten,
                            rows_dropped: outcome.rows_dropped,
                            tuples_deleted: to_u64(deleted),
                        },
                    },
                ));
            }
            Err(e) => report.errors.push(format!(
                "erase {:?} {:?}: graph tuples: {e} — retried next sweep",
                request.tenant, request.conversation_id
            )),
        }
    }
    if completed.is_empty() {
        return;
    }
    // Back on the blocking pool for the audit `put`s and the marker
    // deletes — after the tuples are gone.
    let store = store.clone();
    let markers: Vec<(usize, String, AuditEvent)> = completed
        .into_iter()
        .map(|(index, event)| (index, report.erasures[index].request.marker.clone(), event))
        .collect();
    let mut sink = std::mem::replace(audit_sink, Box::new(NoOpAuditSink::new()));
    let (sink, finished, errors) = tokio::task::spawn_blocking(move || {
        let mut finished = Vec::new();
        let mut errors = Vec::new();
        for (index, marker, event) in markers {
            // The marker removal is the at-most-once transition: only the
            // process that removes it writes the audit event. A marker
            // already gone was finished (and audited) elsewhere; a failed
            // delete leaves the marker in the `tuples` phase — the next
            // sweep repeats the (idempotent) tuple deletion and retries.
            match store.delete_blocking(&marker) {
                Ok(()) => {
                    sink.emit(event);
                    finished.push(index);
                }
                Err(e) if e.is_not_found() => finished.push(index),
                Err(e) => errors.push(format!("erase: remove marker {marker}: {e}")),
            }
        }
        (sink, finished, errors)
    })
    .await
    .expect("erasure completion task should not panic");
    *audit_sink = sink;
    for index in finished {
        let outcome = &mut report.erasures[index];
        outcome.finished = true;
        // RFC 0048 §3.3: completion is observable in the logs too — one
        // structured event per finished erasure.
        // `tuples_deleted` is set on the same path that queued the marker
        // removal; a `None` here would be a regression worth seeing in the
        // log rather than a silent 0 (and never worth a panic — §6.5's
        // no-unwrap rule holds in the sweep).
        let tuples_deleted = outcome
            .tuples_deleted
            .map_or_else(|| "unknown".to_string(), |n| n.to_string());
        tracing::info!(
            name: ourios_semconv::EVENT_OURIOS_COMPACTION_ERASURE_COMPLETED,
            "conversation erasure completed: tenant {:?} conversation {:?}, {} rows dropped, {} tuples deleted",
            outcome.request.tenant,
            outcome.request.conversation_id,
            outcome.rows_dropped,
            tuples_deleted,
        );
    }
    report.errors.extend(errors);
}
