//! Background compaction runner (RFC 0009 §3.2).
//!
//! [`run_sweep`] is one pass over the whole store — for every tenant,
//! select its sealed candidate partitions and consolidate them. It is
//! synchronous (blocking filesystem + Parquet work) and deterministic,
//! so it's the unit the tests exercise. [`Compactor::run`] is the thin
//! daemon: it calls `run_sweep` on a fixed cadence via `spawn_blocking`,
//! records the RFC 0009 §3.6 metrics for each sweep
//! ([`crate::metrics::CompactionMetrics`]), and hands each result to a
//! caller-supplied observer for logging.

#[cfg(feature = "openfga")]
use std::collections::BTreeSet;
use std::path::PathBuf;
#[cfg(feature = "openfga")]
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use ourios_core::audit::{AuditEvent, AuditPayload, AuditSink, NoOpAuditSink};
use ourios_core::record::MinedRecord;
use ourios_core::tenant::TenantId;
use ourios_parquet::{
    Committed, CompactionError, CompactionPolicy, PartitionKey, PromotedAttributes, RowHooks,
    Store, compact_partition_hooked, gc_orphans, hour_partitions, percent_decode_tenant,
    percent_encode_tenant, plan_candidates,
};

#[cfg(feature = "openfga")]
use crate::graph_emitter::GraphEmitter;
use crate::metrics::CompactionMetrics;

/// The tuples one sweep derives for the graph (RFC 0047 §3.3).
#[cfg(feature = "openfga")]
type GraphTuples = BTreeSet<ourios_core::auth::openfga::TupleKey>;
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
    /// Per-tenant / per-partition failures encountered, formatted for
    /// logging. A sweep is **resilient**: one bad tenant or partition
    /// is recorded here and skipped, never aborting the rest (else a
    /// persistent error would starve every later tenant, since the
    /// daemon just retries the same sweep next tick).
    pub errors: Vec<String>,
    /// One [`AuditPayload::Compaction`] audit event per committed
    /// compaction (RFC 0009 §3.6 / RFC 0005 §3.7). Built here;
    /// [`Compactor::run`] emits them through its [`AuditSink`].
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

/// One pending RFC 0047 §3.6 erasure: a durable request marker in the
/// store (`erasure/tenant_id=<enc>/conversation=<enc>`, written by an
/// operator through [`request_erasure`]) naming the conversation to
/// remove from a tenant. The marker's body carries the phase, so a
/// sweep interrupted after the rewrite retries only the tuple deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErasureRequest {
    /// The tenant the conversation lives in.
    pub tenant: String,
    /// The raw conversation id (the promoted-column value).
    pub conversation_id: String,
    /// The marker object's key.
    pub marker: String,
    /// Where the request stands.
    pub phase: ErasurePhase,
}

/// The two phases of an erasure — rows first, tuples after (RFC 0047
/// §3.6: a dangling tuple is harmless, a dangling row is a leak).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErasurePhase {
    /// The Parquet rewrite has not completed for every partition.
    Rows,
    /// Rows are gone; the graph tuples remain to be deleted.
    Tuples,
}

/// What one sweep did for one erasure request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErasureOutcome {
    /// The request as it was when the sweep picked it up.
    pub request: ErasureRequest,
    /// Partitions rewritten this sweep (`0` when the rows phase was
    /// already done).
    pub partitions_rewritten: u64,
    /// Rows dropped this sweep.
    pub rows_dropped: u64,
    /// The phase after this sweep's blocking pass: `Tuples` once every
    /// partition rewrote cleanly, else still `Rows` (retried next sweep).
    pub phase: ErasurePhase,
    /// Graph tuples deleted (set by the async phase; `None` until then).
    pub tuples_deleted: Option<usize>,
    /// Whether the marker was removed — the erasure is complete.
    pub finished: bool,
}

/// The object-store prefix of erasure request markers.
pub const ERASURE_PREFIX: &str = "erasure/";
const ERASURE_PHASE_ROWS: &[u8] = br#"{"phase":"rows"}"#;
const ERASURE_PHASE_TUPLES: &[u8] = br#"{"phase":"tuples"}"#;

/// The marker key for erasing `conversation_id` from `tenant`.
#[must_use]
pub fn erasure_marker_key(tenant: &str, conversation_id: &str) -> String {
    format!(
        "{ERASURE_PREFIX}tenant_id={}/conversation={}",
        percent_encode_tenant(tenant),
        percent_encode_tenant(conversation_id)
    )
}

/// Request the erasure of `conversation_id` from `tenant` (RFC 0047 §3.6):
/// writes the durable marker the next sweep acts on. Idempotent —
/// create-if-absent, so a repeated request never resets an erasure already
/// in flight (one whose rows are gone and whose marker is in the `tuples`
/// phase).
///
/// # Errors
///
/// [`IngestError::Io`] when the marker cannot be written.
pub fn request_erasure(
    store: &Store,
    tenant: &str,
    conversation_id: &str,
) -> Result<(), IngestError> {
    let key = erasure_marker_key(tenant, conversation_id);
    match store.put_if_absent_blocking(&key, ERASURE_PHASE_ROWS.to_vec()) {
        Ok(()) => Ok(()),
        Err(e) if e.is_already_exists() => Ok(()),
        Err(e) => Err(store_error("put erasure marker", &key, &e)),
    }
}

