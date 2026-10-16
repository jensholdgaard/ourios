//! Background compaction runner (RFC 0009 §3.2).
//!
//! [`run_sweep`] is one pass over the whole store — for every tenant,
//! select its sealed candidate partitions and consolidate them. It is
//! synchronous (blocking filesystem + Parquet work) and deterministic,
//! so it's the unit the tests exercise. [`Compactor::run`] is the thin
//! daemon: it calls `run_sweep` on a fixed cadence via `spawn_blocking`,
//! records the RFC 0009 §3.6 metrics and audit events as each partition
//! commits ([`crate::metrics::CompactionMetrics`]), and hands each sweep's
//! result to a caller-supplied observer for logging.

#[cfg(feature = "openfga")]
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use ourios_core::audit::{AuditEvent, AuditPayload, AuditSink, NoOpAuditSink};
use ourios_core::record::MinedRecord;
use ourios_core::tenant::TenantId;
use ourios_parquet::{
    Committed, CompactionError, CompactionOutcome, CompactionPolicy, CompactionStats, PartitionKey,
    PlanCounts, PromotedAttributes, RowFilter, RowHooks, RowObserver, Store,
    compact_partition_hooked, gc_orphans, hour_partitions, percent_decode_tenant,
    percent_encode_tenant, plan_candidates_counted,
};

#[cfg(feature = "openfga")]
use crate::graph_emitter::GraphEmitter;
use crate::metrics::CompactionMetrics;

/// The tuples one sweep derives for the graph (RFC 0047 §3.3).
#[cfg(feature = "openfga")]
type GraphTuples = BTreeSet<ourios_serving::openfga::TupleKey>;
/// Nothing to derive without the graph.
#[cfg(not(feature = "openfga"))]
#[derive(Default)]
struct GraphTuples;

/// Failure during a compaction sweep.
#[derive(Debug)]
#[non_exhaustive]
pub enum IngestError {
    /// Planning or consolidating a partition failed.
    Compaction(CompactionError),
    /// Listing the store's tenant keys failed.
    Io {
        op: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // `CompactionError`'s Display already starts with
            // "compaction …", so no prefix here (avoids "compaction:
            // compaction read: …").
            Self::Compaction(e) => write!(f, "{e}"),
            Self::Io { op, path, source } => write!(f, "{op} {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for IngestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Compaction(e) => Some(e),
            Self::Io { source, .. } => Some(source),
        }
    }
}

impl IngestError {
    /// The low-cardinality `error.type` a failed sweep carries.
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::Compaction(e) => compaction_error_type(e),
            Self::Io { .. } => "io",
        }
    }
}

/// The `error.type` of a compaction failure, shared by the sweep and its
/// planning phase.
fn compaction_error_type(error: &CompactionError) -> &'static str {
    match error {
        CompactionError::Read(_) => "read",
        CompactionError::Write(_) => "write",
        CompactionError::Manifest(_) => "manifest",
        CompactionError::Io { .. } => "io",
        _ => "_OTHER",
    }
}

impl From<CompactionError> for IngestError {
    fn from(e: CompactionError) -> Self {
        Self::Compaction(e)
    }
}

