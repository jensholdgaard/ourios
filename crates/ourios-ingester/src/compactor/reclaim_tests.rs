//! The sweep's orphan-reclaim pass: the RFC 0038 §3.9 `reclaim orphans`
//! span (RFC0038.18) and `ourios.compaction.orphan.reclaimed`
//! (RFC0009.25).

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::trace::Status;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use opentelemetry_sdk::trace::SpanData;
use ourios_core::audit::NoOpAuditSink;
use ourios_semconv as semconv;

use super::tenant_metrics_tests::{by_tenant, tenants};
use super::tests::{NOW_SEALED, attr, only_span, store_at, target, traced, write_sealed_candidate};
use super::*;

/// A sealed candidate for `tenant` plus `orphans` interrupted publishes
/// (`*.parquet.tmp`, an orphan whatever the manifest says) beside it; the
/// partition's directory.
fn candidate_with_orphans(store: &Store, root: &Path, tenant: &str, orphans: usize) -> PathBuf {
    write_sealed_candidate(store, tenant);
    let prefix = format!("data/tenant_id={tenant}");
    let listed = store.list_blocking(Some(&prefix)).expect("list");
    let (partition, _) = listed[0].rsplit_once('/').expect("partition prefix");
    for n in 0..orphans {
        let key = format!("{partition}/orphan-{n}.parquet.tmp");
        store.put_blocking(&key, b"torn".to_vec()).expect("orphan");
    }
    root.join(partition)
}

/// The `(reclaimed_count, failed_count)` `reclaim orphans` carries.
fn reclaim_counts(span: &SpanData) -> [Option<opentelemetry::Value>; 2] {
    [
        semconv::OURIOS_COMPACTION_ORPHAN_RECLAIMED_COUNT,
        semconv::OURIOS_COMPACTION_ORPHAN_FAILED_COUNT,
    ]
    .map(|key| attr(span, key))
}

fn counts(reclaimed: i64, failed: i64) -> [Option<opentelemetry::Value>; 2] {
    [reclaimed, failed].map(|n| Some(opentelemetry::Value::I64(n)))
}

/// The `*.parquet.tmp` files in `dir`.
fn tmp_files(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .expect("read dir")
        .filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.file_name().to_string_lossy().ends_with(".parquet.tmp"))
        })
        .count()
}

/// RFC0038.18 — `reclaim orphans` counts the orphans it deleted, starts
/// after planning and after the last commit, and lies within the sweep.
#[test]
fn the_reclaim_phase_counts_the_orphans_it_deleted() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let dir = candidate_with_orphans(&store, bucket.path(), "a", 2);
    let mut last_commit = None;

    let (result, spans) = traced(|| {
        run_sweep_committing(
            &store,
            SweepClock::sealed_at(NOW_SEALED),
            &CompactionPolicy::default(),
            &PromotedAttributes::default(),
            &mut SweepHooks::default(),
            &mut |_| last_commit = Some(SystemTime::now()),
        )
    });

    let report = result.expect("sweep");
    assert_eq!(
        (report.partitions_compacted, report.orphans_reclaimed),
        (1, 2)
    );
    assert_eq!(tmp_files(&dir), 0, "both orphans deleted");
    let sweep = only_span(&spans, "sweep partitions");
    let plan = only_span(&spans, "plan partitions");
    let reclaim = only_span(&spans, "reclaim orphans");
    assert_eq!(
        (reclaim_counts(reclaim), &reclaim.status),
        (counts(2, 0), &Status::Unset)
    );
    assert!(sweep.start_time <= reclaim.start_time && reclaim.end_time <= sweep.end_time);
    assert!(plan.end_time <= reclaim.start_time, "after planning");
    assert!(
        last_commit.expect("a partition committed") <= reclaim.start_time,
        "after the last commit"
    );
}

/// RFC0038.18 — with nothing to reclaim, the phase is still there with
/// zero counts: a candidate without orphans, and a store without tenants.
#[test]
fn an_idle_reclaim_phase_still_spans_with_zero_counts() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let empty = tempfile::tempdir().expect("temp");
    let empty = store_at(empty.path());
    write_sealed_candidate(&store, "a");

    for store in [&store, &empty] {
        let (result, spans) = traced(|| run_sweep(store, NOW_SEALED, &CompactionPolicy::default()));

        assert_eq!(result.expect("sweep").orphans_reclaimed, 0);
        let reclaim = only_span(&spans, "reclaim orphans");
        assert_eq!(
            (reclaim_counts(reclaim), &reclaim.status),
            (counts(0, 0), &Status::Unset)
        );
    }
}

