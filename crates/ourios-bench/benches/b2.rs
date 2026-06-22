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
//! - `b2/real-corpus` (only when `OURIOS_B2_CORPUS_DIRS` is set) — the
//!   real corpora, one bench id per dir. Both loader formats work
//!   here: OTLP/JSON (`corpus/otel-demo-v*`) and plain text (e.g.
//!   the bench-time-fetched `LogHub` `HDFS_v1` — see
//!   `docs/benchmarks.md` §1), since the template-exact query doesn't
//!   depend on the §3.3 envelope defaults. See the env var's use
//!   below.

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

fn template_exact(tenant: &str, template_id: u64) -> QueryRequest {
    QueryRequest {
        tenant: TenantId::new(tenant),
        time_range: None,
        template_id: Some(template_id),
        severity_text: None,
        limit: None,
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
            .block_on(querier.run(template_exact("a", 1)))
            .expect("probe query");
        assert_eq!(
            probe.rows, TARGET_ROWS,
            "result size must be constant across corpus sizes",
        );

        group.bench_with_input(BenchmarkId::from_parameter(total), &total, |b, _| {
            b.iter(|| {
                let r = rt
                    .block_on(querier.run(template_exact("a", 1)))
                    .expect("query");
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
fn real_corpus(c: &mut Criterion) {
    let Ok(raw) = std::env::var("OURIOS_B2_CORPUS_DIRS") else {
        eprintln!(
            "b2/real-corpus: OURIOS_B2_CORPUS_DIRS unset — skipping (synthetic group still runs)"
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

    let mut group = c.benchmark_group("b2/real-corpus");
    // GiB-class corpora (HDFS_v1 is ~1.5 GiB) make one probe + query
    // iteration expensive; 10 samples (criterion's minimum) keeps the
    // indicative timing affordable. The synthetic group keeps the
    // default sampling.
    group.sample_size(10);
    for dir in dirs {
        let path = Path::new(dir);
        if !path.is_dir() {
            eprintln!("b2/real-corpus: {dir} is not a directory — skipping");
            continue;
        }
        let bucket = tempfile::TempDir::new().expect("temp bucket");
        let built = build_query_store(path, bucket.path()).expect("build query store");
        if built.busiest_template_rows == 0 {
            eprintln!("b2/real-corpus: {dir} produced no rows — skipping");
            continue;
        }
        let querier = Querier::new(bucket.path());
        // Query the corpus's own tenant (the loader is single-tenant);
        // querying any other tenant would hit the empty-result early
        // return and time nothing (RFC0007.5 isolation). Probe first so
        // a result/tenant mismatch fails loudly instead of silently
        // benchmarking a 0-row query.
        let query = template_exact(built.tenant, built.busiest_template_id);
        let probe = rt
            .block_on(querier.run(query.clone()))
            .expect("probe query");
        assert_eq!(
            probe.rows, built.busiest_template_rows,
            "the busiest-template query must return its rows (tenant/template wired correctly)",
        );
        eprintln!(
            "b2/real-corpus: {dir} — {} rows, {} files; tenant {:?}; querying template {} \
             → {} rows; scanned {}/{} row groups, {} B",
            built.rows,
            built.files,
            built.tenant,
            built.busiest_template_id,
            probe.rows,
            probe.stats.row_groups_scanned,
            probe.stats.row_groups_scanned + probe.stats.row_groups_pruned,
            probe.stats.bytes_read,
        );

        let id = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(dir)
            .to_string();
        group.bench_function(BenchmarkId::new("corpus", &id), |b| {
            b.iter(|| {
                let r = rt.block_on(querier.run(query.clone())).expect("query");
                black_box(r.rows);
            });
        });

        // Partition-pruning measurement (RFC 0007): a query bounded to
        // the corpus's first hour reaches DataFusion with only that
        // hour's partition(s) — the rest pruned at the directory level.
        // The unwindowed probe above touched every partition, so its
        // row-group total is the no-window baseline to prune against.
        let baseline_row_groups = probe.stats.row_groups_scanned + probe.stats.row_groups_pruned;
        if let Some(windowed) = first_hour_window(&rt, &querier, &built, dir, baseline_row_groups) {
            group.bench_function(BenchmarkId::new("corpus-window-1h", &id), |b| {
                b.iter(|| {
                    let r = rt
                        .block_on(querier.run(windowed.clone()))
                        .expect("windowed query");
                    black_box(r.rows);
                });
            });
        }
    }
    group.finish();
}

/// Probe the **same busiest-template query** as the unwindowed arm but
/// bounded to the corpus's first hour, assert it prunes partitions
/// before `DataFusion`, log the pruning, and return the query to bench.
/// Adding only the time bound (template held fixed) isolates the
/// directory-level time-pruning effect: the unwindowed template query
/// scans every partition (a template recurs across all hours on real
/// logs), so the drop in scanned row groups is purely the window.
/// `None` (skip) when the corpus is single-partition or has no timestamp
/// span — nothing to prune. `baseline_row_groups` is the row-group total
/// the unwindowed query touched — the no-window denominator the windowed
/// query must come in strictly under (row-groups vs row-groups, not vs
/// the partition-file count, which can differ when a file holds several
/// row groups).
fn first_hour_window(
    rt: &tokio::runtime::Runtime,
    querier: &Querier,
    built: &ourios_bench::BuiltStore,
    dir: &str,
    baseline_row_groups: u64,
) -> Option<QueryRequest> {
    if built.min_effective_time_unix_nano == 0 || built.files < 2 {
        eprintln!(
            "b2/real-corpus: {dir} — single-partition or no timestamp span; skipping windowed arm"
        );
        return None;
    }
    let hour_start =
        built.min_effective_time_unix_nano - (built.min_effective_time_unix_nano % HOUR_NS);
    // Clamp the window end into the querier's i64-nanosecond range, so a
    // corpus near the year-2262 boundary can't push the bound past
    // i64::MAX (which the querier rejects as InvalidQuery, panicking the
    // probe below). Unreachable for real 2026-era corpora; cheap insurance.
    let hour_end = hour_start.saturating_add(HOUR_NS).min(i64::MAX as u64);
    // Same query as the unwindowed arm (busiest template) plus the 1h
    // bound, so only the window varies and the scanned-row-group drop is
    // purely the directory-level time pruning.
    let windowed = QueryRequest {
        tenant: TenantId::new(built.tenant),
        time_range: Some((hour_start, hour_end)),
        template_id: Some(built.busiest_template_id),
        severity_text: None,
        limit: None,
    };
    let probe = rt
        .block_on(querier.run(windowed.clone()))
        .expect("windowed probe");
    let seen = probe.stats.row_groups_scanned + probe.stats.row_groups_pruned;
    assert!(
        seen < baseline_row_groups,
        "the 1h window must leave DataFusion fewer row groups than the {baseline_row_groups} \
         it saw unwindowed (saw {seen}); stats={:?}",
        probe.stats,
    );
    eprintln!(
        "b2/real-corpus: {dir} — windowed 1h [{}, {}) → {} rows; DataFusion saw {} row groups \
         (scanned {}, {} B) vs {} unwindowed → {} row groups pruned by the time window \
         (across {} partitions)",
        hour_start,
        hour_end,
        probe.rows,
        seen,
        probe.stats.row_groups_scanned,
        probe.stats.bytes_read,
        baseline_row_groups,
        baseline_row_groups - seen,
        built.files,
    );
    Some(windowed)
}

criterion_group!(benches, synthetic, real_corpus);
criterion_main!(benches);