/// Summary of one [`run_sweep`] over the store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Tenants whose partitions were scanned.
    pub tenants_scanned: usize,
    /// Partitions actually consolidated (a candidate that wasn't a
    /// no-op).
    pub partitions_compacted: usize,
    /// Total input files merged away across those partitions (the
    /// `files_before` of each consolidated partition) — the H4
    /// small-file signal (RFC 0009 §3.6 `ourios.compaction.files`).
    pub files_compacted: u64,
    /// Total rows rewritten across those partitions.
    pub rows_compacted: u64,
    /// Superseded inputs that couldn't be removed post-commit (orphans
    /// a later sweep/GC reclaims; see `CompactionOutcome.gc_failures`).
    pub gc_failures: usize,
    /// Orphan files (dead inputs / consolidated / `*.tmp` left by a
    /// crashed prior compaction) reclaimed this sweep by `gc_orphans`
    /// (RFC0009.4 — crash safety: orphans are reclaimable on a later
    /// sweep). Counts only candidate partitions visited this sweep.
    pub orphans_reclaimed: u64,
    /// Erasure markers skipped this sweep because their tenant holds a
    /// backfill lock (RFC 0048 §3.4 — the fence; retried next sweep).
    pub erasures_deferred: Vec<String>,
    /// Per-tenant / per-partition failures encountered, formatted for
    /// logging. A sweep is **resilient**: one bad tenant or partition
    /// is recorded here and skipped, never aborting the rest (else a
    /// persistent error would starve every later tenant, since the
    /// daemon just retries the same sweep next tick).
    pub errors: Vec<String>,
    /// One [`AuditPayload::Compaction`] audit event per committed
    /// compaction (RFC 0009 §3.6 / RFC 0005 §3.7). [`sweep_once`] emits
    /// each through its [`AuditSink`] as its partition commits.
    pub compaction_events: Vec<AuditEvent>,
    /// Total input bytes read across the compacted partitions — the
    /// read volume for `ourios.compaction.io` (RFC 0009 §3.6).
    pub bytes_read: u64,
    /// One entry per committed compaction: the consolidated output
    /// file's size, tagged with its tenant. These are the per-tenant
    /// `ourios.storage.parquet.file.size` H4 histogram samples; their
    /// sum is the write volume for `ourios.compaction.io` (RFC 0009
    /// §3.6).
    pub compacted_files: Vec<CompactedFile>,
    /// One entry per *successfully-planned* tenant: how many candidates
    /// the sweep found vs. how many it actually compacted. The residual
    /// (`candidates_found − partitions_compacted`) is that tenant's
    /// current sealed-but-uncompacted backlog — the absolute value the
    /// `ourios.compaction.backlog` observable reports (RFC 0009 §3.6).
    /// Tenants whose planning *errored* are omitted (their candidate
    /// count is unknown; they're recorded in [`Self::errors`]).
    pub per_tenant: Vec<TenantSweep>,
    /// RFC 0047 §3.6 erasure requests this sweep acted on, in the order
    /// they were processed.
    pub erasures: Vec<ErasureOutcome>,
    /// RFC 0047 §3.3 graph tuples the emitter wrote after this sweep
    /// (`0` without an emitter).
    pub graph_tuples_emitted: usize,
}

mod backfill;
mod daemon;
mod erasure;
#[cfg(feature = "openfga")]
mod graph_phase;
#[cfg(all(test, feature = "openfga"))]
mod graph_tests;
#[cfg(test)]
mod sweep_progress_tests;
#[cfg(test)]
mod tenant_metrics_tests;
#[cfg(test)]
mod tests;

// Scope glue: siblings and the test module reach each other's items
// through this parent scope, so every pre-split path resolves
// unchanged. Wildcards are the point (the children ARE this module).
#[allow(unused_imports, clippy::wildcard_imports)]
pub use backfill::*;
#[allow(unused_imports, clippy::wildcard_imports)]
pub use daemon::*;
#[allow(unused_imports, clippy::wildcard_imports)]
pub use erasure::*;
#[cfg(feature = "openfga")]
use graph_phase::GraphPhase;

fn store_error(op: &'static str, key: &str, e: &ourios_parquet::StoreError) -> IngestError {
    IngestError::Io {
        op,
        path: PathBuf::from(key),
        source: std::io::Error::other(e.to_string()),
    }
}

/// Per-tenant candidate vs. compacted counts for one sweep — the basis
/// for the `ourios.compaction.backlog` observable `UpDownCounter`
/// (RFC 0009 §3.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantSweep {
    /// Tenant the counts are for.
    pub tenant: String,
    /// Sealed candidate partitions [`plan_candidates`](ourios_parquet::plan_candidates) selected.
    pub candidates_found: usize,
    /// How many of those actually consolidated (committed) this sweep.
    pub partitions_compacted: usize,
}