/// The pending erasure requests, in deterministic (lexicographic key)
/// order.
///
/// # Errors
///
/// [`IngestError::Io`] when the marker prefix cannot be listed or a marker
/// cannot be read — a transient store error never regresses a marker's
/// phase.
pub fn pending_erasures(store: &Store) -> Result<Vec<ErasureRequest>, IngestError> {
    let mut keys = store
        .list_blocking(Some(ERASURE_PREFIX))
        .map_err(|e| store_error("list erasure markers", ERASURE_PREFIX, &e))?;
    keys.sort();
    let mut requests = Vec::new();
    for key in keys {
        let Some(rest) = key.strip_prefix(ERASURE_PREFIX) else {
            continue;
        };
        let Some((tenant_segment, conversation_segment)) = rest.split_once('/') else {
            continue;
        };
        let (Some(tenant), Some(conversation)) = (
            tenant_segment
                .strip_prefix("tenant_id=")
                .and_then(percent_decode_tenant),
            conversation_segment
                .strip_prefix("conversation=")
                .and_then(percent_decode_tenant),
        ) else {
            continue;
        };
        let body = store
            .get_blocking_opt(&key)
            .map_err(|e| store_error("read erasure marker", &key, &e))?;
        // A marker removed between the listing and this read is finished.
        let Some(body) = body else {
            continue;
        };
        // The marker body is `{"phase": "rows" | "tuples"}`, parsed
        // leniently (whitespace, key order) — operator tooling writes these.
        let phase = match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(marker) if marker["phase"].as_str().map(str::trim) == Some("tuples") => {
                ErasurePhase::Tuples
            }
            _ => ErasurePhase::Rows,
        };
        requests.push(ErasureRequest {
            tenant,
            conversation_id: conversation,
            marker: key,
            phase,
        });
    }
    Ok(requests)
}

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
    /// Sealed candidate partitions [`plan_candidates`] selected.
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
/// partitions ([`plan_candidates`]) and consolidate each
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
    run_sweep_with_promoted(
        store,
        now_unix_nanos,
        policy,
        &PromotedAttributes::default(),
    )
}

/// Like [`run_sweep`] but consolidating under an explicit RFC 0022 promoted
/// attribute set (§3.4: rewrites re-project with the *current* set). The bare
/// [`run_sweep`] delegates with the default (`service.name`-only) set.
///
/// # Errors
///
/// See [`run_sweep`].
pub fn run_sweep_with_promoted(
    store: &Store,
    now_unix_nanos: u64,
    policy: &CompactionPolicy,
    promoted: &PromotedAttributes,
) -> Result<SweepReport, IngestError> {
    run_sweep_hooked(
        store,
        now_unix_nanos,
        policy,
        promoted,
        &mut SweepHooks::default(),
    )
}

/// The RFC 0047 hooks a sweep runs with: `observe` sees every row the
/// sweep rewrites, per tenant (the graph feed, §3.3); `erasure_match`
/// decides which rows an erasure request drops (§3.6) — without it,
/// pending erasures are recorded as errors, never silently skipped.
#[derive(Default)]
pub struct SweepHooks<'a> {
    /// `(tenant, rows)` for every input file the sweep decodes.
    pub observe: Option<&'a mut SweepObserver<'a>>,
    /// `(row, conversation_id)` → whether the row belongs to the conversation.
    pub erasure_match: Option<&'a ErasureMatch<'a>>,
}

/// A [`SweepHooks::observe`] callback.
pub type SweepObserver<'a> = dyn FnMut(&str, &[MinedRecord]) + 'a;
/// A [`SweepHooks::erasure_match`] predicate.
pub type ErasureMatch<'a> = dyn Fn(&MinedRecord, &str) -> bool + 'a;

/// [`run_sweep_with_promoted`] with [`SweepHooks`]: the consolidation
/// pass, then the RFC 0047 §3.6 erasure pass — every pending request in
/// the `Rows` phase rewrites each of its tenant's partitions with the
/// conversation's rows dropped; once every partition rewrote cleanly the
/// marker advances to `Tuples` (the tuple deletion is the async caller's,
/// after this pass — never before the rewrite).
///
/// # Errors
///
/// As [`run_sweep`].
// RFC 0038: one span per compaction sweep — coarse and periodic. Opened inside
// the callee (the tick `spawn_blocking`s this), and the per-tenant / per-file
// loops below stay span-free.
#[tracing::instrument(
    skip_all,
    name = "sweep partitions",
    fields(otel.kind = "internal")
)]
pub fn run_sweep_hooked(
    store: &Store,
    now_unix_nanos: u64,
    policy: &CompactionPolicy,
    promoted: &PromotedAttributes,
    hooks: &mut SweepHooks<'_>,
) -> Result<SweepReport, IngestError> {
    let mut report = SweepReport::default();
    for tenant in tenants(store)? {
        report.tenants_scanned += 1;
        let candidates = match plan_candidates(store, &tenant, now_unix_nanos, policy) {
            Ok(candidates) => candidates,
            Err(e) => {
                report.errors.push(format!("plan tenant {tenant:?}: {e}"));
                continue;
            }
        };
        let candidates_found = candidates.len();
        let mut compacted_here = 0usize;
        for partition in candidates {
            // Reclaim orphans a prior crashed compaction of this partition
            // left (RFC0009.4). Manifest-authoritative, so it never touches
            // a live file; a scan error is recorded, not fatal.
            match gc_orphans(store, &partition) {
                Ok(gc) => report.orphans_reclaimed += gc.reclaimed,
                Err(e) => report.errors.push(format!(
                    "gc-orphans {tenant:?} {:04}-{:02}-{:02}T{:02}: {e}",
                    partition.year, partition.month, partition.day, partition.hour,
                )),
            }
            let tenant_name = tenant.as_str();
            let mut observe = hooks
                .observe
                .as_deref_mut()
                .map(|observe| move |rows: &[MinedRecord]| observe(tenant_name, rows));
            let mut row_hooks = RowHooks {
                observe: observe
                    .as_mut()
                    .map(|observe| observe as &mut dyn FnMut(&[MinedRecord])),
                drop: None,
            };
            match compact_partition_hooked(store, &partition, promoted, &mut row_hooks) {
                Ok(outcome) => {
                    if let Some(committed) = &outcome.committed {
                        report.partitions_compacted += 1;
                        compacted_here += 1;
                        report.files_compacted += to_u64(outcome.files_before);
                        report.rows_compacted += outcome.rows;
                        report.bytes_read = report.bytes_read.saturating_add(outcome.bytes_read);
                        report.compacted_files.push(CompactedFile {
                            tenant: tenant.clone(),
                            bytes: outcome.bytes_written,
                        });
                        report.compaction_events.push(compaction_audit_event(
                            &tenant,
                            now_unix_nanos,
                            &partition,
                            committed,
                            outcome.rows,
                        ));
                    }
                    report.gc_failures += outcome.gc_failures;
                }
                Err(e) => report.errors.push(format!(
                    "compact {tenant:?} {:04}-{:02}-{:02}T{:02}: {e}",
                    partition.year, partition.month, partition.day, partition.hour,
                )),
            }
        }
        report.per_tenant.push(TenantSweep {
            tenant,
            candidates_found,
            partitions_compacted: compacted_here,
        });
    }
    erase_pending(
        store,
        now_unix_nanos,
        promoted,
        hooks.erasure_match,
        &mut report,
    )?;
    Ok(report)
}

