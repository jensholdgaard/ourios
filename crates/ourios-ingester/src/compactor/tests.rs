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
use ourios_parquet::{PartitionKey, Store, Writer, plan_candidates};

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
pub(super) fn write_file(store: &Store, tenant: &str, template_id: u64, ts_ns: u64) {
    let record = rec(tenant, template_id, ts_ns);
    let mut w = Writer::open_in(store, PartitionKey::derive(&record).expect("derive"))
        .expect("open writer");
    w.append_records(&[record]).expect("append");
    w.close().expect("close");
}

/// Two committed files in one sealed partition = a candidate.
pub(super) fn write_sealed_candidate(store: &Store, tenant: &str) {
    write_file(store, tenant, 1, TS0);
    write_file(store, tenant, 2, TS0 + 1_000_000);
}

/// The spans `run` emits under a scoped subscriber over an in-memory
/// exporter, with what `run` returned.
fn traced<T>(run: impl FnOnce() -> T) -> (T, Vec<opentelemetry_sdk::trace::SpanData>) {
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use tracing_subscriber::prelude::*;

    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(ourios_telemetry::test_tracer(&provider)));
    let out = tracing::subscriber::with_default(subscriber, run);
    provider.force_flush().expect("spans flush");
    (out, exporter.get_finished_spans().expect("spans exported"))
}

/// The one span named `name`.
fn only_span<'s>(
    spans: &'s [opentelemetry_sdk::trace::SpanData],
    name: &str,
) -> &'s opentelemetry_sdk::trace::SpanData {
    let [span] = spans
        .iter()
        .filter(|s| s.name.as_ref() == name)
        .collect::<Vec<_>>()[..]
    else {
        panic!("one `{name}` span, got {spans:?}");
    };
    span
}

fn attr(span: &opentelemetry_sdk::trace::SpanData, key: &str) -> Option<opentelemetry::Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.clone())
}

/// The `ourios.file_set.*` counts `plan partitions` carries, in order:
/// list requests, listed objects, manifest reads.
fn plan_counts(span: &opentelemetry_sdk::trace::SpanData) -> [Option<opentelemetry::Value>; 3] {
    [
        ourios_semconv::OURIOS_FILE_SET_LIST_REQUEST_COUNT,
        ourios_semconv::OURIOS_FILE_SET_LISTED_OBJECTS,
        ourios_semconv::OURIOS_FILE_SET_MANIFEST_READ_COUNT,
    ]
    .map(|key| attr(span, key))
}

fn counts(
    list_requests: i64,
    listed_objects: i64,
    manifest_reads: i64,
) -> [Option<opentelemetry::Value>; 3] {
    [list_requests, listed_objects, manifest_reads].map(|n| Some(opentelemetry::Value::I64(n)))
}

/// RFC0038.1 (sweep arm, as amended for RFC 0038 §3.8) — one `sweep
/// partitions` INTERNAL root per sweep, with exactly one `plan partitions`
/// INTERNAL child. `run_sweep` is the sync body `spawn_blocking`ed in
/// production; a scoped `with_default` subscriber captures the spans it
/// opens (the per-tenant / per-file loops below them stay span-free —
/// RFC0038.2). Replaces the one-span test the amendment supersedes.
#[test]
fn rfc0038_1_sweep_emits_its_span_and_one_plan_phase() {
    use opentelemetry::trace::{SpanId, SpanKind};

    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_sealed_candidate(&store, "a");

    let (result, spans) = traced(|| run_sweep(&store, NOW_SEALED, &CompactionPolicy::default()));
    result.expect("sweep");

    // The sweep path is our code only (filesystem + Parquet, no async
    // runtime / DataFusion), so asserting the total count catches any
    // accidental extra instrumentation.
    assert_eq!(spans.len(), 2, "exactly two spans total, got {spans:?}");
    let sweep = only_span(&spans, "sweep partitions");
    let plan = only_span(&spans, "plan partitions");
    assert_eq!(
        (&sweep.span_kind, &plan.span_kind),
        (&SpanKind::Internal, &SpanKind::Internal),
        "both spans are INTERNAL",
    );
    assert_eq!(
        (
            sweep.parent_span_id,
            plan.parent_span_id,
            plan.span_context.trace_id(),
        ),
        (
            SpanId::INVALID,
            sweep.span_context.span_id(),
            sweep.span_context.trace_id(),
        ),
        "the sweep is a root, and the plan phase its child in the same trace",
    );
}