/// A consolidated output file's size tagged with its tenant — one
/// `ourios.storage.parquet.file.size` sample (RFC 0009 §3.6 H4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactedFile {
    /// Tenant whose partition was compacted (the `ourios.tenant`
    /// histogram dimension).
    pub tenant: String,
    /// On-disk size of the consolidated file, in bytes.
    pub bytes: u64,
}

/// Run one compaction sweep over `store`, as of wall-clock
/// `now_unix_nanos`: for each tenant, select its sealed candidate
/// partitions ([`plan_candidates`](ourios_parquet::plan_candidates)) and consolidate each
/// ([`compact_partition_hooked`]), accumulating a [`SweepReport`].
///
/// Resilient: a tenant whose planning fails, or a partition whose
/// consolidation fails, is recorded in [`SweepReport::errors`] and
/// skipped — the sweep continues with the rest. Only a failure to
/// list the store itself (the tenant enumeration) is fatal.
///
/// # Errors
///
/// [`IngestError`] only if the store's tenant keys can't be listed;
/// per-tenant / per-partition failures are collected into the returned
/// report, not propagated.
pub fn run_sweep(
    store: &Store,
    now_unix_nanos: u64,
    policy: &CompactionPolicy,
) -> Result<SweepReport, IngestError> {
    run_sweep_hooked(
        store,
        now_unix_nanos,
        policy,
        &PromotedAttributes::default(),
        &mut SweepHooks::default(),
    )
}

/// The RFC 0047 hooks a sweep runs with: `observe` sees every row the
/// sweep rewrites, per tenant (the graph feed, §3.3); `erasure_match`
/// decides which rows an erasure request drops (§3.6) — without it,
/// pending erasures are recorded as errors, never silently skipped.
#[derive(Default)]
pub struct SweepHooks<'a> {
    /// `(tenant, rows)` for every batch of input rows the sweep decodes.
    pub observe: Option<&'a mut SweepObserver<'a>>,
    /// `(row, conversation_id)` → whether the row belongs to the conversation.
    pub erasure_match: Option<&'a ErasureMatch<'a>>,
    /// Kept current while the sweep runs, for the metrics to read.
    pub gauges: Option<&'a SweepGauges>,
}

/// Live measurements of a running sweep (#925), written by the sweep and
/// read by the metric callbacks at collect time.
#[derive(Debug, Default)]
pub struct SweepGauges {
    /// Consolidation candidates the running sweep has not yet processed;
    /// zero between sweeps. Set from the sweep's one planning pass, so it
    /// never rises during a sweep ([`Countdown`]).
    remaining: AtomicU64,
    /// Every rewrite's memory, budgets and spills.
    compaction: CompactionStats,
}

impl SweepGauges {
    /// Candidates the running sweep has not yet processed.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.remaining.load(Ordering::Relaxed)
    }

    /// The rewrites' memory, budget and spill measurements.
    #[must_use]
    pub fn compaction(&self) -> &CompactionStats {
        &self.compaction
    }
}

/// Counts a sweep's consolidation candidates down as they are processed,
/// and back to zero when dropped, however the pass ends.
struct Countdown<'a> {
    remaining: Option<&'a AtomicU64>,
}

impl<'a> Countdown<'a> {
    fn start(gauges: Option<&'a SweepGauges>, planned: usize) -> Self {
        let remaining = gauges.map(|gauges| &gauges.remaining);
        if let Some(remaining) = remaining {
            remaining.store(to_u64(planned), Ordering::Relaxed);
        }
        Self { remaining }
    }

    fn tick(&self) {
        self.store(|left| left.saturating_sub(1));
    }

    fn store(&self, update: impl FnOnce(u64) -> u64) {
        if let Some(remaining) = self.remaining {
            // The sweep is the only writer.
            remaining.store(update(remaining.load(Ordering::Relaxed)), Ordering::Relaxed);
        }
    }
}

impl Drop for Countdown<'_> {
    fn drop(&mut self) {
        self.store(|_| 0);
    }
}

/// One tenant's sealed consolidation candidates, or why planning them
/// failed.
type TenantPlan<'t> = (&'t str, Result<Vec<PartitionKey>, CompactionError>);

