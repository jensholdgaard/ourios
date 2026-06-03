//! Background compaction runner (RFC 0009 §3.2).
//!
//! [`run_sweep`] is one pass over the whole store — for every tenant,
//! select its sealed candidate partitions and consolidate them. It is
//! synchronous (blocking filesystem + Parquet work) and deterministic,
//! so it's the unit the tests exercise. [`Compactor::run`] is the thin
//! daemon: it calls `run_sweep` on a fixed cadence via `spawn_blocking`
//! and hands each result to a caller-supplied observer (metrics/log
//! wiring is a later slice).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use ourios_parquet::{
    CompactionError, CompactionPolicy, compact_partition, percent_decode_tenant, plan_candidates,
};

use crate::metrics::CompactionMetrics;

/// Failure during a compaction sweep.
#[derive(Debug)]
#[non_exhaustive]
pub enum IngestError {
    /// Planning or consolidating a partition failed.
    Compaction(CompactionError),
    /// Scanning the store's tenant directories failed.
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
    /// Per-tenant / per-partition failures encountered, formatted for
    /// logging. A sweep is **resilient**: one bad tenant or partition
    /// is recorded here and skipped, never aborting the rest (else a
    /// persistent error would starve every later tenant, since the
    /// daemon just retries the same sweep next tick).
    pub errors: Vec<String>,
}

/// Run one compaction sweep over `bucket_root`, as of wall-clock
/// `now_unix_nanos`: for each tenant, select its sealed candidate
/// partitions ([`plan_candidates`]) and consolidate each
/// ([`compact_partition`]), accumulating a [`SweepReport`].
///
/// Resilient: a tenant whose planning fails, or a partition whose
/// consolidation fails, is recorded in [`SweepReport::errors`] and
/// skipped — the sweep continues with the rest. Only a failure to
/// scan the store itself (the tenant listing) is fatal.
///
/// # Errors
///
/// [`IngestError`] only if the store's tenant directory can't be
/// scanned; per-tenant / per-partition failures are collected into
/// the returned report, not propagated.
pub fn run_sweep(
    bucket_root: &Path,
    now_unix_nanos: u64,
    policy: &CompactionPolicy,
) -> Result<SweepReport, IngestError> {
    let mut report = SweepReport::default();
    for tenant in tenants(bucket_root)? {
        report.tenants_scanned += 1;
        let candidates = match plan_candidates(bucket_root, &tenant, now_unix_nanos, policy) {
            Ok(candidates) => candidates,
            Err(e) => {
                report.errors.push(format!("plan tenant {tenant:?}: {e}"));
                continue;
            }
        };
        for partition in candidates {
            match compact_partition(bucket_root, &partition) {
                Ok(outcome) => {
                    if outcome.committed.is_some() {
                        report.partitions_compacted += 1;
                        report.files_compacted += to_u64(outcome.files_before);
                        report.rows_compacted += outcome.rows;
                    }
                    report.gc_failures += outcome.gc_failures;
                }
                Err(e) => report.errors.push(format!(
                    "compact {tenant:?} {:04}-{:02}-{:02}T{:02}: {e}",
                    partition.year, partition.month, partition.day, partition.hour,
                )),
            }
        }
    }
    Ok(report)
}

/// Raw tenant ids present in the store, decoded from the
/// `data/tenant_id=<enc>/` directory names (sorted, so a sweep is
/// deterministic). Names that don't decode are skipped (not Ourios
/// output); a missing `data/` directory yields none.
fn tenants(bucket_root: &Path) -> Result<Vec<String>, IngestError> {
    let data = bucket_root.join("data");
    let entries = match std::fs::read_dir(&data) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(IngestError::Io {
                op: "read_dir",
                path: data,
                source,
            });
        }
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| IngestError::Io {
            op: "read_dir entry",
            path: data.clone(),
            source,
        })?;
        let is_dir = entry
            .file_type()
            .map_err(|source| IngestError::Io {
                op: "file_type",
                path: entry.path(),
                source,
            })?
            .is_dir();
        if !is_dir {
            continue;
        }
        if let Some(tenant) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix("tenant_id="))
            .and_then(percent_decode_tenant)
        {
            out.push(tenant);
        }
    }
    out.sort();
    Ok(out)
}

/// Background compaction daemon (RFC 0009 §3.2): sweeps the store on a
/// fixed cadence. Hosted in the ingester role so it never lands on the
/// ack-latency hot path.
#[derive(Debug, Clone)]
pub struct Compactor {
    bucket_root: PathBuf,
    policy: CompactionPolicy,
    interval: Duration,
}

