//! The compactor suite, split alongside the module directory
//! (epic #745 wave 1); every original `super::X` path resolves through
//! the parent scope.

use std::path::Path;

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::data::{
    AggregatedMetrics, MetricData, ResourceMetrics, SumDataPoint,
};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
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
pub(super) const HOUR: u64 = 3_600_000_000_000;
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

/// A candidate whose manifest bootstrap loses to another compactor wrote
/// nothing and belongs to the winner, so the sweep stays a clean no-op. The
/// bootstrap is made to lose by occupying the manifest key with a
/// directory: it reads as absent, so the create-if-absent runs, and is
/// refused as already existing.
#[test]
fn sweep_leaves_a_candidate_whose_bootstrap_lost_as_a_no_op() {
    // Arrange
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_sealed_candidate(&store, "a");
    std::fs::create_dir(
        bucket
            .path()
            .join("data/tenant_id=a/year=2026/month=04/day=02/hour=10/manifest.json"),
    )
    .expect("occupy the manifest key");

    // Act
    let report = run_sweep(&store, NOW_SEALED, &CompactionPolicy::default()).expect("sweep");

    // Assert
    let found: Vec<_> = report
        .per_tenant
        .iter()
        .map(|t| t.candidates_found)
        .collect();
    assert_eq!(
        (report.partitions_compacted, found, report.errors.len()),
        (0, vec![1], 0),
        "{report:?}"
    );
}

fn outcome(files_before: usize, commit_lost: bool) -> CompactionOutcome {
    CompactionOutcome {
        files_before,
        rows: 0,
        rows_dropped: 0,
        committed: None,
        commit_lost,
        gc_failures: 0,
        bytes_read: 0,
        bytes_written: 0,
    }
}

/// #807 — a rewrite whose final manifest swap lost is a sweep error for a
/// consolidation and an erasure alike, so a store whose swaps always lose
/// never reads as an idle sweep.
#[test]
fn a_lost_final_swap_is_a_sweep_error() {
    let errors = [
        check_committed(outcome(2, true), false),
        check_committed(outcome(2, true), true),
    ];

    assert!(
        errors.iter().all(|e| e
            .as_ref()
            .is_err_and(|e| e.reason.contains("not committed"))),
        "{errors:?}"
    );
}

/// A lost bootstrap is benign for a consolidation but not for an erasure,
/// whose rows are still on disk; a partition with nothing to rewrite is a
/// no-op for both.
#[test]
fn a_lost_bootstrap_is_an_error_only_for_an_erasure() {
    let verdicts = [
        check_committed(outcome(2, false), false).is_err(),
        check_committed(outcome(2, false), true).is_err(),
        check_committed(outcome(0, false), false).is_err(),
        check_committed(outcome(0, false), true).is_err(),
    ];

    assert_eq!(verdicts, [false, true, false, false]);
}

/// A lost swap whose discarded rewrite could not be deleted keeps that
/// cleanup failure in the error, so the sweep still counts it.
#[test]
fn a_lost_swap_error_keeps_its_cleanup_failures() {
    let lost = CompactionOutcome {
        gc_failures: 1,
        ..outcome(2, true)
    };

    let error = check_committed(lost, true).expect_err("a lost swap is an error");

    assert_eq!(error.gc_failures, 1);
}

/// An erasure rewrite whose manifest swap loses leaves the marker in the
/// `rows` phase: advancing it would let the tuples be deleted while the
/// conversation's rows are still live. One file, so the consolidation pass
/// leaves the partition alone and only the erasure rewrite runs.
#[test]
fn erasure_keeps_the_rows_phase_when_its_manifest_swap_lost() {
    // Arrange
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_file(&store, "a", 1, TS0);
    std::fs::create_dir(
        bucket
            .path()
            .join("data/tenant_id=a/year=2026/month=04/day=02/hour=10/manifest.json"),
    )
    .expect("occupy the manifest key");
    request_erasure(&store, "a", "c-1").expect("request");
    let erase_all = |_: &MinedRecord, _: &str| true;
    let mut hooks = SweepHooks {
        observe: None,
        erasure_match: Some(&erase_all),
    };

    // Act
    let report = run_sweep_hooked(
        &store,
        NOW_SEALED,
        &CompactionPolicy::default(),
        &PromotedAttributes::default(),
        &mut hooks,
    )
    .expect("sweep");

    // Assert
    let outcomes: Vec<_> = report
        .erasures
        .iter()
        .map(|o| (o.phase, o.partitions_rewritten))
        .collect();
    assert_eq!(outcomes, vec![(ErasurePhase::Rows, 0)], "{report:?}");
    assert!(
        matches!(report.errors.as_slice(),
            [e] if e.contains("\"c-1\"") && e.contains("not committed")),
        "{:?}",
        report.errors
    );
    let pending: Vec<_> = pending_erasures(&store)
        .expect("pending")
        .iter()
        .map(|r| r.phase)
        .collect();
    assert_eq!(pending, vec![ErasurePhase::Rows]);
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
    let compactor = Compactor::new(store, CompactionPolicy::default(), Duration::from_millis(5));
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
        let first = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(5)))
            .await
            .expect("join")
            .expect("a sweep ran within 5s");
        handle.abort();
        first
    });

    // Assert — the loop ran a sweep that compacted the candidate.
    assert_eq!(compacted.expect("sweep ok"), 1);
}