/// Plan every tenant once, up front. Planning lists each tenant's objects
/// and reads the manifest of every sealed partition holding two or more
/// files, so it is the sweep's dominant request cost on a store with a long
/// history; the plans both set the countdown and drive the consolidation. A
/// partition that becomes a candidate during the sweep (a late write into a
/// sealed hour) is planned by the next one. The plans
/// hold one key, the tenant plus four integers, per *candidate*: they grow
/// with the compaction backlog, not with the retained history.
///
/// The whole pass is the RFC 0038 §3.8 `plan partitions` phase, carrying its
/// store work summed over the tenants.
fn plan_tenants<'t>(
    store: &Store,
    tenants: &'t [String],
    sealed_at: u64,
    policy: &CompactionPolicy,
) -> Vec<TenantPlan<'t>> {
    let span = tracing::info_span!(
        "plan partitions",
        otel.kind = "internal",
        ourios.file_set.list_request_count = tracing::field::Empty,
        ourios.file_set.listed_objects = tracing::field::Empty,
        ourios.file_set.manifest_read_count = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
        error.type = tracing::field::Empty,
    );
    let _phase = span.enter();
    let mut counts = PlanCounts::default();
    let plans: Vec<TenantPlan<'t>> = tenants
        .iter()
        .map(|tenant| {
            let (planned, tenant_counts) =
                plan_candidates_counted(store, tenant, sealed_at, policy);
            counts += tenant_counts;
            (tenant.as_str(), planned)
        })
        .collect();
    record_plan(&span, counts, &plans);
    plans
}

/// Set the planning phase's counts, and mark it `Error` with the first
/// failed tenant's class: the sweep skips that tenant and goes on.
fn record_plan(span: &tracing::Span, counts: PlanCounts, plans: &[TenantPlan<'_>]) {
    // `i64`: tracing-opentelemetry renders a `u64` field as a string.
    let as_count = |count: usize| i64::try_from(count).unwrap_or(i64::MAX);
    span.record(
        ourios_semconv::OURIOS_FILE_SET_LIST_REQUEST_COUNT,
        as_count(counts.list_requests),
    );
    span.record(
        ourios_semconv::OURIOS_FILE_SET_LISTED_OBJECTS,
        as_count(counts.listed_objects),
    );
    span.record(
        ourios_semconv::OURIOS_FILE_SET_MANIFEST_READ_COUNT,
        as_count(counts.manifest_reads),
    );
    if let Some(error) = plans.iter().find_map(|(_, planned)| planned.as_ref().err()) {
        ourios_core::telemetry::record_span_error(span, compaction_error_type(error));
    }
}

/// A [`SweepHooks::observe`] callback.
pub type SweepObserver<'a> = dyn FnMut(&str, &[MinedRecord]) + 'a;
/// A [`SweepHooks::erasure_match`] predicate.
pub type ErasureMatch<'a> = dyn Fn(&MinedRecord, &str) -> bool + 'a;

/// [`run_sweep`] under an explicit RFC 0022 promoted attribute set
/// (§3.4: rewrites re-project with the *current* set), with
/// [`SweepHooks`]: the consolidation
/// pass, then the RFC 0047 §3.6 erasure pass — every pending request in
/// the `Rows` phase rewrites each of its tenant's partitions with the
/// conversation's rows dropped; once every partition rewrote cleanly the
/// marker advances to `Tuples` (the tuple deletion is the async caller's,
/// after this pass — never before the rewrite).
///
/// # Errors
///
/// As [`run_sweep`].
pub fn run_sweep_hooked(
    store: &Store,
    now_unix_nanos: u64,
    policy: &CompactionPolicy,
    promoted: &PromotedAttributes,
    hooks: &mut SweepHooks<'_>,
) -> Result<SweepReport, IngestError> {
    run_sweep_committing(
        store,
        SweepClock::sealed_at(now_unix_nanos),
        policy,
        promoted,
        hooks,
        &mut |_| {},
    )
    .map_err(|failed| failed.error)
}

/// A sweep cut short by a fatal error, with the report of what it did
/// before the error when there is one: a failure listing the store's
/// tenants leaves none, while a failure in the erasure pass comes after
/// the whole consolidation pass, whose partitions are committed.
#[derive(Debug)]
pub struct FailedSweep {
    /// The fatal error.
    pub error: IngestError,
    /// What the sweep did before it.
    pub partial: Option<Box<SweepReport>>,
}

/// A sweep's two times: the instant its candidates are sealed against,
/// fixed for the whole sweep, and the clock each committed partition's
/// audit event is stamped from as it is emitted (RFC 0005 §3.7: the
/// cluster clock at emit time).
#[derive(Debug, Clone, Copy)]
pub struct SweepClock {
    /// Unix nanoseconds the candidates are planned and sealed against.
    pub sealed_at: u64,
    /// Unix nanoseconds now, read once per committed partition.
    pub now: fn() -> u64,
}

impl SweepClock {
    /// Seal against `sealed_at`; stamp audit events with the wall clock.
    #[must_use]
    pub fn sealed_at(sealed_at: u64) -> Self {
        Self {
            sealed_at,
            now: now_unix_nanos,
        }
    }
}

/// One partition rewrite whose manifest commit was just won, handed to the
/// sweep's commit callback before the superseded inputs are cleaned up
/// (RFC 0009 §3.6): the
/// counters it adds and the audit event it carries are the ones the
/// [`SweepReport`] accumulates, delivered while the sweep is still running.
#[derive(Debug)]
pub struct PartitionCommitted<'a> {
    /// Tenant whose partition was rewritten.
    pub tenant: &'a str,
    /// Input files merged away.
    pub files: u64,
    /// Rows rewritten.
    pub rows: u64,
    /// Input bytes read: the inputs' stored (encoded) size.
    pub bytes_read: u64,
    /// The inputs' decoded size as the RFC 0036 §3.2 budget estimates it.
    pub decoded_bytes_estimate: u64,
    /// Size of the consolidated output file.
    pub bytes_written: u64,
    /// The partition's [`AuditPayload::Compaction`] event.
    pub event: &'a AuditEvent,
}