impl Compactor {
    /// A compactor sweeping `bucket_root` every `interval` under
    /// `policy`.
    pub fn new(
        bucket_root: impl Into<PathBuf>,
        policy: CompactionPolicy,
        interval: Duration,
    ) -> Self {
        Self {
            bucket_root: bucket_root.into(),
            policy,
            interval,
        }
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
        // Built (and zero-seeded) once, before the loop, so the metric
        // set is visible to the exporter even before the first sweep.
        let metrics = CompactionMetrics::new();
        let mut ticker = tokio::time::interval(self.interval);
        // A maintenance sweep that overruns `interval` must not make
        // the next ticks fire back-to-back (the default `Burst`) —
        // that would pile sustained compaction load after any slow
        // pass. `Delay` keeps a full `interval` gap between sweeps.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let bucket = self.bucket_root.clone();
            let policy = self.policy;
            let (result, elapsed) = tokio::task::spawn_blocking(move || {
                let start = Instant::now();
                let result = run_sweep(&bucket, now_unix_nanos(), &policy);
                (result, start.elapsed())
            })
            .await
            .expect("compaction sweep task should not panic");
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
    use ourios_core::audit::ParamType;
    use ourios_core::record::{BodyKind, MinedRecord, Param};
    use ourios_core::tenant::TenantId;
    use ourios_parquet::{PartitionKey, Writer};

    use super::*;

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

    /// Write one committed file for `tenant` at `ts_ns`.
    fn write_file(bucket: &Path, tenant: &str, template_id: u64, ts_ns: u64) {
        let record = rec(tenant, template_id, ts_ns);
        let mut w = Writer::open(bucket, PartitionKey::derive(&record).expect("derive"))
            .expect("open writer");
        w.append_records(&[record]).expect("append");
        w.close().expect("close");
    }

    /// Two committed files in one sealed partition = a candidate.
    fn write_sealed_candidate(bucket: &Path, tenant: &str) {
        write_file(bucket, tenant, 1, TS0);
        write_file(bucket, tenant, 2, TS0 + 1_000_000);
    }

    #[test]
    fn sweep_compacts_a_sealed_candidate() {
        // Arrange
        let bucket = tempfile::tempdir().expect("temp");
        write_sealed_candidate(bucket.path(), "a");

        // Act
        let report =
            run_sweep(bucket.path(), NOW_SEALED, &CompactionPolicy::default()).expect("sweep");

        // Assert
        assert_eq!(report.tenants_scanned, 1);
        assert_eq!(report.partitions_compacted, 1);
        assert_eq!(report.rows_compacted, 2);
    }

    #[test]
    fn sweep_skips_an_unsealed_partition() {
        // Arrange — a candidate, but `now` is still inside its hour.
        let bucket = tempfile::tempdir().expect("temp");
        write_sealed_candidate(bucket.path(), "a");

        // Act
        let report = run_sweep(bucket.path(), TS0, &CompactionPolicy::default()).expect("sweep");

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
        write_sealed_candidate(bucket.path(), "a");
        write_file(bucket.path(), "b", 1, TS0);

        // Act
        let report =
            run_sweep(bucket.path(), NOW_SEALED, &CompactionPolicy::default()).expect("sweep");

        // Assert
        assert_eq!(report.tenants_scanned, 2, "both tenants scanned");
        assert_eq!(report.partitions_compacted, 1, "only tenant a's partition");
    }

    #[test]
    fn sweep_isolates_a_failing_tenant() {
        // Arrange — tenant "a" is a healthy sealed candidate; tenant
        // "b" has a malformed manifest.json, so planning it errors.
        let bucket = tempfile::tempdir().expect("temp");
        write_sealed_candidate(bucket.path(), "a");
        write_file(bucket.path(), "b", 1, TS0);
        let b_dir = PartitionKey::derive(&rec("b", 1, TS0))
            .expect("derive")
            .data_path(bucket.path());
        std::fs::write(b_dir.join(ourios_parquet::MANIFEST_FILENAME), b"not json")
            .expect("corrupt b's manifest");

        // Act
        let report =
            run_sweep(bucket.path(), NOW_SEALED, &CompactionPolicy::default()).expect("sweep");

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

        // Act
        let report =
            run_sweep(bucket.path(), NOW_SEALED, &CompactionPolicy::default()).expect("sweep");

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
        let hour_start = (now_unix_nanos().saturating_sub(3 * HOUR) / HOUR) * HOUR;
        write_file(bucket.path(), "a", 1, hour_start + 1_000_000);
        write_file(bucket.path(), "a", 2, hour_start + 2_000_000);
        let compactor = Compactor::new(
            bucket.path(),
            CompactionPolicy::default(),
            Duration::from_millis(5),
        );
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