/// The RFC 0047 §3.6 erasure pass (rows phase).
fn erase_pending(
    store: &Store,
    now_unix_nanos: u64,
    promoted: &PromotedAttributes,
    erasure_match: Option<&ErasureMatch<'_>>,
    report: &mut SweepReport,
) -> Result<(), IngestError> {
    for request in pending_erasures(store)? {
        let mut outcome = ErasureOutcome {
            request: request.clone(),
            partitions_rewritten: 0,
            rows_dropped: 0,
            phase: request.phase,
            tuples_deleted: None,
            finished: false,
        };
        if request.phase == ErasurePhase::Rows {
            let Some(matches) = erasure_match else {
                report.errors.push(format!(
                    "erase {:?} {:?}: no conversation column configured \
                     (auth.openfga.visibility.objects) — request left pending",
                    request.tenant, request.conversation_id
                ));
                report.erasures.push(outcome);
                continue;
            };
            let id = request.conversation_id.as_str();
            let drop = |record: &MinedRecord| matches(record, id);
            let mut clean = true;
            let partitions = match hour_partitions(store, &request.tenant) {
                Ok(partitions) => partitions,
                Err(e) => {
                    report
                        .errors
                        .push(format!("erase {:?}: list partitions: {e}", request.tenant));
                    report.erasures.push(outcome);
                    continue;
                }
            };
            for partition in partitions {
                let mut row_hooks = RowHooks {
                    observe: None,
                    drop: Some(&drop),
                };
                match compact_partition_hooked(store, &partition, promoted, &mut row_hooks) {
                    Ok(o) => {
                        if let Some(committed) = &o.committed {
                            outcome.partitions_rewritten += 1;
                            outcome.rows_dropped += o.rows_dropped;
                            // An erasure rewrite is a compaction like any
                            // other for the sweep's IO accounting and audit
                            // trail (RFC 0009 §3.6): it reads and writes the
                            // partition and commits a generation.
                            report.partitions_compacted += 1;
                            report.files_compacted += to_u64(o.files_before);
                            report.rows_compacted += o.rows;
                            report.bytes_read = report.bytes_read.saturating_add(o.bytes_read);
                            report.compacted_files.push(CompactedFile {
                                tenant: request.tenant.clone(),
                                bytes: o.bytes_written,
                            });
                            report.compaction_events.push(compaction_audit_event(
                                &request.tenant,
                                now_unix_nanos,
                                &partition,
                                committed,
                                o.rows,
                            ));
                            report.gc_failures += o.gc_failures;
                        }
                    }
                    Err(e) => {
                        clean = false;
                        report.errors.push(format!(
                            "erase {:?} {:?} {:04}-{:02}-{:02}T{:02}: {e}",
                            request.tenant,
                            request.conversation_id,
                            partition.year,
                            partition.month,
                            partition.day,
                            partition.hour,
                        ));
                    }
                }
            }
            if clean {
                match store.put_blocking(&request.marker, ERASURE_PHASE_TUPLES.to_vec()) {
                    Ok(()) => outcome.phase = ErasurePhase::Tuples,
                    Err(e) => report.errors.push(format!(
                        "erase {:?} {:?}: advance marker: {e}",
                        request.tenant, request.conversation_id
                    )),
                }
            }
        }
        report.erasures.push(outcome);
    }
    Ok(())
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
    /// recorded for every sweep via the `ourios.compaction` meter
    /// (instruments built and seeded once here, before the loop). Does
    /// not return.
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
        let metrics = CompactionMetrics::new();
        let mut ticker = tokio::time::interval(interval);
        // A maintenance sweep that overruns `interval` must not make
        // the next ticks fire back-to-back (the default `Burst`) —
        // that would pile sustained compaction load after any slow
        // pass. `Delay` keeps a full `interval` gap between sweeps.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let (result, elapsed, sink) = sweep_once(
                store.clone(),
                policy,
                promoted.clone(),
                audit_sink,
                #[cfg(feature = "openfga")]
                emitter.clone(),
            )
            .await;
            audit_sink = sink;
            metrics.record_sweep(&result, elapsed);
            on_sweep(result);
        }
    }
}