/// `count` sealed candidates for tenant `a`, one per consecutive hour from
/// `TS0`'s, two files each.
fn write_sealed_hours(store: &Store, count: u64) {
    for hour in 0..count {
        write_file(store, "a", 1, TS0 + hour * HOUR);
        write_file(store, "a", 2, TS0 + hour * HOUR + 1_000_000);
    }
}

/// The partition a compaction audit event names.
fn event_partition(event: &AuditEvent) -> String {
    match &event.payload {
        AuditPayload::Compaction { partition, .. } => partition.clone(),
        other => panic!("not a compaction event: {other:?}"),
    }
}

/// An audit sink that hands every event on, then stands in for the process
/// dying once it has emitted `crash_after` of them.
pub(super) struct CrashingSink {
    inner: ourios_core::audit::SharedAuditSink,
    crash_after: usize,
    emitted: usize,
}

impl CrashingSink {
    pub(super) fn new(inner: &ourios_core::audit::SharedAuditSink, crash_after: usize) -> Self {
        Self {
            inner: inner.clone(),
            crash_after,
            emitted: 0,
        }
    }
}

impl AuditSink for CrashingSink {
    fn emit(&mut self, event: AuditEvent) {
        self.inner.emit(event);
        self.emitted += 1;
        assert!(self.emitted < self.crash_after, "the process dies here");
    }
}

/// A sweep that dies after its `k`th committed partition has already
/// emitted the audit events of partitions 1..=k — each right after its
/// manifest commit, not at sweep end — and left the rest uncompacted for
/// the next sweep (RFC 0009 §3.6).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sweep_dying_mid_backlog_has_emitted_every_committed_partitions_event() {
    // Arrange
    const PARTITIONS: u64 = 5;
    const K: usize = 2;
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_sealed_hours(&store, PARTITIONS);
    let audit = ourios_core::audit::SharedAuditSink::new();
    let sink = CrashingSink::new(&audit, K);

    // Act
    let sweep = tokio::spawn(sweep_once(
        store.clone(),
        CompactionPolicy::default(),
        PromotedAttributes::default(),
        Box::new(sink),
        #[cfg(feature = "openfga")]
        None,
    ))
    .await;

    // Assert
    assert!(matches!(&sweep, Err(e) if e.is_panic()), "the sweep died");
    let emitted: Vec<String> = audit.drain().iter().map(event_partition).collect();
    assert_eq!(emitted.len(), K, "{emitted:?}");
    let left =
        plan_candidates(&store, "a", now_unix_nanos(), &CompactionPolicy::default()).expect("plan");
    assert_eq!(
        left.len(),
        usize::try_from(PARTITIONS).expect("small") - K,
        "the sweep stopped at the crash: {left:?}"
    );
    for partition in &left {
        let key = format!(
            "year={:04}/month={:02}/day={:02}/hour={:02}",
            partition.year, partition.month, partition.day, partition.hour
        );
        assert!(!emitted.contains(&key), "{key} uncompacted yet audited");
    }
}

/// Reads `partitions` and `files` through a fresh collection at every
/// event the sweep emits.
struct ReadingSink {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
    readings: Arc<std::sync::Mutex<Vec<(u64, u64)>>>,
}

fn u64_sum(rms: &[ResourceMetrics], name: &str) -> u64 {
    rms.iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .filter(|m| m.name() == name)
        .map(|m| match m.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                sum.data_points().map(SumDataPoint::value).sum()
            }
            other => panic!("{name} is not a u64 sum: {other:?}"),
        })
        .last()
        .unwrap_or(0)
}

impl AuditSink for ReadingSink {
    fn emit(&mut self, _: AuditEvent) {
        self.exporter.reset();
        self.provider.force_flush().expect("flush");
        let rms = self.exporter.get_finished_metrics().expect("collect");
        self.readings.lock().expect("readings").push((
            u64_sum(&rms, ourios_semconv::OURIOS_COMPACTION_PARTITIONS),
            u64_sum(&rms, ourios_semconv::OURIOS_COMPACTION_FILES),
        ));
    }
}

/// The partition counters rise as each partition commits, not once the
/// sweep ends: a sweep over a backlog of hourly partitions runs for hours,
/// and `ourios.compaction.partitions` must show its progress meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partition_counters_rise_with_each_commit_before_the_sweep_ends() {
    // Arrange
    const PARTITIONS: u64 = 4;
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_sealed_hours(&store, PARTITIONS);
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .build();
    let metrics = Arc::new(CompactionMetrics::from_meter(
        &provider.meter("ourios.compaction"),
    ));
    let readings = Arc::default();
    let sink = ReadingSink {
        provider: provider.clone(),
        exporter,
        readings: Arc::clone(&readings),
    };

    // Act
    let (result, _, _) = sweep_recorded(
        store,
        CompactionPolicy::default(),
        PromotedAttributes::default(),
        Box::new(sink),
        #[cfg(feature = "openfga")]
        None,
        Some(metrics),
    )
    .await;

    // Assert
    let report = result.expect("sweep");
    assert_eq!(report.partitions_compacted, 4, "{report:?}");
    let readings = readings.lock().expect("readings").clone();
    assert_eq!(
        readings,
        vec![(1, 2), (2, 4), (3, 6), (4, 8)],
        "one partition and its two files counted at each commit"
    );
}