/// RFC0038.17 — `plan partitions` counts the planning's listings and
/// manifest reads over every tenant, lies within the sweep, and ends
/// before the first partition commits.
#[test]
fn the_plan_phase_counts_its_listings_and_manifest_reads() {
    use opentelemetry::trace::Status;

    // Arrange — `a`: a sealed hour-10 partition with two files and no
    // manifest. `b`: one file in hour 12, still open at `NOW_SEALED`.
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_sealed_candidate(&store, "a");
    write_file(&store, "b", 1, NOW_SEALED);
    let mut first_commit = None;

    // Act
    let (result, spans) = traced(|| {
        run_sweep_committing(
            &store,
            SweepClock::sealed_at(NOW_SEALED),
            &CompactionPolicy::default(),
            &PromotedAttributes::default(),
            &mut SweepHooks::default(),
            &mut |_| {
                first_commit.get_or_insert_with(SystemTime::now);
            },
        )
    });

    // Assert — one recursive listing per tenant, returning `a`'s two files
    // and `b`'s one; `a`'s partition lists no manifest, so none is read
    // (RFC 0009 §3.3, 2026-10-16 amendment).
    assert_eq!(result.expect("sweep").partitions_compacted, 1);
    let sweep = only_span(&spans, "sweep partitions");
    let plan = only_span(&spans, "plan partitions");
    assert_eq!(
        (plan_counts(plan), &plan.status),
        (counts(2, 3, 0), &Status::Unset)
    );
    assert!(sweep.start_time <= plan.start_time && plan.end_time <= sweep.end_time);
    let first_commit = first_commit.expect("a partition committed");
    assert!(
        plan.end_time <= first_commit,
        "planning ends before the first commit"
    );
}

/// RFC0038.17 — a tenant whose planning fails marks `plan partitions`
/// `Error`, keeps the counts up to the failure, and the sweep goes on.
#[cfg(unix)]
#[test]
fn a_failed_tenant_plan_marks_the_plan_phase_error() {
    use opentelemetry::trace::Status;

    // Arrange — `a` plans and compacts; `b`'s directory can't be listed.
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_sealed_candidate(&store, "a");
    write_file(&store, "b", 1, TS0);
    let unlistable = Unreadable::new(bucket.path().join("data/tenant_id=b"));

    // Act
    let (result, spans) = traced(|| run_sweep(&store, NOW_SEALED, &CompactionPolicy::default()));
    drop(unlistable);

    // Assert — `a`'s one listing of its two objects, no manifest read
    // (none is listed), plus `b`'s one failed listing.
    let report = result.expect("a failed tenant does not fail the sweep");
    assert_eq!(report.partitions_compacted, 1, "{report:?}");
    let plan = only_span(&spans, "plan partitions");
    assert!(
        matches!(plan.status, Status::Error { .. }),
        "{:?}",
        plan.status
    );
    assert_eq!(
        (
            attr(plan, "error.type"),
            plan_counts(plan),
            &only_span(&spans, "sweep partitions").status,
        ),
        (Some("io".into()), counts(2, 2, 0), &Status::Unset),
    );
}

/// RFC0038.17 — a store with no tenant still has its (empty) planning
/// phase.
#[test]
fn an_empty_store_still_spans_its_planning() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());

    let (result, spans) = traced(|| run_sweep(&store, NOW_SEALED, &CompactionPolicy::default()));

    result.expect("sweep");
    assert_eq!(
        plan_counts(only_span(&spans, "plan partitions")),
        counts(0, 0, 0)
    );
}