/// One full sweep as the daemon runs it: the blocking pass (consolidation,
/// erasure rewrites, compaction audit events) on the blocking pool, then
/// — with an emitter — the async graph phase (write the tuples the pass
/// derived; delete the tuples of every erasure whose rows are gone; then,
/// back on the blocking pool, the `conversation_erased` audit event and
/// the marker removal). Returns the report, the wall-clock spent, and the
/// audit sink handed back. Runs the same way whether called by
/// [`Compactor::run`] or a test.
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
    let start = Instant::now();
    #[cfg(feature = "openfga")]
    let blocking_emitter = emitter.clone();
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
            run_sweep_hooked(
                &blocking_store,
                now_unix_nanos(),
                &policy,
                &promoted,
                &mut hooks,
            )
        };
        if let Ok(report) = &result {
            for event in &report.compaction_events {
                audit_sink.emit(event.clone());
            }
        }
        (result, audit_sink, tuples.into_inner())
    })
    .await
    .expect("compaction sweep task should not panic");

    #[cfg(feature = "openfga")]
    if let (Ok(report), Some(emitter)) = (&mut result, emitter.as_ref()) {
        graph_phase(&store, emitter, report, &mut audit_sink, tuples).await;
    }
    #[cfg(not(feature = "openfga"))]
    let GraphTuples = tuples;
    (result, start.elapsed(), audit_sink)
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
            Ok(written) => report.graph_tuples_emitted = written.tuples,
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
        report.erasures[index].finished = true;
    }
    report.errors.extend(errors);
}

/// Saturating `usize` → `u64` (lossless on 64-bit; saturates rather
/// than truncating on a theoretically wider target).
pub(crate) fn to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Build the RFC 0009 §3.6 audit event for a committed compaction
/// (RFC 0005 §3.7 `AuditPayload::Compaction`). The event timestamp is
/// the sweep's wall clock; the partition is the canonical
/// `year=…/month=…/day=…/hour=…` key (RFC 0005 §3.4).
fn compaction_audit_event(
    tenant: &str,
    now_unix_nanos: u64,
    partition: &PartitionKey,
    committed: &Committed,
    rows: u64,
) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        // `checked_add` so a saturated `now_unix_nanos` (year ~2554,
        // unreachable in practice — see `now_unix_nanos`) can't panic;
        // falls back to the epoch rather than aborting a sweep.
        timestamp: SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_nanos(now_unix_nanos))
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

