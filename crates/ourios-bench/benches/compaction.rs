//! D2 / D3 — background-compaction throughput & small-file collapse
//! (RFC 0009 §6 / RFC0009.7; `docs/benchmarks.md` D2/D3).
//!
//! Supportive, **non-gating**, **indicative** wall-clock evidence for the
//! validated-side measure of RFC 0009. The *structural* side — that
//! compaction consolidates a partition's live files into one and conserves
//! every row — is pinned deterministically in `ourios-parquet`'s
//! `rfc0009_1_*` / `compaction_conserves_every_row` tests; here we measure
//! the thing those can't: how fast the consolidation runs, and the
//! file-count collapse as a ratio.
//!
//! Two groups:
//!
//! - `d2/compaction-throughput` — time `compact_partition` over a backlog
//!   of `N` small files in one partition, swept across `N`. The per-call
//!   wall-clock, divided into the printed `bytes_read`, is the D2
//!   throughput (MiB/s); it should stay roughly linear in backlog bytes.
//! - `d3/small-file-collapse` — time the same consolidation for one
//!   representative backlog and print the D3 distribution: live files
//!   before → after (N → 1) and rows conserved.
//!
//! **Scale caveat (`ci-runner`).** D3's absolute target — output files in
//! the 256 MiB–2 GiB band, < 5% under 128 MiB (`docs/benchmarks.md` D3) —
//! needs real corpus *volume* and the `baseline-8vcpu-32gib` host; a
//! synthetic CI run produces sub-MiB files, so here D3 is the *structural*
//! collapse (count + row conservation), not the size band. The size-band
//! number is a later authoritative-baseline measurement.

use std::hint::black_box;
use std::path::Path;
use std::time::Instant;

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};

use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, Store, Writer, compact_partition};
use ourios_querier::{Querier, QueryRequest};

/// Anchored in 2026 so the derived partition path is stable; all records
/// fall in one hour ⇒ one partition (the small-file problem is *within* a
/// partition).
const TS0: u64 = 1_775_127_480_000_000_000;
/// Rows per small input file. Modest — the point is the file *count*, not
/// per-file size; every input lands far under the 128 MiB small-file
/// threshold, i.e. a genuine compaction candidate.
const ROWS_PER_FILE: u64 = 2_000;
/// Backlog sizes (live files in one partition) for the D2 sweep.
const D2_BACKLOGS: [u64; 2] = [8, 32];
/// Representative backlog for the D3 collapse measurement.
const D3_FILES: u64 = 64;
/// Small-file count for the B2-post-compaction query comparison.
const B2_POST_FILES: u64 = 32;
/// Bytes per MiB, for the D3 integer-byte size-band classification.
const MIB: u64 = 1 << 20;

/// `len` bytes of pseudo-random printable ASCII (same generator shape as
/// the RFC0005.6 sizing test). Used only by the band-scale baseline mode so
/// the consolidated file reaches the D3 256 MiB–2 GiB band — high entropy
/// keeps the on-disk volume from compressing away. CI mode uses no body.
// The `>> 56 as u8` deliberately keeps the top byte as a pseudo-random
// value; truncation is the intent.
#[allow(clippy::cast_possible_truncation)]
fn high_entropy_body(seed: u64, len: usize) -> String {
    let mut s = String::with_capacity(len);
    let mut x: u64 = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
    for _ in 0..len {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        s.push((((x >> 56) as u8 & 0x3F) + b' ') as char);
    }
    s
}

fn rec(i: u64) -> MinedRecord {
    rec_sized(i, 0)
}

/// A record; with `body_bytes > 0` it carries a high-entropy body of that
/// size (band-scale baseline mode), else no body (CI mode).
fn rec_sized(i: u64, body_bytes: usize) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("c"),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("lib.cart".to_string()),
        scope_version: Some("1.0.0".to_string()),
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        // Keep every record inside one hour so all files share a partition.
        time_unix_nano: TS0 + i * 1_000,
        observed_time_unix_nano: Some(TS0 + i * 1_000 + 1),
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
        body: (body_bytes > 0).then(|| high_entropy_body(i, body_bytes)),
        confidence: 1.0,
        lossy_flag: false,
    }
}

/// Write `num_files` small Parquet files into one partition (each a
/// separate `Writer` cycle ⇒ a separate `<uuid>.parquet`), returning the
/// shared partition key. No manifest is written, so the reader globs all
/// of them as live (RFC 0009 reader-first / glob-fallback) — exactly the
/// sealed-but-uncompacted backlog compaction consolidates.
fn build_backlog(bucket: &Path, num_files: u64) -> PartitionKey {
    build_backlog_sized(bucket, num_files, ROWS_PER_FILE, 0)
}

