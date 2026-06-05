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
//! `b1/otel-demo` is deferred to the corpus run: it needs the OTLP/JSON
//! corpus loader's per-record severity (the plain-text loader forces
//! INFO) plus in-window raw-line extraction with a real time window —
//! wired when the staged corpus lands, not here.

use std::hint::black_box;
use std::path::Path;

use criterion::{Criterion, criterion_group, criterion_main};

use ourios_bench::ReferenceCorpus;
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

criterion_group!(benches, synthetic);
criterion_main!(benches);