/// `SystemTime::now()` as Unix nanoseconds (`0` if the clock is before
/// the epoch; saturated at `u64::MAX` past year 2554 — neither is
/// reachable in practice).
fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ourios_core::audit::ParamType;
    use ourios_core::record::{BodyKind, MinedRecord, Param};
    use ourios_core::tenant::TenantId;
    use ourios_parquet::{PartitionKey, Store, Writer};

    use super::*;

    /// A local [`Store`] rooted at `bucket` — the seam every sweep runs
    /// through (RFC 0019 §3.3).
    pub(super) fn store_at(bucket: &Path) -> Store {
        Store::local(bucket).expect("local store")
    }

    /// 2026-04-02T10:58:00 UTC (hour 10).
    pub(super) const TS0: u64 = 1_775_127_480_000_000_000;
    const HOUR: u64 = 3_600_000_000_000;
    /// Well past hour 10's end + grace.
    const NOW_SEALED: u64 = TS0 + 2 * HOUR;

    pub(super) fn rec(tenant: &str, template_id: u64, ts_ns: u64) -> MinedRecord {
        MinedRecord {
            tenant_id: TenantId::new(tenant),
            template_id,
            template_version: 1,
            severity_number: 9,
            severity_text: Some("INFO".to_string()),
            scope_name: Some("lib.cart".to_string()),
            scope_version: Some("1.0.0".to_string()),
            scope_attributes: Vec::new(),
            resource_schema_url: None,
            scope_schema_url: None,
            time_unix_nano: ts_ns,
            observed_time_unix_nano: Some(ts_ns + 1_000),
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            resource_attributes: Vec::new(),
            trace_id: None,
            span_id: None,
            flags: 0x01,
            event_name: None,
            body_kind: BodyKind::String,
            params: vec![Param {
                type_tag: ParamType::Num,
                value: "42".to_string(),
            }],
            separators: vec![String::new(), " ".to_string()],
            body: None,
            confidence: 1.0,
            lossy_flag: false,
        }
    }

    /// Write one committed file for `tenant` at `ts_ns` through the store seam.
    fn write_file(store: &Store, tenant: &str, template_id: u64, ts_ns: u64) {
        let record = rec(tenant, template_id, ts_ns);
        let mut w = Writer::open_in(store, PartitionKey::derive(&record).expect("derive"))
            .expect("open writer");
        w.append_records(&[record]).expect("append");
        w.close().expect("close");
    }

    /// Two committed files in one sealed partition = a candidate.
    fn write_sealed_candidate(store: &Store, tenant: &str) {
        write_file(store, tenant, 1, TS0);
        write_file(store, tenant, 2, TS0 + 1_000_000);
    }

    /// RFC0038.1 — one `sweep partitions` INTERNAL span per sweep.
    /// `run_sweep` is the sync body `spawn_blocking`ed in production; a scoped
    /// `with_default` subscriber captures the span it opens internally (the
    /// per-tenant / per-file loops below it stay span-free — RFC0038.2).
    #[test]
    fn rfc0038_1_sweep_emits_one_internal_span() {
        use opentelemetry::trace::{SpanKind, TracerProvider as _};
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        use tracing_subscriber::prelude::*;

        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        write_sealed_candidate(&store, "a");

        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("ourios-test")));

        tracing::subscriber::with_default(subscriber, || {
            run_sweep(&store, NOW_SEALED, &CompactionPolicy::default()).expect("sweep");
        });
        provider.force_flush().expect("spans flush");

        let spans = exporter.get_finished_spans().expect("spans exported");
        // The sweep path is our code only (filesystem + Parquet, no async
        // runtime / DataFusion), so the whole sweep emits exactly this one
        // span — asserting the total count catches any accidental extra
        // instrumentation (the "one span per sweep" contract, RFC0038.2).
        assert_eq!(spans.len(), 1, "exactly one span total, got {spans:?}");
        assert_eq!(spans[0].name.as_ref(), "sweep partitions");
        assert_eq!(
            spans[0].span_kind,
            SpanKind::Internal,
            "sweep partitions is an INTERNAL span",
        );
    }

    #[test]
    fn sweep_compacts_a_sealed_candidate() {
        // Arrange
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        write_sealed_candidate(&store, "a");

        // Act
        let report = run_sweep(&store, NOW_SEALED, &CompactionPolicy::default()).expect("sweep");

        // Assert
        assert_eq!(report.tenants_scanned, 1);
        assert_eq!(report.partitions_compacted, 1);
        assert_eq!(report.rows_compacted, 2);
        assert_eq!(
            report.files_compacted, 2,
            "both input files are merged away (the H4 signal)"
        );
    }

    #[test]
    fn sweep_reports_per_tenant_backlog_breakdown() {
        // Arrange — tenant "a" is a sealed candidate (compacts); tenant
        // "b" has a single file (not a candidate → 0 found, 0 compacted).
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        write_sealed_candidate(&store, "a");
        write_file(&store, "b", 1, TS0);

        // Act
        let report = run_sweep(&store, NOW_SEALED, &CompactionPolicy::default()).expect("sweep");

        // Assert — both tenants get a per-tenant entry; the residual
        // (candidates_found − partitions_compacted) is each one's backlog.
        let by_tenant: std::collections::HashMap<&str, &TenantSweep> = report
            .per_tenant
            .iter()
            .map(|t| (t.tenant.as_str(), t))
            .collect();
        let a = by_tenant.get("a").expect("tenant a present");
        assert_eq!(a.candidates_found, 1, "a's sealed partition is a candidate");
        assert_eq!(a.partitions_compacted, 1, "and it compacts → backlog 0");
        let b = by_tenant.get("b").expect("tenant b present");
        assert_eq!(b.candidates_found, 0, "b's single file is not a candidate");
        assert_eq!(b.partitions_compacted, 0, "→ backlog 0");
    }

    #[test]
    fn sweep_emits_a_compaction_audit_event() {
        // Arrange
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        write_sealed_candidate(&store, "a");

        // Act
        let report = run_sweep(&store, NOW_SEALED, &CompactionPolicy::default()).expect("sweep");

        // Assert — one RFC 0009 §3.6 compaction audit event, carrying
        // the partition / input set / output / generation / rows.
        assert_eq!(report.compaction_events.len(), 1);
        let event = &report.compaction_events[0];
        assert_eq!(event.tenant_id, TenantId::new("a"));
        let AuditPayload::Compaction {
            partition,
            input_files,
            output_file,
            generation,
            rows,
        } = &event.payload
        else {
            panic!("expected Compaction payload, got {:?}", event.payload);
        };
        // TS0 = 2026-04-02T10:58:00Z → hour 10.
        assert_eq!(partition, "year=2026/month=04/day=02/hour=10");
        assert_eq!(input_files.len(), 2, "two inputs merged away");
        assert!(
            output_file.ends_with(".parquet") && !input_files.contains(output_file),
            "output is the new consolidated file, distinct from the inputs",
        );
        assert_eq!(*generation, 2, "bootstrap gen 1, commit gen 2");
        assert_eq!(*rows, 2);
    }

    #[test]
    fn sweep_skips_an_unsealed_partition() {
        // Arrange — a candidate, but `now` is still inside its hour.
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        write_sealed_candidate(&store, "a");

        // Act
        let report = run_sweep(&store, TS0, &CompactionPolicy::default()).expect("sweep");

        // Assert
        assert_eq!(report.tenants_scanned, 1);
        assert_eq!(
            report.partitions_compacted, 0,
            "unsealed → nothing compacted"
        );
    }

    #[test]
    fn sweep_scans_every_tenant() {
        // Arrange — tenant "a" is a candidate; tenant "b" has one file
        // (nothing to consolidate).
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        write_sealed_candidate(&store, "a");
        write_file(&store, "b", 1, TS0);

        // Act
        let report = run_sweep(&store, NOW_SEALED, &CompactionPolicy::default()).expect("sweep");

        // Assert
        assert_eq!(report.tenants_scanned, 2, "both tenants scanned");
        assert_eq!(report.partitions_compacted, 1, "only tenant a's partition");
    }

    #[test]
    fn sweep_isolates_a_failing_tenant() {
        // Arrange — tenant "a" is a healthy sealed candidate; tenant
        // "b" has a malformed manifest.json, so planning it errors.
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        write_sealed_candidate(&store, "a");
        write_file(&store, "b", 1, TS0);
        // Corrupt b's manifest on the local store (its partition dir exists
        // after the write above); planning b then fails to parse it.
        let b_dir = PartitionKey::derive(&rec("b", 1, TS0))
            .expect("derive")
            .data_path(bucket.path());
        std::fs::write(b_dir.join(ourios_parquet::MANIFEST_FILENAME), b"not json")
            .expect("corrupt b's manifest");

        // Act
        let report = run_sweep(&store, NOW_SEALED, &CompactionPolicy::default()).expect("sweep");

        // Assert — b's failure is recorded, but a is still compacted.
        assert_eq!(report.tenants_scanned, 2);
        assert_eq!(
            report.partitions_compacted, 1,
            "tenant a compacted despite b failing"
        );
        assert_eq!(
            report.errors.len(),
            1,
            "tenant b's failure is recorded, not fatal"
        );
    }

    #[test]
    fn sweep_of_an_empty_store_is_zero() {
        // Arrange
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());

        // Act
        let report = run_sweep(&store, NOW_SEALED, &CompactionPolicy::default()).expect("sweep");

        // Assert
        assert_eq!(report, SweepReport::default());
    }

    #[test]
    fn run_executes_sweeps_until_cancelled() {
        // Arrange — a sealed candidate placed ~3h before the real wall
        // clock (floored to the hour so both files share a partition),
        // so it is sealed under `now_unix_nanos()` regardless of the
        // date the suite runs.
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let hour_start = (now_unix_nanos().saturating_sub(3 * HOUR) / HOUR) * HOUR;
        write_file(&store, "a", 1, hour_start + 1_000_000);
        write_file(&store, "a", 2, hour_start + 2_000_000);
        let compactor =
            Compactor::new(store, CompactionPolicy::default(), Duration::from_millis(5));
        let (tx, rx) = std::sync::mpsc::channel();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");

        // Act — spawn the loop, await its first sweep result, cancel.
        let compacted = rt.block_on(async move {
            let handle = tokio::spawn(compactor.run(move |result| {
                let _ = tx.send(result.map(|r| r.partitions_compacted));
            }));
            let first =
                tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(5)))
                    .await
                    .expect("join")
                    .expect("a sweep ran within 5s");
            handle.abort();
            first
        });

        // Assert — the loop ran a sweep that compacted the candidate.
        assert_eq!(compacted.expect("sweep ok"), 1);
    }
}