fn build_backlog_sized(
    bucket: &Path,
    num_files: u64,
    rows_per_file: u64,
    body_bytes: usize,
) -> PartitionKey {
    let partition = PartitionKey::derive(&rec(0)).expect("derive partition");
    for f in 0..num_files {
        let base = f * rows_per_file;
        let records: Vec<MinedRecord> = (0..rows_per_file)
            .map(|i| rec_sized(base + i, body_bytes))
            .collect();
        let mut w = Writer::open(bucket, partition.clone()).expect("open writer");
        w.append_records(&records).expect("append");
        w.close().expect("close");
    }
    partition
}

fn compaction_throughput(c: &mut Criterion) {
    if baseline_params().is_some() {
        return; // band-scale baseline mode runs the one-shot instead
    }
    let mut group = c.benchmark_group("d2/compaction-throughput");
    // Each iteration rebuilds the backlog (compaction is destructive — it
    // consolidates to one file, so a second compaction would be a no-op),
    // so keep the sample count modest.
    group.sample_size(10);
    for num_files in D2_BACKLOGS {
        group.bench_with_input(
            BenchmarkId::from_parameter(num_files),
            &num_files,
            |b, &nf| {
                b.iter_batched(
                    || {
                        // Build the backlog *and* open the store in the untimed
                        // setup phase, so only `compact_partition` is timed (not
                        // store construction / canonicalization). `_dir` is held
                        // to keep the temp bucket alive for the iteration.
                        let dir = tempfile::TempDir::new().expect("temp bucket");
                        let part = build_backlog(dir.path(), nf);
                        let store = Store::local(dir.path()).expect("store");
                        (dir, store, part)
                    },
                    |(_dir, store, part)| {
                        let outcome = compact_partition(&store, &part).expect("compact");
                        black_box(outcome.rows);
                    },
                    BatchSize::PerIteration,
                );
            },
        );
    }
    group.finish();

    // Print the read volume per backlog so the criterion wall-clock can be
    // turned into a MiB/s throughput (D2). One extra build+compact outside
    // the timed loop — cheap relative to the sweep above.
    for num_files in D2_BACKLOGS {
        let dir = tempfile::TempDir::new().expect("temp bucket");
        let part = build_backlog(dir.path(), num_files);
        let store = Store::local(dir.path()).expect("store");
        let outcome = compact_partition(&store, &part).expect("compact");
        eprintln!(
            "d2/compaction-throughput: {num_files} files → {} rows; read {} B, wrote {} B \
             (divide the criterion time for {num_files} into bytes_read for MiB/s)",
            outcome.rows, outcome.bytes_read, outcome.bytes_written,
        );
    }
}

