//! The compactor suite, split alongside the module directory
//! (epic #745 wave 1); every original `super::X` path resolves through
//! the parent scope.

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