/// The commit callback of [`run_sweep_committing`].
pub type CommitObserver<'a> = dyn FnMut(&PartitionCommitted<'_>) + 'a;

/// [`run_sweep_hooked`], calling `on_commit` for every partition rewrite
/// (consolidation or erasure) as its manifest commit is won, so the caller
/// can record and emit per partition rather than at sweep end: a sweep
/// over a large backlog runs for hours, and a restart mid-sweep no longer
/// loses what the partitions committed before it already did. The
/// callback's own work is best-effort — a crash inside it, or a sink that
/// suppresses an error, can still drop that one partition's record.
/// Candidates are sealed against `clock.sealed_at`; each audit event is
/// stamped from `clock.now` as it is built.
///
/// # Errors
///
/// As [`run_sweep`], with the report of what the sweep did before the
/// error ([`FailedSweep`]).
// RFC 0038: one span per compaction sweep — coarse and periodic. Opened inside
// the callee (the tick `spawn_blocking`s this), and the per-tenant / per-file
// loops below stay span-free.
#[tracing::instrument(
    skip_all,
    name = "sweep partitions",
    fields(
        otel.kind = "internal",
        otel.status_code = tracing::field::Empty,
        error.type = tracing::field::Empty,
    )
)]
pub fn run_sweep_committing(
    store: &Store,
    clock: SweepClock,
    policy: &CompactionPolicy,
    promoted: &PromotedAttributes,
    hooks: &mut SweepHooks<'_>,
    on_commit: &mut CommitObserver<'_>,
) -> Result<SweepReport, FailedSweep> {
    let outcome = sweep_committing(store, clock, policy, promoted, hooks, on_commit);
    if let Err(failed) = &outcome {
        ourios_core::telemetry::record_span_error(
            &tracing::Span::current(),
            failed.error.error_type(),
        );
    }
    outcome
}