fn small_file_collapse(c: &mut Criterion) {
    if baseline_params().is_some() {
        return; // band-scale baseline mode runs the one-shot instead
    }
    // One-shot D3 measurement: build a representative backlog, consolidate,
    // and report the collapse. Done before the timed group so the print is
    // emitted once with a clean (untimed) outcome.
    let dir = tempfile::TempDir::new().expect("temp bucket");
    let part = build_backlog(dir.path(), D3_FILES);
    let store = Store::local(dir.path()).expect("store");
    let outcome = compact_partition(&store, &part).expect("compact");
    assert!(
        outcome.committed.is_some(),
        "a {D3_FILES}-file backlog must compact (not a no-op)",
    );
    assert_eq!(
        outcome.files_before,
        usize::try_from(D3_FILES).expect("backlog fits usize"),
        "every written file should be live (no manifest ⇒ all globbed)",
    );
    assert_eq!(
        outcome.rows,
        D3_FILES * ROWS_PER_FILE,
        "compaction must conserve every row",
    );
    eprintln!(
        "d3/small-file-collapse: {} live files → 1 ({} rows conserved); output {} B. \
         NOTE: the absolute 256 MiB–2 GiB size band (benchmarks.md D3) needs real corpus \
         volume on baseline-8vcpu-32gib — this synthetic ci-runner run shows the structural \
         count collapse only.",
        outcome.files_before, outcome.rows, outcome.bytes_written,
    );

    // A timed data point for the representative backlog too.
    let mut group = c.benchmark_group("d3/small-file-collapse");
    group.sample_size(10);
    group.bench_function(BenchmarkId::from_parameter(D3_FILES), |b| {
        b.iter_batched(
            || {
                // Open the store in the untimed setup phase so only
                // `compact_partition` is timed; `_d` keeps the bucket alive.
                let d = tempfile::TempDir::new().expect("temp bucket");
                let p = build_backlog(d.path(), D3_FILES);
                let store = Store::local(d.path()).expect("store");
                (d, store, p)
            },
            |(_d, store, p)| {
                let o = compact_partition(&store, &p).expect("compact");
                black_box(o.rows);
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

/// Band-scale baseline parameters, read from the environment. Returns
/// `Some((files, rows_per_file, body_bytes))` when `OURIOS_COMPACTION_BASELINE`
/// is set — the authoritative `baseline-8vcpu-32gib` run sets these to a
/// volume large enough that the consolidated file reaches the D3
/// 256 MiB–2 GiB band (`docs/benchmarks.md` D3). Unset ⇒ CI mode (the
/// criterion micro-sweep above). Defaults: 16 files × 16 000 rows × 2 KiB
/// body ≈ 0.5 GiB of input, tunable via the matching env vars.
fn baseline_params() -> Option<(u64, u64, usize)> {
    // Require an explicit `=1` (not mere presence) so `…=0` can't silently
    // suppress the CI criterion sweeps. A non-UTF-8 value fails fast, like the
    // other baseline vars — not silently treated as "unset".
    match std::env::var("OURIOS_COMPACTION_BASELINE") {
        Ok(v) if v == "1" => {}
        Ok(_) | Err(std::env::VarError::NotPresent) => return None,
        Err(std::env::VarError::NotUnicode(v)) => {
            panic!(
                "OURIOS_COMPACTION_BASELINE={} is not valid UTF-8",
                v.display()
            )
        }
    }
    // Fail fast on a present-but-unparsable value (an authoritative run must
    // not silently fall back to a default and report misleading scale); use
    // the default only when the var is absent.
    let var = |k: &str, d: u64| match std::env::var(k) {
        Ok(v) => v
            .parse()
            .unwrap_or_else(|_| panic!("{k}={v:?} is not a valid u64")),
        // Default only when the var is genuinely absent; a present-but-non-UTF-8
        // value is a misconfig, not "use the default".
        Err(std::env::VarError::NotPresent) => d,
        Err(std::env::VarError::NotUnicode(v)) => {
            panic!("{k}={} is not valid UTF-8", v.display())
        }
    };
    let files = var("OURIOS_COMPACTION_FILES", 16);
    let rows = var("OURIOS_COMPACTION_ROWS", 16_000);
    let body = usize::try_from(var("OURIOS_COMPACTION_BODY_BYTES", 2_048))
        .expect("OURIOS_COMPACTION_BODY_BYTES fits usize");
    Some((files, rows, body))
}

/// One-shot band-scale measurement (not a criterion sweep — the input is
/// too large to rebuild per sample). Builds one backlog, times a single
/// `compact_partition`, and prints the authoritative D2 throughput (MiB/s)
/// + D3 size band (output file bytes, in-band check, count collapse).
// Byte counts → f64 for a MiB/s print; precision loss is irrelevant here.
#[allow(clippy::cast_precision_loss)]
fn baseline(_c: &mut Criterion) {
    let Some((files, rows, body)) = baseline_params() else {
        return; // CI mode — the criterion groups above run instead
    };
    // Below 2 files (or 0 rows) `compact_partition` no-ops, which would make
    // the D2/D3 numbers below meaningless — fail loudly on a misconfig.
    assert!(
        files >= 2,
        "baseline needs >=2 input files to compact (got {files})"
    );
    assert!(rows > 0, "baseline needs rows > 0 (got {rows})");
    let dir = tempfile::TempDir::new().expect("temp bucket");
    eprintln!(
        "compaction/baseline: building {files} files × {rows} rows × {body} B body \
         in one partition…"
    );
    let part = build_backlog_sized(dir.path(), files, rows, body);

    let t0 = Instant::now();
    let store = Store::local(dir.path()).expect("store");
    let outcome = compact_partition(&store, &part).expect("compact");
    // Clamp so a sub-microsecond run can't print an infinite/NaN MiB/s.
    let secs = t0.elapsed().as_secs_f64().max(1e-9);

    // Verify the run did what the D2/D3 lines below claim — a no-op or a
    // short read would otherwise print authoritative-looking but bogus
    // numbers.
    assert!(
        outcome.committed.is_some(),
        "baseline compaction was a no-op (nothing consolidated)"
    );
    assert_eq!(
        outcome.files_before,
        usize::try_from(files).expect("files fits usize"),
        "compaction must see every built file as live",
    );
    assert_eq!(
        outcome.rows,
        files * rows,
        "compaction must conserve every row",
    );

    // Classify in integer bytes to avoid float-boundary misclassification at
    // 128 MiB / 256 MiB / 2 GiB; floats are display-only.
    let in_band = (256 * MIB..=2048 * MIB).contains(&outcome.bytes_written);
    // One live file after compaction, so the D3 "% under 128 MiB" is 0 or 100
    // depending on whether this run was dialed to band scale.
    let pct_under_128 = if outcome.bytes_written < 128 * MIB {
        100.0
    } else {
        0.0
    };
    let read_mib = outcome.bytes_read as f64 / (1024.0 * 1024.0);
    let out_mib = outcome.bytes_written as f64 / (1024.0 * 1024.0);
    eprintln!(
        "compaction/baseline D2: compacted {} files ({read_mib:.1} MiB read) → 1 in {secs:.2}s \
         = {:.1} MiB/s; {} rows conserved",
        outcome.files_before,
        read_mib / secs,
        outcome.rows,
    );
    eprintln!(
        "compaction/baseline D3: output 1 file {out_mib:.1} MiB — \
         {} the 256 MiB–2 GiB band; {pct_under_128:.0}% of live files under 128 MiB after compaction",
        if in_band { "IN" } else { "OUTSIDE" },
    );
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

/// B2-post-compaction (RFC0009.7): the same template-exact query over a
/// partition of `N` small files vs the single consolidated file compaction
/// produces. Compaction conserves the rows, so the query returns the same
/// result either way — the latency drop is the per-file footer/metadata-read
/// overhead compaction removes (the PR #92 B2 finding that motivated RFC
/// 0009). The printed row-group / byte counts are hardware-independent; the
/// criterion latencies are indicative. Skipped in band-scale baseline mode.
fn b2_post_compaction(c: &mut Criterion) {
    if baseline_params().is_some() {
        return;
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("tokio runtime");

    // Uncompacted: N small files (no manifest ⇒ the reader globs all N live).
    let uncompacted = tempfile::TempDir::new().expect("temp bucket");
    build_backlog(uncompacted.path(), B2_POST_FILES);

    // Compacted: the same N files consolidated to one (manifest ⇒ the reader
    // resolves the single live file; the superseded inputs are ignored).
    let compacted = tempfile::TempDir::new().expect("temp bucket");
    let part = build_backlog(compacted.path(), B2_POST_FILES);
    let store = Store::local(compacted.path()).expect("store");
    let outcome = compact_partition(&store, &part).expect("compact");
    assert!(
        outcome.committed.is_some(),
        "b2-post setup must actually compact (not a no-op)"
    );

    let q_unc = Querier::new(uncompacted.path());
    let q_cmp = Querier::new(compacted.path());
    let req = template_exact("c", 1);

    let p_unc = rt
        .block_on(q_unc.run(req.clone()))
        .expect("probe uncompacted");
    let p_cmp = rt
        .block_on(q_cmp.run(req.clone()))
        .expect("probe compacted");
    assert_eq!(
        p_unc.rows, p_cmp.rows,
        "compaction must not change the query result set",
    );
    // Guard against a vacuous 0==0 pass (tenant/template mismatch, writer
    // failure): every row is template 1, so the query must return them all.
    assert_eq!(
        p_unc.rows,
        B2_POST_FILES * ROWS_PER_FILE,
        "b2-post query must return every built row (setup wired correctly)",
    );
    eprintln!(
        "b2-post-compaction: {} rows; uncompacted scanned {} row groups ({} B) across {B2_POST_FILES} \
         files → compacted scanned {} row groups ({} B) in 1 file",
        p_unc.rows,
        p_unc.stats.row_groups_scanned,
        p_unc.stats.bytes_read,
        p_cmp.stats.row_groups_scanned,
        p_cmp.stats.bytes_read,
    );

    let mut group = c.benchmark_group("b2-post-compaction");
    group.bench_function("uncompacted", |b| {
        b.iter(|| {
            let r = rt.block_on(q_unc.run(req.clone())).expect("query");
            black_box(r.rows);
        });
    });
    group.bench_function("compacted", |b| {
        b.iter(|| {
            let r = rt.block_on(q_cmp.run(req.clone())).expect("query");
            black_box(r.rows);
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    compaction_throughput,
    small_file_collapse,
    b2_post_compaction,
    baseline
);
criterion_main!(benches);
