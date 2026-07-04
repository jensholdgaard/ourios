//! B1 — predicate-pushdown query latency vs. the `zstdcat | grep`
//! reference (`docs/benchmarks.md` §3 B1; RFC0007.1).
//!
//! Supportive **wall-clock** evidence for the B1 thesis gate
//! ("Ourios ≥ 10× faster than `zstdcat files_in_range.zst | grep ERROR
//! | wc -l`"). The structural side — that the selective query prunes
//! row groups rather than scanning them — is proven deterministically
//! in `ourios-querier`'s `rfc0007_1_*` tests; here we measure the thing
//! the structural test can't: the latency ratio.
//!
//! `b1/synthetic` (always runs) — the controlled instrument. A query
//! window holds ERROR rows (one hour/file) + INFO rows (another
//! hour/file); out-of-window ERROR filler sits in a later hour. The
//! Ourios query (`tenant` + time window + `severity_text='ERROR'`)
//! prunes the INFO row group (severity statistics) **and** the
//! out-of-window file (time statistics), so it reads only the in-window
//! ERROR row group. The reference is given the **same in-window file
//! set** (fairness — see [`ourios_bench::ReferenceCorpus`]) and greps
//! every line. Two timings land in the group, `ourios` and
//! `zstd-grep-reference`; the B1 ratio is `reference / ourios`.
//!
//! `b1/real-corpus` (only when `OURIOS_B1_CORPUS_DIRS` is set) — the
//! same two timings over real corpora, one pair per dir. The store is
//! built by [`ourios_bench::build_b1_store`], which also renders the
//! reference (`<severity_text> <body>` per record — the flat file a
//! traditional logger would have written, one zstd block per hour)
//! and picks the severity to query (`ERROR`, or the busiest
//! error-band text). The query window is the corpus's full
//! *effective*-timestamp span (RFC 0005 §3.2 — `timeUnixNano`, else
//! `observedTimeUnixNano`), so the reference scans the same in-window
//! set and observed-only corpora are B1-eligible. **OTLP
//! corpora only**: the RFC 0006 §3.3 plain-text loader fixes every
//! line at severity `9` / `INFO`, so a severity predicate over a
//! plain-text corpus has no selectivity — such dirs are skipped with
//! a note rather than benchmarking a meaningless full-match query
//! (parsing severities out of raw text would be unspecified
//! behaviour, not a loader the RFC pins).

use std::hint::black_box;
use std::path::Path;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use ourios_bench::{ReferenceCorpus, TxtSeverity, build_b1_store};
use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, Writer};
use ourios_querier::{Querier, QueryRequest};

/// 2026-04-02T10:58:00 UTC (hour 10) — stable partition anchor.
const TS0: u64 = 1_775_127_480_000_000_000;
const HOUR_NS: u64 = 3_600_000_000_000;

/// In-window ERROR rows (the B1 result) and INFO rows (pruned by
/// severity), plus out-of-window ERROR filler (pruned by time).
const ERROR_ROWS: u64 = 2_000;
const INFO_ROWS: u64 = 2_000;
const OUT_OF_WINDOW_ROWS: u64 = 4_000;
/// zstd level for the reference `*.zst` (matches A1's reference codec).
const ZSTD_LEVEL: i32 = 19;