/// [`run_sweep_committing`]'s work, inside its span.
fn sweep_committing(
    store: &Store,
    clock: SweepClock,
    policy: &CompactionPolicy,
    promoted: &PromotedAttributes,
    hooks: &mut SweepHooks<'_>,
    on_commit: &mut CommitObserver<'_>,
) -> Result<SweepReport, FailedSweep> {
    let tenants = tenants(store).map_err(|error| FailedSweep {
        error,
        partial: None,
    })?;
    let mut sweep = Sweep {
        store,
        clock,
        promoted,
        report: SweepReport::default(),
        on_commit,
        stats: hooks.gauges.map(SweepGauges::compaction),
    };
    sweep.consolidate_all(&tenants, policy, hooks);
    match erase_pending(&mut sweep, hooks.erasure_match) {
        Ok(()) => Ok(sweep.report),
        Err(error) => Err(FailedSweep {
            error,
            partial: Some(Box::new(sweep.report)),
        }),
    }
}

/// One sweep in progress: its fixed inputs, the report it accumulates, and
/// the commit callback each committed partition reaches.
struct Sweep<'s, 'c> {
    store: &'s Store,
    clock: SweepClock,
    promoted: &'s PromotedAttributes,
    report: SweepReport,
    on_commit: &'s mut CommitObserver<'c>,
    stats: Option<&'s CompactionStats>,
}

/// One partition of one tenant.
#[derive(Clone, Copy)]
struct Target<'p> {
    tenant: &'p str,
    partition: &'p PartitionKey,
}

impl Sweep<'_, '_> {
    /// The consolidation pass over every tenant, planned once
    /// ([`plan_tenants`]), counting the candidates down ([`Countdown`]).
    fn consolidate_all(
        &mut self,
        tenants: &[String],
        policy: &CompactionPolicy,
        hooks: &mut SweepHooks<'_>,
    ) {
        let plans = plan_tenants(self.store, tenants, self.clock.sealed_at, policy);
        let planned = plans
            .iter()
            .map(|(_, planned)| planned.as_ref().map_or(0, Vec::len))
            .sum();
        let countdown = Countdown::start(hooks.gauges, planned);
        for (tenant, planned) in plans {
            let observe = hooks.observe.as_deref_mut();
            self.tenant(tenant, planned, observe, &countdown);
        }
    }

    /// Consolidate `tenant`'s planned candidates, counting each down. A
    /// planning failure is recorded and the tenant skipped.
    fn tenant(
        &mut self,
        tenant: &str,
        planned: Result<Vec<PartitionKey>, CompactionError>,
        mut observe: Option<&mut SweepObserver<'_>>,
        countdown: &Countdown<'_>,
    ) {
        self.report.tenants_scanned += 1;
        let candidates = match planned {
            Ok(candidates) => candidates,
            Err(e) => {
                self.report
                    .errors
                    .push(format!("plan tenant {tenant:?}: {e}"));
                return;
            }
        };
        let mut compacted_here = 0usize;
        for partition in &candidates {
            let target = Target { tenant, partition };
            if self.consolidate(target, observe.as_deref_mut()) {
                compacted_here += 1;
            }
            countdown.tick();
        }
        self.report.per_tenant.push(TenantSweep {
            tenant: tenant.to_string(),
            candidates_found: candidates.len(),
            partitions_compacted: compacted_here,
        });
    }

    /// Consolidate the partition, then reclaim its orphans; whether it
    /// committed.
    fn consolidate(&mut self, target: Target<'_>, observe: Option<&mut SweepObserver<'_>>) -> bool {
        let Target { tenant, partition } = target;
        let mut observe = observe.map(|observe| move |rows: &[MinedRecord]| observe(tenant, rows));
        let observe = observe
            .as_mut()
            .map(|observe| observe as &mut dyn FnMut(&[MinedRecord]));
        let committed = match self.rewrite(target, observe, None) {
            Ok(outcome) => {
                self.report.gc_failures += outcome.gc_failures;
                outcome.committed.is_some()
            }
            Err(e) => {
                let context = format!("compact {tenant:?} {}", hour_label(partition));
                e.record(&mut self.report, &context);
                false
            }
        };
        self.reclaim_orphans(target);
        committed
    }