/// A sweep that fails marks its span `Error`, with the failure's class as
/// `error.type`: here the tenant listing under an unreadable `data/`.
#[cfg(unix)]
#[test]
fn a_failed_sweep_marks_its_span_error() {
    use opentelemetry::trace::Status;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use tracing_subscriber::prelude::*;

    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_sealed_candidate(&store, "a");
    let unlistable = Unreadable::new(bucket.path().join("data"));

    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(ourios_telemetry::test_tracer(&provider)));
    let result = tracing::subscriber::with_default(subscriber, || {
        run_sweep(&store, NOW_SEALED, &CompactionPolicy::default())
    });
    drop(unlistable);
    assert!(result.is_err(), "an unlistable data/ fails the sweep");

    let spans = exporter.get_finished_spans().expect("spans exported");
    let [span] = spans.as_slice() else {
        panic!("one sweep span, got {spans:?}");
    };
    assert!(
        matches!(span.status, Status::Error { .. }),
        "{:?}",
        span.status
    );
    let error_type = span
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == "error.type")
        .map(|kv| kv.value.as_str().into_owned());
    assert_eq!(error_type.as_deref(), Some("io"));
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
        decoded_bytes_estimate: 0,
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
        gauges: None,
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

/// RFC0009.12 — an erasure still rewrites a settled partition: one live
/// file at or above the small-file size, which candidate planning skips
/// without reading it. Erasure walks every hour partition itself.
#[test]
fn rfc0009_12_erasure_rewrites_a_settled_partition_the_planner_skips() {
    // Arrange — one file holding a row to erase (template 1) and one to keep.
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let records = [rec("a", 1, TS0), rec("a", 2, TS0 + 1_000_000)];
    let partition = PartitionKey::derive(&records[0]).expect("derive");
    let mut w = Writer::open_in(&store, partition.clone()).expect("open writer");
    w.append_records(&records).expect("append");
    w.close().expect("close");
    let policy = CompactionPolicy {
        small_file_bytes: 1,
        ..CompactionPolicy::default()
    };
    assert!(
        plan_candidates(&store, "a", NOW_SEALED, &policy)
            .expect("plan")
            .is_empty(),
        "the partition is settled: the planner skips it",
    );
    request_erasure(&store, "a", "c-1").expect("request");
    let erase_template_1 = |record: &MinedRecord, _: &str| record.template_id == 1;
    let mut hooks = SweepHooks {
        observe: None,
        erasure_match: Some(&erase_template_1),
        gauges: None,
    };

    // Act
    let report = run_sweep_hooked(
        &store,
        NOW_SEALED,
        &policy,
        &PromotedAttributes::default(),
        &mut hooks,
    )
    .expect("sweep");

    // Assert
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let candidates: Vec<_> = report
        .per_tenant
        .iter()
        .map(|t| t.candidates_found)
        .collect();
    assert_eq!(candidates, vec![0], "no consolidation candidate");
    let outcomes: Vec<_> = report
        .erasures
        .iter()
        .map(|o| (o.partitions_rewritten, o.rows_dropped))
        .collect();
    assert_eq!(outcomes, vec![(1, 1)], "{report:?}");
    let mut kept = Vec::new();
    ourios_parquet::compaction::visit_partition_rows(&store, &partition, |batch| {
        kept.extend(batch.iter().map(|r| r.template_id));
    })
    .expect("read");
    assert_eq!(kept, vec![2], "the erased row is gone, the other kept");
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
    // "b" has a malformed manifest.json, so planning it errors. b holds two
    // files: planning reads a manifest only where two or more are listed
    // (RFC 0009 §3.3, 2026-10-16 amendment).
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_sealed_candidate(&store, "a");
    write_sealed_candidate(&store, "b");
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

/// A graph-less sweep of `store` under the default policy and promoted set.
pub(super) fn target(store: &Store) -> SweepTarget {
    SweepTarget::new(
        store.clone(),
        CompactionPolicy::default(),
        PromotedAttributes::default(),
    )
}

/// `count` sealed candidates for tenant `a`, one per consecutive hour from
/// `TS0`'s, two files each.
pub(super) fn write_sealed_hours(store: &Store, count: u64) {
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
async fn a_sweep_dying_mid_backlog_has_emitted_the_events_committed_before_it() {
    // Arrange
    const PARTITIONS: u64 = 5;
    const K: usize = 2;
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_sealed_hours(&store, PARTITIONS);
    let audit = ourios_core::audit::SharedAuditSink::new();
    let sink = CrashingSink::new(&audit, K);

    // Act
    let sweep = tokio::spawn(sweep_once(target(&store), Box::new(sink))).await;

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

pub(super) fn u64_sum(rms: &[ResourceMetrics], name: &str) -> u64 {
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
        &provider.meter_with_scope(ourios_core::telemetry::scope("ourios.compaction")),
    ));
    let readings = Arc::default();
    let sink = ReadingSink {
        provider: provider.clone(),
        exporter,
        readings: Arc::clone(&readings),
    };

    // Act
    let (result, _, _) = sweep_recorded(target(&store), Box::new(sink), Some(metrics)).await;

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

/// A store backend whose deletes of data files — the cleanup of a
/// compaction's superseded inputs, after its commit — fail: with `dies`,
/// the process dies (panics) on the first; otherwise each delete errors.
#[derive(Debug)]
struct BrokenCleanup {
    inner: Arc<dyn object_store::ObjectStore>,
    dies: bool,
}

impl std::fmt::Display for BrokenCleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BrokenCleanup({})", self.inner)
    }
}

