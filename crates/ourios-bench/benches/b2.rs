//! B2 — template-exact query latency vs corpus size (RFC 0007 §6).
//!
//! Supportive, **non-gating** wall-clock evidence for the RFC0007.2
//! thesis gate that `ourios-querier` already proves *structurally*
//! (deterministically, via `QueryStats`) in its test suite. Here we
//! measure the thing the structural test can't: actual latency.
//!
//! Two groups:
//!
//! - `b2/synthetic` (always runs) — the controlled instrument. The
//!   *result* size is held constant (`TARGET_ROWS` of one template)
//!   while the *corpus* grows 1×/10×/50× with filler under other
//!   templates in other hours (own files ⇒ own row groups, pruned
//!   by `template_id` statistics). If the inverted-index-collapse
//!   thesis holds, latency stays ~flat as the corpus grows.
//! - `b2/otel-demo` (only when `OURIOS_B2_CORPUS_DIRS` is set) — the
//!   real corpora (`corpus/otel-demo-v*`), one bench id per dir. See
//!   the env var's use below.

use std::hint::black_box;
use std::path::Path;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use ourios_bench::build_query_store;
use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, Writer};
use ourios_querier::{Querier, QueryRequest};

/// Anchored in 2026 so the derived partition path is stable.
const TS0: u64 = 1_775_127_480_000_000_000;
const HOUR_NS: u64 = 3_600_000_000_000;

/// Rows under the queried template — the result size, held constant
/// across corpus sizes so latency reflects result, not corpus.
const TARGET_ROWS: u64 = 2_000;
/// Filler rows per hour-file (each a distinct non-target template).
const FILLER_PER_FILE: u64 = 4_000;

fn rec(template_id: u64, ts_ns: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("a"),
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

/// Write `records` that all share one partition into a single file.
fn write_one_file(bucket: &Path, records: &[MinedRecord]) {
    let part = PartitionKey::derive(&records[0]).expect("derive partition");
    let mut w = Writer::open(bucket, part).expect("open writer");
    w.append_records(records).expect("append");
    w.close().expect("close");
}

/// Build a store with `TARGET_ROWS` rows of template 1 (hour 0) plus
/// `filler_rows` rows of distinct non-target templates spread across
/// later hours, so a `template_id = 1` query returns a fixed result
/// while total corpus size is `TARGET_ROWS + filler_rows`.
fn build_synthetic(bucket: &Path, filler_rows: u64) {
    let target: Vec<MinedRecord> = (0..TARGET_ROWS).map(|i| rec(1, TS0 + i * 1_000)).collect();
    write_one_file(bucket, &target);

    let mut written = 0u64;
    let mut hour = 1u64;
    while written < filler_rows {
        let n = (filler_rows - written).min(FILLER_PER_FILE);
        let base = TS0 + hour * HOUR_NS;
        // A distinct template per file ⇒ its row group is pruned by
        // a `template_id = 1` query.
        let template = 1_000 + hour;
        let file: Vec<MinedRecord> = (0..n).map(|i| rec(template, base + i * 1_000)).collect();
        write_one_file(bucket, &file);
        written += n;
        hour += 1;
    }
}

fn template_exact(template_id: u64) -> QueryRequest {
    QueryRequest {
        tenant: TenantId::new("a"),
        time_range: None,
        template_id: Some(template_id),
    }
}

fn synthetic(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("tokio runtime");

    let mut group = c.benchmark_group("b2/synthetic");
    for mult in [1u64, 10, 50] {
        let filler = TARGET_ROWS * (mult - 1);
        let total = TARGET_ROWS + filler;

        let bucket = tempfile::TempDir::new().expect("temp bucket");
        build_synthetic(bucket.path(), filler);
        let querier = Querier::new(bucket.path());

        // Sanity: the result is the fixed TARGET_ROWS regardless of
        // corpus — otherwise the latency comparison is meaningless.
        let probe = rt
            .block_on(querier.run(template_exact(1)))
            .expect("probe query");
        assert_eq!(
            probe.rows, TARGET_ROWS,
            "result size must be constant across corpus sizes",
        );

        group.bench_with_input(BenchmarkId::from_parameter(total), &total, |b, _| {
            b.iter(|| {
                let r = rt.block_on(querier.run(template_exact(1))).expect("query");
                black_box(r.rows);
            });
        });
    }
    group.finish();
}

/// Real corpora, one bench id per directory in `OURIOS_B2_CORPUS_DIRS`
/// (comma-separated). Each dir is loaded → mined → written to a temp
/// Parquet store, then the busiest template is queried. Skipped
/// entirely when the env var is unset (the corpora aren't committed;
/// CI / a local operator stages them and points this at them, e.g.
/// `OURIOS_B2_CORPUS_DIRS=corpus/otel-demo-v1,corpus/otel-demo-v4`).
fn otel_demo(c: &mut Criterion) {
    let Ok(raw) = std::env::var("OURIOS_B2_CORPUS_DIRS") else {
        eprintln!(
            "b2/otel-demo: OURIOS_B2_CORPUS_DIRS unset — skipping (synthetic group still runs)"
        );
        return;
    };
    let dirs: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if dirs.is_empty() {
        return;
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("tokio runtime");

    let mut group = c.benchmark_group("b2/otel-demo");
    for dir in dirs {
        let path = Path::new(dir);
        if !path.is_dir() {
            eprintln!("b2/otel-demo: {dir} is not a directory — skipping");
            continue;
        }
        let bucket = tempfile::TempDir::new().expect("temp bucket");
        let built = build_query_store(path, bucket.path()).expect("build query store");
        if built.busiest_template_rows == 0 {
            eprintln!("b2/otel-demo: {dir} produced no rows — skipping");
            continue;
        }
        let querier = Querier::new(bucket.path());
        eprintln!(
            "b2/otel-demo: {dir} — {} rows, {} files; querying template {} ({} rows)",
            built.rows, built.files, built.busiest_template_id, built.busiest_template_rows,
        );

        let id = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(dir)
            .to_string();
        group.bench_function(BenchmarkId::new("corpus", id), |b| {
            b.iter(|| {
                let r = rt
                    .block_on(querier.run(template_exact(built.busiest_template_id)))
                    .expect("query");
                black_box(r.rows);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, synthetic, otel_demo);
criterion_main!(benches);