/// RFC0038.18 — an orphan whose delete fails marks the phase `Error` with
/// `error.type = io`; the sweep span stays `Unset`.
#[cfg(unix)]
#[test]
fn a_failed_orphan_delete_marks_the_reclaim_phase_error() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let dir = candidate_with_orphans(&store, bucket.path(), "a", 1);
    let read_only = ReadOnly::new(dir.clone());

    let (result, spans) = traced(|| run_sweep(&store, NOW_SEALED, &CompactionPolicy::default()));
    drop(read_only);

    result.expect("a failed delete does not fail the sweep");
    assert_eq!(tmp_files(&dir), 1, "the orphan is left for a later pass");
    let reclaim = only_span(&spans, "reclaim orphans");
    assert!(
        matches!(reclaim.status, Status::Error { .. }),
        "{:?}",
        reclaim.status
    );
    assert_eq!(
        (
            attr(reclaim, "error.type"),
            reclaim_counts(reclaim),
            &only_span(&spans, "sweep partitions").status,
        ),
        (Some("io".into()), counts(0, 1), &Status::Unset),
    );
}

/// A meter provider over an in-memory exporter, and the compaction
/// instruments on it.
fn metered() -> (
    InMemoryMetricExporter,
    SdkMeterProvider,
    Arc<CompactionMetrics>,
) {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .build();
    let metrics = Arc::new(CompactionMetrics::from_meter(
        &provider.meter_with_scope(ourios_core::telemetry::scope("ourios.compaction")),
    ));
    (exporter, provider, metrics)
}

/// One sweep over `store`, recorded as the daemon records it; the metrics
/// exported after it.
async fn sweep_metrics(store: &Store) -> Vec<ResourceMetrics> {
    let (exporter, provider, metrics) = metered();
    let (result, elapsed, _) = sweep_recorded(
        target(store),
        Box::new(NoOpAuditSink),
        Some(Arc::clone(&metrics)),
    )
    .await;
    metrics.record_sweep_outcome(&result, elapsed);
    result.expect("sweep");
    provider.force_flush().expect("flush");
    exporter.get_finished_metrics().expect("collect")
}

/// RFC0009.25 — reclaimed orphans are counted under their tenant with no
/// `error.type`; a tenant with none adds no point, and
/// `ourios.compaction.orphan.files` keeps only its zero seed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0009_25_reclaimed_orphans_are_counted_per_tenant() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    candidate_with_orphans(&store, bucket.path(), "a", 2);
    write_sealed_candidate(&store, "b");

    let rms = sweep_metrics(&store).await;

    let reclaimed = semconv::OURIOS_COMPACTION_ORPHAN_RECLAIMED;
    assert_eq!(by_tenant(&rms, reclaimed, None), tenants(&[("a", 2)]));
    assert_eq!(
        by_tenant(&rms, reclaimed, Some(("error.type", "io"))),
        tenants(&[])
    );
    assert_eq!(
        by_tenant(&rms, semconv::OURIOS_COMPACTION_ORPHAN_FILES, None),
        tenants(&[("", 0)])
    );
}

/// RFC0009.25 — a failed orphan delete is counted with `error.type = io`
/// and the file stays; the next sweep deletes it and counts it without.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0009_25_a_failed_orphan_delete_carries_error_type() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let dir = candidate_with_orphans(&store, bucket.path(), "a", 1);
    let reclaimed = semconv::OURIOS_COMPACTION_ORPHAN_RECLAIMED;
    let failed = Some(("error.type", "io"));

    let read_only = ReadOnly::new(dir.clone());
    let first = sweep_metrics(&store).await;
    drop(read_only);
    assert_eq!(tmp_files(&dir), 1, "the orphan is left for a later pass");
    let second = sweep_metrics(&store).await;

    assert_eq!(
        (
            by_tenant(&first, reclaimed, None),
            by_tenant(&first, reclaimed, failed)
        ),
        (tenants(&[("a", 1)]), tenants(&[("a", 1)])),
        "the first sweep's one point is the failed delete"
    );
    assert_eq!(
        (
            by_tenant(&second, reclaimed, None),
            by_tenant(&second, reclaimed, failed)
        ),
        (tenants(&[("a", 1)]), tenants(&[])),
        "the second sweep deletes it, without error.type"
    );
    assert_eq!(tmp_files(&dir), 0);
}

/// A directory made read-only (listable, nothing created or deleted in
/// it) until dropped.
#[cfg(unix)]
struct ReadOnly {
    path: PathBuf,
    mode: std::fs::Permissions,
}

#[cfg(unix)]
impl ReadOnly {
    fn new(path: PathBuf) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("metadata").permissions();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o555)).expect("read-only");
        Self { path, mode }
    }
}

#[cfg(unix)]
impl Drop for ReadOnly {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.path, self.mode.clone());
    }
}