#[async_trait::async_trait]
impl object_store::ObjectStore for BrokenCleanup {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
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

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<
            'static,
            object_store::Result<object_store::path::Path>,
        >,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>> {
        use futures::StreamExt as _;
        let dies = self.dies;
        let broken = locations.map(move |location| {
            let path = location?;
            let data = path.as_ref().starts_with("data/") && path.as_ref().ends_with(".parquet");
            assert!(!(dies && data), "the process dies cleaning up {path}");
            if data {
                return Err(object_store::Error::Generic {
                    store: "broken-cleanup",
                    source: format!("cannot delete {path}").into(),
                });
            }
            Ok(path)
        });
        self.inner.delete_stream(broken.boxed())
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

/// A sweep dying in the cleanup after a partition's commit — the
/// superseded inputs' deletes — has already recorded that partition: its
/// counters and its audit event come from the commit itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sweep_dying_in_post_commit_cleanup_has_recorded_the_partition() {
    // Arrange
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_sealed_hours(&store, 2);
    let store = store.wrap_backend(|inner| Arc::new(BrokenCleanup { inner, dies: true }));
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .build();
    let metrics = Arc::new(CompactionMetrics::from_meter(
        &provider.meter_with_scope(ourios_core::telemetry::scope("ourios.compaction")),
    ));
    let audit = ourios_core::audit::SharedAuditSink::new();

    // Act
    let sweep = tokio::spawn(sweep_recorded(
        target(&store),
        Box::new(audit.clone()),
        Some(metrics),
    ))
    .await;

    // Assert
    assert!(matches!(&sweep, Err(e) if e.is_panic()), "the sweep died");
    provider.force_flush().expect("flush");
    let rms = exporter.get_finished_metrics().expect("collect");
    let events: Vec<String> = audit.drain().iter().map(event_partition).collect();
    assert_eq!(
        (
            events.len(),
            u64_sum(&rms, ourios_semconv::OURIOS_COMPACTION_PARTITIONS),
            u64_sum(&rms, ourios_semconv::OURIOS_COMPACTION_FILES),
        ),
        (1, 1, 2),
        "the committed partition's event and counters: {events:?}"
    );
}

/// A clock that advances by an hour on every read, starting a day after
/// `NOW_SEALED`.
fn advancing_clock() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(NOW_SEALED + 24 * HOUR);
    NEXT.fetch_add(HOUR, std::sync::atomic::Ordering::SeqCst)
}