    /// Reclaim the orphans a prior crashed compaction of the partition left
    /// (RFC0009.4). It runs after the rewrite: a crashed output written for
    /// the generation this rewrite just committed is dead only now
    /// (RFC0009.17), and the partition may not be a candidate again.
    /// Manifest-authoritative, so it never touches a live file; a scan
    /// error is recorded, not fatal.
    fn reclaim_orphans(&mut self, target: Target<'_>) {
        let Target { tenant, partition } = target;
        match gc_orphans(self.store, partition) {
            Ok(gc) => self.report.orphans_reclaimed += gc.reclaimed,
            Err(e) => self.report.errors.push(format!(
                "gc-orphans {tenant:?} {:04}-{:02}-{:02}T{:02}: {e}",
                partition.year, partition.month, partition.day, partition.hour,
            )),
        }
    }

    /// Rewrite the partition ([`compact_candidate`]), recording its commit
    /// — report, audit event, commit callback — the moment the manifest
    /// commit is won, before the superseded inputs are cleaned up: a crash
    /// in that cleanup leaves the partition committed, and recorded.
    fn rewrite(
        &mut self,
        target: Target<'_>,
        observe: Option<&mut RowObserver<'_>>,
        drop: Option<&RowFilter<'_>>,
    ) -> Result<CompactionOutcome, Uncommitted> {
        let Self {
            store,
            clock,
            promoted,
            report,
            on_commit,
            stats,
        } = self;
        let mut record = |outcome: &CompactionOutcome| {
            if let Some(committed) = &outcome.committed {
                let event = compaction_audit_event(
                    target.tenant,
                    (clock.now)(),
                    target.partition,
                    committed,
                    outcome.rows,
                );
                record_commit(report, target.tenant, outcome, event, &mut **on_commit);
            }
        };
        let mut hooks = RowHooks {
            observe: observe.map(|observe| observe as &mut dyn FnMut(&[MinedRecord])),
            drop: drop.map(|drop| drop as &dyn Fn(&MinedRecord) -> bool),
            on_commit: Some(&mut record),
            stats: *stats,
        };
        compact_candidate(store, target.partition, promoted, &mut hooks)
    }
}

/// Raw tenant ids present in the store, decoded from the immediate
/// `data/tenant_id=<enc>` child common-prefixes
/// ([`Store::list_common_prefixes_blocking`], RFC 0019 §3.3), sorted +
/// deduplicated so a sweep is deterministic. This is a **one-level** roll-up
/// (the object-store equivalent of the original `read_dir(data/)`), not a
/// recursive scan of every object. Prefixes that don't decode are skipped (not
/// Ourios output); an empty `data/` prefix yields none.
fn tenants(store: &Store) -> Result<Vec<String>, IngestError> {
    let prefixes = store
        .list_common_prefixes_blocking(Some("data"))
        .map_err(|source| IngestError::Io {
            op: "list",
            path: PathBuf::from("data"),
            source: std::io::Error::other(source),
        })?;
    let mut tenants: Vec<String> = prefixes
        .iter()
        // Each prefix is `data/tenant_id=<enc>`; take the trailing segment.
        .filter_map(|prefix| prefix.rsplit('/').next())
        .filter_map(|segment| segment.strip_prefix("tenant_id="))
        .filter_map(percent_decode_tenant)
        .collect();
    tenants.sort();
    tenants.dedup();
    Ok(tenants)
}

/// Saturating `usize` → `u64` (lossless on 64-bit; saturates rather
/// than truncating on a theoretically wider target).
pub(crate) fn to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// A candidate rewrite that failed or did not commit, carrying the
/// non-live files its cleanup could not remove so the sweep still counts
/// them in [`SweepReport::gc_failures`].
#[derive(Debug)]
struct Uncommitted {
    reason: String,
    gc_failures: usize,
}

/// `partition`'s hour as `YYYY-MM-DDTHH`, for sweep error messages.
fn hour_label(partition: &PartitionKey) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}",
        partition.year, partition.month, partition.day, partition.hour
    )
}