#[cfg(all(test, feature = "openfga"))]
mod graph_tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::State;
    use axum::routing::post;
    use ourios_core::audit::{AuditPayload, SharedAuditSink};
    use ourios_core::auth::openfga::{OpenFgaSpec, TupleKey, build_openfga_config};
    use ourios_core::otlp::any_value::Value;
    use ourios_core::otlp::{AnyValue, KeyValue};
    use ourios_core::record::MinedRecord;
    use ourios_parquet::{
        CompactionPolicy, PartitionKey, PromotedAttributes, PromotedKey, Reader, Store, Writer,
    };
    use serde_json::json;

    use super::{ErasurePhase, pending_erasures, request_erasure, sweep_once};
    use crate::graph_emitter::GraphEmitter;

    /// A fake `OpenFGA` store: `/write` applies writes/deletes (asserting the
    /// ≤ 100 chunk), `/read` answers by object.
    #[derive(Clone, Default)]
    struct Fake {
        tuples: Arc<Mutex<Vec<TupleKey>>>,
        writes: Arc<Mutex<Vec<usize>>>,
    }

    fn json(value: &serde_json::Value) -> ([(&'static str, &'static str); 1], String) {
        ([("content-type", "application/json")], value.to_string())
    }

    async fn write(
        State(fake): State<Fake>,
        body: axum::body::Bytes,
    ) -> ([(&'static str, &'static str); 1], String) {
        let request: serde_json::Value = serde_json::from_slice(&body).expect("json");
        let mut tuples = fake.tuples.lock().expect("lock");
        if let Some(keys) = request["writes"]["tuple_keys"].as_array() {
            assert!(keys.len() <= 100, "RFC 0047 §3.3: ≤ 100 tuples per Write");
            assert_eq!(request["writes"]["on_duplicate"], "ignore");
            fake.writes.lock().expect("lock").push(keys.len());
            for key in keys {
                let key: TupleKey = serde_json::from_value(key.clone()).expect("tuple");
                if !tuples.contains(&key) {
                    tuples.push(key);
                }
            }
        }
        if let Some(keys) = request["deletes"]["tuple_keys"].as_array() {
            assert!(keys.len() <= 100);
            assert_eq!(request["deletes"]["on_missing"], "ignore");
            for key in keys {
                let key: TupleKey = serde_json::from_value(key.clone()).expect("tuple");
                tuples.retain(|t| *t != key);
            }
        }
        json(&json!({}))
    }

    async fn read(
        State(fake): State<Fake>,
        body: axum::body::Bytes,
    ) -> ([(&'static str, &'static str); 1], String) {
        let request: serde_json::Value = serde_json::from_slice(&body).expect("json");
        let object = request["tuple_key"]["object"].as_str().expect("object");
        let tuples = fake.tuples.lock().expect("lock");
        let matching: Vec<serde_json::Value> = tuples
            .iter()
            .filter(|t| t.object == object)
            .map(|t| json!({ "key": t }))
            .collect();
        json(&json!({ "tuples": matching, "continuation_token": "" }))
    }

    async fn serve(fake: Fake) -> String {
        let app = Router::new()
            .route("/stores/{store}/write", post(write))
            .route("/stores/{store}/read", post(read))
            .with_state(fake);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        url
    }

    fn emitter(url: &str) -> Arc<GraphEmitter> {
        use ourios_core::auth::openfga::{VisibilityObjectSpec, VisibilitySpec};
        let config = build_openfga_config(&OpenFgaSpec {
            api_url: Some(url.to_string()),
            store_id: Some("s".to_string()),
            request_timeout_secs: Some("2".to_string()),
            visibility: VisibilitySpec {
                objects: vec![VisibilityObjectSpec {
                    object_type: Some("conversation".to_string()),
                    column: Some("attr.gen_ai.conversation.id".to_string()),
                }],
                ..VisibilitySpec::default()
            },
            ..OpenFgaSpec::default()
        })
        .expect("config");
        Arc::new(
            GraphEmitter::from_config(&config)
                .expect("client")
                .expect("bound"),
        )
    }

    fn kv(key: &str, value: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(value.to_string())),
            }),
            ..Default::default()
        }
    }

    fn promoted() -> PromotedAttributes {
        PromotedAttributes::new_typed(
            [],
            [
                PromotedKey::string("gen_ai.conversation.id".to_string()),
                PromotedKey::string("user.hash".to_string()),
            ],
        )
    }

    /// One file per call, in the sealed hour partition, `rows` records with
    /// `conversation`/`user` attributes.
    fn write_rows(store: &Store, conversation: &str, user: &str, agent: Option<&str>, n: u64) {
        let rows: Vec<MinedRecord> = (0..n)
            .map(|i| {
                let mut r = super::tests::rec("acme", 1, super::tests::TS0 + i * 1_000);
                r.attributes = vec![
                    kv("gen_ai.conversation.id", conversation),
                    kv("user.hash", user),
                ];
                if let Some(agent) = agent {
                    r.attributes.push(kv("gen_ai.agent.id", agent));
                }
                r
            })
            .collect();
        let partition = PartitionKey::derive(&rows[0]).expect("derive");
        let mut w = Writer::open_in_with_promoted(
            store,
            partition,
            ourios_parquet::DEFAULT_ZSTD_LEVEL,
            promoted(),
        )
        .expect("open writer");
        w.append_records(&rows).expect("append");
        w.close().expect("close");
    }

    fn live_rows(store: &Store, bucket: &Path) -> Vec<MinedRecord> {
        let mut rows = Vec::new();
        for key in store.list_blocking(Some("data/")).expect("list") {
            if !key.ends_with(".parquet") {
                continue;
            }
            let bytes = store.get_blocking(&key).expect("get");
            let reader = Reader::open_bytes(bytes.into()).expect("open");
            rows.extend(reader.read_all().expect("read"));
        }
        let _ = bucket;
        rows
    }

    /// Scenario RFC0047.10 — the sweep feeds the graph: after a sweep the
    /// `parent`, `participant`, `actor` (and binding, and tool) tuples exist
    /// with tenant-prefixed ids; a second sweep writes nothing new (the
    /// partition is consolidated, nothing is rewritten); every `Write` is
    /// ≤ 100 tuples. See `docs/rfcs/0047-rebac-resolver-and-graph-visibility.md` §5.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rfc0047_10_sweep_emits_tuples_idempotently() {
        let fake = Fake::default();
        let url = serve(fake.clone()).await;
        let bucket = tempfile::TempDir::new().expect("temp");
        let store = super::tests::store_at(bucket.path());
        // Two files → a sealed candidate; 130 distinct conversations so the
        // tuple set spans more than one chunk.
        write_rows(&store, "c-1", "alice", Some("bot"), 3);
        for i in 0..130 {
            write_rows(&store, &format!("c-{}", i + 10), "bob", None, 1);
        }
        let emitter = emitter(&url);
        let (result, _, sink) = sweep_once(
            store.clone(),
            CompactionPolicy::default(),
            promoted(),
            Box::new(SharedAuditSink::new()),
            Some(Arc::clone(&emitter)),
        )
        .await;
        let report = result.expect("sweep");
        assert_eq!(report.partitions_compacted, 1, "{report:?}");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let tuples = fake.tuples.lock().expect("lock").clone();
        let t = |u: &str, r: &str, o: &str| TupleKey::new(u, r, o);
        for tuple in [
            t("tenant:acme", "parent", "conversation:acme/c-1"),
            t("user:alice", "participant", "conversation:acme/c-1"),
            t("user:alice", "scoped_reader", "tenant:acme"),
            t("agent:bot", "actor", "conversation:acme/c-1"),
            t("agent:bot", "scoped_reader", "tenant:acme"),
            t("tenant:acme", "parent", "conversation:acme/c-42"),
            t("user:bob", "participant", "conversation:acme/c-42"),
            t("user:bob", "scoped_reader", "tenant:acme"),
            t("tenant:acme", "parent", "tool:acme/query_logs"),
        ] {
            assert!(tuples.contains(&tuple), "missing {tuple:?}");
        }
        assert_eq!(
            report.graph_tuples_emitted,
            tuples.len(),
            "every tuple sent once"
        );
        let writes = fake.writes.lock().expect("lock").clone();
        assert!(
            writes.len() >= 2 && writes.iter().all(|n| *n <= 100),
            "{writes:?}"
        );

        // Second sweep: nothing to consolidate, nothing rewritten, nothing sent.
        let before = fake.writes.lock().expect("lock").len();
        let (result, _, _) = sweep_once(
            store.clone(),
            CompactionPolicy::default(),
            promoted(),
            sink,
            Some(emitter),
        )
        .await;
        let report = result.expect("sweep");
        assert_eq!(report.partitions_compacted, 0);
        assert_eq!(report.graph_tuples_emitted, 0);
        assert_eq!(
            fake.writes.lock().expect("lock").len(),
            before,
            "nothing new"
        );
        assert_eq!(fake.tuples.lock().expect("lock").len(), tuples.len());
    }

    /// Scenario RFC0047.11 — erasure removes tuples after rows: a requested
    /// erasure rewrites the tenant's partitions with the conversation's rows
    /// dropped, then deletes its tuples, then writes the `conversation_erased`
    /// audit event after every compaction event, then removes the marker; the
    /// object is unlisted (no tuple on it remains) and other conversations'
    /// tuples are untouched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::too_many_lines)] // one store, one graph: rows → tuples → audit → marker in sequence
    async fn rfc0047_11_erasure_removes_tuples_after_rows() {
        let fake = Fake::default();
        let url = serve(fake.clone()).await;
        let bucket = tempfile::TempDir::new().expect("temp");
        let store = super::tests::store_at(bucket.path());
        write_rows(&store, "c-1", "alice", Some("bot"), 3);
        write_rows(&store, "c-2", "bob", None, 2);
        let emitter = emitter(&url);
        let audit = SharedAuditSink::new();
        // Sweep 1: consolidate + feed the graph.
        let (result, _, sink) = sweep_once(
            store.clone(),
            CompactionPolicy::default(),
            promoted(),
            Box::new(audit.clone()),
            Some(Arc::clone(&emitter)),
        )
        .await;
        result.expect("sweep");
        assert!(
            fake.tuples
                .lock()
                .expect("lock")
                .iter()
                .any(|t| t.object == "conversation:acme/c-1")
        );
        let _ = audit.drain();

        // Request the erasure of c-1; sweep 2 performs it. A repeated request
        // is a no-op (create-if-absent) — it never resets a marker's phase.
        request_erasure(&store, "acme", "c-1").expect("request");
        request_erasure(&store, "acme", "c-1").expect("repeat");
        assert_eq!(pending_erasures(&store).expect("pending").len(), 1);
        store
            .put_blocking(
                &super::erasure_marker_key("acme", "c-9"),
                b" { \"phase\" : \"tuples\" }\n".to_vec(),
            )
            .expect("hand-written marker");
        request_erasure(&store, "acme", "c-9").expect("repeat");
        let phases: Vec<_> = pending_erasures(&store)
            .expect("pending")
            .into_iter()
            .map(|r| (r.conversation_id, r.phase))
            .collect();
        assert!(
            phases.contains(&("c-9".to_string(), ErasurePhase::Tuples)),
            "{phases:?}"
        );
        store
            .delete_blocking(&super::erasure_marker_key("acme", "c-9"))
            .expect("cleanup");
        let (result, _, _) = sweep_once(
            store.clone(),
            CompactionPolicy::default(),
            promoted(),
            sink,
            Some(Arc::clone(&emitter)),
        )
        .await;
        let report = result.expect("sweep");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.erasures.len(), 1, "{report:?}");
        let outcome = &report.erasures[0];
        assert_eq!(outcome.request.conversation_id, "c-1");
        assert_eq!(outcome.rows_dropped, 3);
        assert_eq!(outcome.partitions_rewritten, 1);
        assert_eq!(outcome.phase, ErasurePhase::Tuples);
        assert_eq!(
            outcome.tuples_deleted,
            Some(3),
            "parent + participant + actor"
        );
        assert!(outcome.finished);

        // Rows: only c-2's remain.
        let rows = live_rows(&store, bucket.path());
        assert_eq!(rows.len(), 2, "c-1's three rows are gone");
        assert!(rows.iter().all(|r| {
            r.attributes.iter().any(|kv| {
                kv.key == "gen_ai.conversation.id"
                    && kv.value.as_ref().and_then(|v| v.value.as_ref())
                        == Some(&Value::StringValue("c-2".to_string()))
            })
        }));
        // Tuples: no tuple on the object remains; c-2's untouched; the
        // binding tuples (tenant-scoped, not object-scoped) stay.
        let tuples = fake.tuples.lock().expect("lock").clone();
        assert!(!tuples.iter().any(|t| t.object == "conversation:acme/c-1"));
        assert!(tuples.iter().any(|t| t.object == "conversation:acme/c-2"));
        assert!(tuples.contains(&TupleKey::new("user:alice", "scoped_reader", "tenant:acme")));
        // Marker gone; nothing pending.
        assert!(pending_erasures(&store).expect("pending").is_empty());
        // Audit order: the erasure event comes after every compaction
        // event of the sweep (the rewrite is itself audited and counted as
        // a compaction), carrying the counts.
        let events = audit.drain();
        assert!(
            events
                .iter()
                .any(|e| matches!(e.payload, AuditPayload::Compaction { .. })),
            "the erasure rewrite is audited as a compaction: {events:?}"
        );
        assert_eq!(
            report.partitions_compacted, 1,
            "and counted in the sweep's IO accounting"
        );
        assert!(report.bytes_read > 0);
        let erased = events
            .iter()
            .position(|e| matches!(e.payload, AuditPayload::ConversationErased { .. }))
            .expect("conversation_erased event");
        assert_eq!(erased, events.len() - 1, "last event of the sweep");
        match &events[erased].payload {
            AuditPayload::ConversationErased {
                conversation_id,
                partitions_rewritten,
                rows_dropped,
                tuples_deleted,
            } => {
                assert_eq!(conversation_id, "c-1");
                assert_eq!(*partitions_rewritten, 1);
                assert_eq!(*rows_dropped, 3);
                assert_eq!(*tuples_deleted, 3);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(events[erased].tenant_id.as_str(), "acme");
    }

    /// RFC0047.11 (raw ids): a conversation whose id can never be a graph
    /// object (here: whitespace) still has its rows erased — matched on the
    /// stored value — with zero tuples to delete and an honest audit event.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rfc0047_11_erasure_matches_raw_ids() {
        let fake = Fake::default();
        let url = serve(fake.clone()).await;
        let bucket = tempfile::TempDir::new().expect("temp");
        let store = super::tests::store_at(bucket.path());
        write_rows(&store, "odd id", "alice", None, 2);
        write_rows(&store, "c-2", "bob", None, 1);
        let emitter = emitter(&url);
        let audit = SharedAuditSink::new();
        let (result, _, sink) = sweep_once(
            store.clone(),
            CompactionPolicy::default(),
            promoted(),
            Box::new(audit.clone()),
            Some(Arc::clone(&emitter)),
        )
        .await;
        result.expect("sweep");
        assert!(
            !fake
                .tuples
                .lock()
                .expect("lock")
                .iter()
                .any(|t| t.object.contains("odd")),
            "no tuple was ever minted for a non-object-id conversation"
        );
        request_erasure(&store, "acme", "odd id").expect("request");
        let (result, _, _) = sweep_once(
            store.clone(),
            CompactionPolicy::default(),
            promoted(),
            sink,
            Some(emitter),
        )
        .await;
        let report = result.expect("sweep");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let outcome = &report.erasures[0];
        assert_eq!(outcome.rows_dropped, 2, "rows matched on the raw value");
        assert_eq!(outcome.tuples_deleted, Some(0));
        assert!(outcome.finished);
        assert_eq!(live_rows(&store, bucket.path()).len(), 1);
    }
}