/// Each commit's audit event is stamped from the clock as it is emitted
/// (RFC 0005 §3.7), not with the sweep's start: two commits read an
/// advancing clock and carry distinct, increasing timestamps after the
/// sweep's start. Candidates are still sealed against the start: hour 12,
/// open at `NOW_SEALED` but long closed by the advanced clock, is left
/// alone.
#[test]
fn audit_events_carry_the_clock_at_emit_while_sealing_uses_the_sweep_start() {
    // Arrange
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_sealed_candidate(&store, "a");
    write_sealed_candidate(&store, "b");
    write_file(&store, "a", 3, NOW_SEALED);
    write_file(&store, "a", 4, NOW_SEALED + 1_000_000);
    let clock = SweepClock {
        sealed_at: NOW_SEALED,
        now: advancing_clock,
    };
    let mut stamps = Vec::new();

    // Act
    let report = run_sweep_committing(
        &store,
        clock,
        &CompactionPolicy::default(),
        &PromotedAttributes::default(),
        &mut SweepHooks::default(),
        &mut |commit| stamps.push(commit.event.timestamp),
    )
    .expect("sweep");

    // Assert
    let start = SystemTime::UNIX_EPOCH + Duration::from_nanos(NOW_SEALED);
    assert_eq!(
        report.partitions_compacted, 2,
        "hour 12 is not sealed: {report:?}"
    );
    assert!(
        matches!(stamps.as_slice(), [first, second] if start < *first && first < second),
        "{stamps:?}"
    );
}

/// The backlog the in-memory exporter last collected, per tenant.
fn backlog(rms: &[ResourceMetrics]) -> Vec<(String, i64)> {
    rms.iter()
        .flat_map(ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .filter(|m| m.name() == ourios_semconv::OURIOS_COMPACTION_BACKLOG)
        .flat_map(|m| match m.data() {
            AggregatedMetrics::I64(MetricData::Sum(sum)) => sum
                .data_points()
                .map(|dp| {
                    let tenant = dp
                        .attributes()
                        .find(|kv| kv.key.as_str() == ourios_semconv::OURIOS_TENANT)
                        .map(|kv| kv.value.as_str().to_string())
                        .unwrap_or_default();
                    (tenant, dp.value())
                })
                .collect::<Vec<_>>(),
            other => panic!("backlog is not an i64 sum: {other:?}"),
        })
        .collect()
}

/// Takes every permission off a path, and restores its mode when dropped,
/// so a failing assertion or panic still leaves the temp dir removable.
#[cfg(unix)]
pub(super) struct Unreadable {
    path: std::path::PathBuf,
    mode: std::fs::Permissions,
}

#[cfg(unix)]
impl Unreadable {
    pub(super) fn new(path: std::path::PathBuf) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("metadata").permissions();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("unreadable");
        Self { path, mode }
    }
}

#[cfg(unix)]
impl Drop for Unreadable {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.path, self.mode.clone());
    }
}

/// A sweep that commits and then fails fatally still records what it
/// did: the backlog gauge is rebuilt from its partial report (tenant `a`'s
/// candidate was compacted, so its lag is 0) and the inputs its cleanup
/// could not delete count as orphan files.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fatally_failed_sweep_records_its_partial_backlog_and_gc_failures() {
    // Arrange
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_sealed_hours(&store, 1);
    let store = store.wrap_backend(|inner| Arc::new(BrokenCleanup { inner, dies: false }));
    let markers = bucket.path().join("erasure");
    std::fs::create_dir(&markers).expect("erasure dir");
    let unlistable = Unreadable::new(markers);
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .build();
    let metrics = Arc::new(CompactionMetrics::from_meter(
        &provider.meter_with_scope(ourios_core::telemetry::scope("ourios.compaction")),
    ));

    // Act
    let (result, elapsed, _) = sweep_recorded(
        target(&store),
        Box::new(ourios_core::audit::SharedAuditSink::new()),
        Some(Arc::clone(&metrics)),
    )
    .await;
    metrics.record_sweep_outcome(&result, elapsed);
    drop(unlistable);

    // Assert
    let failed = result.expect_err("the erasure listing fails the sweep");
    let partial = failed.partial.as_deref().expect("a partial report");
    assert_eq!(
        (partial.partitions_compacted, partial.gc_failures),
        (1, 2),
        "{partial:?}"
    );
    provider.force_flush().expect("flush");
    let rms = exporter.get_finished_metrics().expect("collect");
    assert_eq!(
        (
            backlog(&rms),
            u64_sum(&rms, ourios_semconv::OURIOS_COMPACTION_ORPHAN_FILES),
        ),
        (vec![("a".to_string(), 0)], 2),
        "the partial report's backlog and gc failures"
    );
}