impl Uncommitted {
    /// Record this as a sweep error under `context`, with its cleanup
    /// failures.
    fn record(self, report: &mut SweepReport, context: &str) {
        report.gc_failures += self.gc_failures;
        report.errors.push(format!("{context}: {}", self.reason));
    }
}

/// [`compact_partition_hooked`], with an uncommitted rewrite turned into an
/// error by [`check_committed`]. With one sweeper a lost swap is never a
/// benign race: a store whose swaps always lose must not look like an idle
/// sweep, and an erasure must not advance past rows it never rewrote.
fn compact_candidate(
    store: &Store,
    partition: &PartitionKey,
    promoted: &PromotedAttributes,
    hooks: &mut RowHooks<'_>,
) -> Result<CompactionOutcome, Uncommitted> {
    let erasing = hooks.drop.is_some();
    let outcome =
        compact_partition_hooked(store, partition, promoted, hooks).map_err(|e| Uncommitted {
            reason: e.to_string(),
            gc_failures: 0,
        })?;
    check_committed(outcome, erasing)
}

/// `outcome`, or the sweep error it must be rather than a no-op. A lost
/// manifest commit always is. An erasure is too whenever it left live rows
/// unrewritten, because its marker must not advance past rows still on disk.
fn check_committed(
    outcome: CompactionOutcome,
    erasing: bool,
) -> Result<CompactionOutcome, Uncommitted> {
    let unrewritten = erasing && outcome.committed.is_none() && outcome.files_before > 0;
    if !(outcome.commit_lost || unrewritten) {
        return Ok(outcome);
    }
    Err(Uncommitted {
        reason: format!(
            "rewrite of {} live files not committed \
             (manifest compare-and-swap lost); retried next sweep",
            outcome.files_before
        ),
        gc_failures: outcome.gc_failures,
    })
}

/// Build the RFC 0009 §3.6 audit event for a committed compaction
/// (RFC 0005 §3.7 `AuditPayload::Compaction`), stamped `emitted_at` —
/// the cluster clock as the event is built; the partition is the canonical
/// `year=…/month=…/day=…/hour=…` key (RFC 0005 §3.4).
fn compaction_audit_event(
    tenant: &str,
    emitted_at: u64,
    partition: &PartitionKey,
    committed: &Committed,
    rows: u64,
) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        // `checked_add` so a saturated `emitted_at` (year ~2554,
        // unreachable in practice — see `now_unix_nanos`) can't panic;
        // falls back to the epoch rather than aborting a sweep.
        timestamp: SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_nanos(emitted_at))
            .unwrap_or(SystemTime::UNIX_EPOCH),
        payload: AuditPayload::Compaction {
            partition: format!(
                "year={:04}/month={:02}/day={:02}/hour={:02}",
                partition.year, partition.month, partition.day, partition.hour,
            ),
            input_files: committed.input_files.clone(),
            output_file: committed.file.clone(),
            generation: committed.generation,
            rows,
        },
    }
}

/// Account one committed rewrite in `report`, handing it to `on_commit`
/// first — the moment its manifest commit is durable.
fn record_commit(
    report: &mut SweepReport,
    tenant: &str,
    outcome: &CompactionOutcome,
    event: AuditEvent,
    on_commit: &mut CommitObserver<'_>,
) {
    let files = to_u64(outcome.files_before);
    on_commit(&PartitionCommitted {
        tenant,
        files,
        rows: outcome.rows,
        bytes_read: outcome.bytes_read,
        decoded_bytes_estimate: outcome.decoded_bytes_estimate,
        bytes_written: outcome.bytes_written,
        event: &event,
    });
    report.partitions_compacted += 1;
    report.files_compacted += files;
    report.rows_compacted += outcome.rows;
    report.bytes_read = report.bytes_read.saturating_add(outcome.bytes_read);
    report.compacted_files.push(CompactedFile {
        tenant: tenant.to_string(),
        bytes: outcome.bytes_written,
    });
    report.compaction_events.push(event);
}

/// `SystemTime::now()` as Unix nanoseconds (`0` if the clock is before
/// the epoch; saturated at `u64::MAX` past year 2554 — neither is
/// reachable in practice).
fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}