fn rec(template_id: u64, ts_ns: u64, severity: &str) -> MinedRecord {
    let severity_number: u8 = match severity {
        "INFO" => 9,
        "ERROR" => 17,
        _ => 0,
    };
    MinedRecord {
        tenant_id: TenantId::new("a"),
        template_id,
        template_version: 1,
        severity_number,
        severity_text: Some(severity.to_string()),
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

/// Write `records` (sharing one partition) as a single file.
fn write_one_file(bucket: &Path, records: &[MinedRecord]) {
    let part = PartitionKey::derive(&records[0]).expect("derive partition");
    let mut w = Writer::open(bucket, part).expect("open writer");
    w.append_records(records).expect("append");
    w.close().expect("close");
}

/// Build the synthetic store and the matching reference corpus. The
/// store gets structured records (severity set); the reference gets the
/// raw lines of the **same in-window files** (hour 10 ERROR + hour 11
/// INFO) — the out-of-window hour-20 file is not part of the reference,
/// just as `files_in_range.zst` wouldn't name it.
fn build(bucket: &Path) -> ReferenceCorpus {
    // hour 10 — the in-window ERROR file (the B1 result).
    let errors: Vec<MinedRecord> = (0..ERROR_ROWS)
        .map(|i| rec(1, TS0 + i * 1_000, "ERROR"))
        .collect();
    write_one_file(bucket, &errors);

    // hour 11 — in-window INFO file (pruned by the severity predicate).
    let infos: Vec<MinedRecord> = (0..INFO_ROWS)
        .map(|i| rec(2, TS0 + HOUR_NS + i * 1_000, "INFO"))
        .collect();
    write_one_file(bucket, &infos);

    // hour 20 — out-of-window ERROR filler (pruned by the time bound).
    let filler: Vec<MinedRecord> = (0..OUT_OF_WINDOW_ROWS)
        .map(|i| rec(3, TS0 + 10 * HOUR_NS + i * 1_000, "ERROR"))
        .collect();
    write_one_file(bucket, &filler);

    // Reference: the raw lines of the two IN-WINDOW files only. The grep
    // token "ERROR" matches the error file's lines, not the info file's.
    let error_lines: Vec<String> = (0..ERROR_ROWS)
        .map(|i| format!("ERROR request failed id={i}"))
        .collect();
    let info_lines: Vec<String> = (0..INFO_ROWS)
        .map(|i| format!("INFO request ok id={i}"))
        .collect();
    ReferenceCorpus::compress(&[error_lines, info_lines], ZSTD_LEVEL).expect("compress reference")
}

/// The B1 query: tenant "a", the half-open window covering hours 10–11,
/// `level='ERROR'`.
fn b1_query() -> QueryRequest {
    QueryRequest {
        tenant: TenantId::new("a"),
        time_range: Some((TS0, TS0 + 2 * HOUR_NS)),
        template_id: None,
        severity_text: Some("ERROR".to_string()),
        limit: None,
    }
}

fn synthetic(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("tokio runtime");

    let bucket = tempfile::TempDir::new().expect("temp bucket");
    let reference = build(bucket.path());
    let querier = Querier::new(bucket.path());

    // Sanity + visibility: both sides return the same B1 result, and the
    // Ourios scan prunes (the comparison is meaningless otherwise).
    let probe = rt.block_on(querier.run(b1_query())).expect("probe query");
    let ref_count = reference
        .count_lines_containing("ERROR")
        .expect("reference");
    assert_eq!(probe.rows, ERROR_ROWS, "Ourios B1 result");
    assert_eq!(ref_count, ERROR_ROWS, "reference B1 result matches Ourios");
    // The bench premise: the Ourios query *prunes* (the INFO + the
    // out-of-window files), so it isn't silently doing a full scan that
    // would make the latency comparison meaningless.
    assert!(
        probe.stats.row_groups_pruned >= 1,
        "Ourios B1 query must prune row groups; stats={:?}",
        probe.stats,
    );
    let total_rg = probe.stats.row_groups_scanned + probe.stats.row_groups_pruned;
    eprintln!(
        "b1/synthetic: result={} rows; ourios pruned {}/{} row groups, read {} B; \
         reference scans {} in-window compressed B",
        probe.rows,
        probe.stats.row_groups_pruned,
        total_rg,
        probe.stats.bytes_read,
        reference.compressed_bytes(),
    );

    let mut group = c.benchmark_group("b1/synthetic");
    group.bench_function("ourios", |b| {
        b.iter(|| {
            let r = rt.block_on(querier.run(b1_query())).expect("query");
            black_box(r.rows);
        });
    });
    group.bench_function("zstd-grep-reference", |b| {
        b.iter(|| {
            let n = reference
                .count_lines_containing("ERROR")
                .expect("reference");
            black_box(n);
        });
    });
    group.finish();
}

/// Real corpora, one `ourios` / `zstd-grep-reference` timing pair per
/// directory in `OURIOS_B1_CORPUS_DIRS` (comma-separated; skipped
/// entirely when unset — the corpora aren't committed, CI / a local
/// operator stages them). Dirs without severity selectivity (plain
/// text — see the module doc), without error-band rows, or without a
/// usable timestamp span are skipped with a note.
fn real_corpus(c: &mut Criterion) {
    let Ok(raw) = std::env::var("OURIOS_B1_CORPUS_DIRS") else {
        eprintln!(
            "b1/real-corpus: OURIOS_B1_CORPUS_DIRS unset — skipping (synthetic group still runs)"
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

    let mut group = c.benchmark_group("b1/real-corpus");
    // GiB-class corpora make one reference scan multi-second; the
    // default 100 samples would run for hours. 10 (criterion's
    // minimum) keeps the indicative timing affordable.
    group.sample_size(10);
    for dir in dirs {
        let path = Path::new(dir);
        if !path.is_dir() {
            eprintln!("b1/real-corpus: {dir} is not a directory — skipping");
            continue;
        }
        let bucket = tempfile::TempDir::new().expect("temp bucket");
        let severity = TxtSeverity::from_env().expect("OURIOS_CORPUS_SEVERITY");
        let built =
            build_b1_store(path, bucket.path(), ZSTD_LEVEL, severity).expect("build b1 store");
        let Some((severity, query)) = severity_query(&built, dir) else {
            continue;
        };

        let querier = Querier::new(bucket.path());
        probe_real_corpus(&rt, &querier, &built, &severity, &query, dir);

        let id = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(dir)
            .to_string();
        group.bench_function(BenchmarkId::new("ourios", &id), |b| {
            b.iter(|| {
                let r = rt.block_on(querier.run(query.clone())).expect("query");
                black_box(r.rows);
            });
        });
        group.bench_function(BenchmarkId::new("zstd-grep-reference", &id), |b| {
            b.iter(|| {
                let n = built
                    .reference
                    .count_lines_containing(&severity)
                    .expect("reference");
                black_box(n);
            });
        });
    }
    group.finish();
}

/// The B1 query for a built real-corpus store: the chosen error-band
/// severity over the corpus's full *effective*-timestamp span
/// (RFC 0005 §3.2 amendment 2026-06-11 — observed-only corpora are
/// B1-eligible; only genuinely timeless rows disqualify), half-open
/// (`max + 1` keeps the last record in-window; clamped like b2's
/// windowed arm so a near-2262 corpus can't push the bound past the
/// querier's i64 range). `None` (with a skip note) when the corpus
/// has no severity selectivity (single `severity_text` — the RFC 0006
/// §3.3 plain-text case), no error-band rows, or no usable span.
fn severity_query(built: &ourios_bench::B1Store, dir: &str) -> Option<(String, QueryRequest)> {
    if built.distinct_severities < 2 {
        eprintln!(
            "b1/real-corpus: {dir} carries a single severity_text — no severity \
             selectivity (the RFC 0006 §3.3 plain-text loader forces INFO); B1 needs \
             an OTLP corpus with real severities — skipping"
        );
        return None;
    }
    let Some((severity, _)) = built.query_severity.clone() else {
        eprintln!("b1/real-corpus: {dir} has no error-band severity_text rows — skipping");
        return None;
    };
    if built.min_effective_time_unix_nano == 0 || built.zero_effective_ts_rows > 0 {
        eprintln!(
            "b1/real-corpus: {dir} — no usable effective-timestamp span ({} \
             zero-effective-ts rows, i.e. neither timeUnixNano nor \
             observedTimeUnixNano); the B1 query needs a real time window — skipping",
            built.zero_effective_ts_rows,
        );
        return None;
    }
    #[allow(clippy::cast_sign_loss)] // i64::MAX as u64 is exact
    let window_end = built
        .max_effective_time_unix_nano
        .saturating_add(1)
        .min(i64::MAX as u64);
    let query = QueryRequest {
        tenant: TenantId::new(built.tenant),
        time_range: Some((built.min_effective_time_unix_nano, window_end)),
        template_id: None,
        severity_text: Some(severity.clone()),
        limit: None,
    };
    Some((severity, query))
}

/// Sanity + visibility before timing: the Ourios result must be
/// exactly the corpus's row count at the chosen severity
/// (tenant/window/predicate wired correctly), and the reference grep
/// may legitimately exceed it (a body can contain the token) but
/// never undercount — every matching record's reference line is
/// severity-prefixed.
fn probe_real_corpus(
    rt: &tokio::runtime::Runtime,
    querier: &Querier,
    built: &ourios_bench::B1Store,
    severity: &str,
    query: &QueryRequest,
    dir: &str,
) {
    let probe = rt
        .block_on(querier.run(query.clone()))
        .expect("probe query");
    let expected_rows = built.query_severity.as_ref().map_or(0, |(_, rows)| *rows);
    assert_eq!(
        probe.rows, expected_rows,
        "the severity query must return the corpus's rows at that severity",
    );
    let ref_count = built
        .reference
        .count_lines_containing(severity)
        .expect("reference");
    assert!(
        ref_count >= probe.rows,
        "reference grep ({ref_count}) must not undercount the severity rows ({})",
        probe.rows,
    );
    eprintln!(
        "b1/real-corpus: {dir} — {} rows, {} files; severity {severity:?} over the full \
         span → {} rows (reference grep: {ref_count}); ourios scanned {}/{} row groups, \
         {} B; reference scans {} compressed B",
        built.rows,
        built.files,
        probe.rows,
        probe.stats.row_groups_scanned,
        probe.stats.row_groups_scanned + probe.stats.row_groups_pruned,
        probe.stats.bytes_read,
        built.reference.compressed_bytes(),
    );
}

criterion_group!(benches, synthetic, real_corpus);
criterion_main!(benches);
