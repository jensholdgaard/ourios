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

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use ourios_core::audit::{AuditEvent, AuditPayload, AuditSink, NoOpAuditSink};
use ourios_core::tenant::TenantId;
use ourios_parquet::{
    Committed, CompactionError, CompactionPolicy, PartitionKey, PromotedAttributes, Store,
    compact_partition_with_promoted, gc_orphans, percent_decode_tenant, plan_candidates,
};

use crate::metrics::CompactionMetrics;

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
/// ([`compact_partition_with_promoted`]), accumulating a [`SweepReport`].
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
// RFC 0038: one span per compaction sweep — coarse and periodic. Opened inside
// the callee (the tick `spawn_blocking`s this), and the per-tenant / per-file
// loops below stay span-free.
#[tracing::instrument(
    skip_all,
    name = "ourios.compaction.sweep",
    fields(otel.kind = "internal")
)]
pub fn run_sweep_with_promoted(
    store: &Store,
    now_unix_nanos: u64,
    policy: &CompactionPolicy,
    promoted: &PromotedAttributes,
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
            match compact_partition_with_promoted(store, &partition, promoted) {
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
    Ok(report)
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
}

impl std::fmt::Debug for Compactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `AuditSink` is not `Debug`; name it without its contents.
        f.debug_struct("Compactor")
            .field("store", &self.store)
            .field("policy", &self.policy)
            .field("interval", &self.interval)
            .field("promoted", &self.promoted)
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
            store,
            policy,
            interval,
            promoted: PromotedAttributes::default(),
            audit_sink: Box::new(NoOpAuditSink::new()),
        }
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
        // Destructure up front into owned locals: `audit_sink` moves into each
        // sweep's blocking task and back out, and `store`/`policy` are used
        // inside the loop without holding `self`.
        let Self {
            store,
            policy,
            interval,
            promoted,
            // Own the audit sink locally so it can move into each sweep's
            // blocking task and back out. Its `emit` performs Parquet `put`s
            // through the store — now S3 network I/O (RFC 0019 slice 2d) — so it
            // must run on the blocking pool alongside the sweep, never on the
            // async task where slow S3 would stall the runtime.
            mut audit_sink,
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
            // `Store` is a cheap `Arc` handle; clone it into the blocking task
            // (compaction is blocking I/O). `policy` is `Copy`, so the `move`
            // closure copies it and the outer binding stays valid each loop.
            let store = store.clone();
            let promoted = promoted.clone();
            let (result, elapsed, sink) = tokio::task::spawn_blocking(move || {
                let start = Instant::now();
                let result = run_sweep_with_promoted(&store, now_unix_nanos(), &policy, &promoted);
                // Emit the committed-compaction audit events here, on the
                // blocking pool, since the sink's `put`s are blocking store I/O.
                if let Ok(report) = &result {
                    for event in &report.compaction_events {
                        audit_sink.emit(event.clone());
                    }
                }
                (result, start.elapsed(), audit_sink)
            })
            .await
            .expect("compaction sweep task should not panic");
            audit_sink = sink;
            metrics.record_sweep(&result, elapsed);
            on_sweep(result);
        }
    }
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
    fn store_at(bucket: &Path) -> Store {
        Store::local(bucket).expect("local store")
    }

    /// 2026-04-02T10:58:00 UTC (hour 10).
    const TS0: u64 = 1_775_127_480_000_000_000;
    const HOUR: u64 = 3_600_000_000_000;
    /// Well past hour 10's end + grace.
    const NOW_SEALED: u64 = TS0 + 2 * HOUR;

    fn rec(tenant: &str, template_id: u64, ts_ns: u64) -> MinedRecord {
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
