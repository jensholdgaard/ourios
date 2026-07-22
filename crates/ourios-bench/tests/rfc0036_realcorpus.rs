//! RFC 0036 §9.29 — real-corpus window-materialization before/after
//! (indicative, local, Ourios-only, no Loki).
//!
//! §9.26 proved the frozen RFC 0031 comparative harness writes one ingest
//! file per partition, so `compact_partition` no-ops and RFC 0036's sort never
//! runs there — every `ourios_bytes_read` was byte-identical pre/post-0036.
//! §9.27 measured the before/after on a *synthetic* hour (1.43×). This test is
//! the real-otel-demo-corpus analogue: it builds the store two ways from a
//! **subset** of the v8 capture —
//!
//! - **before**: the default single-file [`build_comparative_store`] (no
//!   compaction — the frozen store shape);
//! - **after**: the opt-in [`build_comparative_store_compacted_with_threshold`]
//!   (two ingest files per partition, then `compact_partition` sorts by
//!   (`service.name`, time), rotates row groups, and declares
//!   `sorting_columns`) —
//!
//! then runs one L6-shape window query (one service, a narrow time window in
//! the busiest hour) against each and reads the **materialization bytes** (the
//! footer survivor-chunk sum, the RFC 0036 §9 metric — *not* the count-scan
//! `stats.bytes_read`) plus the RFC 0016 `row_groups_scanned`.
//!
//! `#[ignore]`d and heavy (mines a real-corpus subset twice). It **skips with
//! a clear message** when the gitignored v8 capture is absent, so it never
//! fails CI or another machine. Run manually to (re)produce §9.29:
//!
//! ```text
//! OURIOS_V8_CORPUS=scratch/baseline/otel-demo-v8/logs.jsonl.gz \
//!   cargo test -p ourios-bench --test rfc0036_realcorpus -- --ignored --nocapture
//! ```
//!
//! Knobs (env): `OURIOS_V8_CORPUS` (path to `logs.jsonl`/`.jsonl.gz`),
//! `OURIOS_V8_SUBSET_LINES` (default `120_000` `LogsData` batches),
//! `OURIOS_V8_COMPACTED_RG_BYTES` (compacted row-group threshold, default
//! 2 MiB — real per-hour v8 volume is far below the 32 MiB shipped default, so
//! a finer threshold is what rotates the real busy hour into the several
//! service-clustered groups the pruning mechanism needs; §9.28 finding 3. At
//! the shipped 32 MiB a real hour compacts to a single group and the test
//! skips with a "raise subset / lower threshold" message).

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flate2::read::GzDecoder;
use ourios_bench::{
    TxtSeverity, build_comparative_store, build_comparative_store_compacted_with_threshold,
};
use ourios_core::tenant::TenantId;
use ourios_parquet::columns;
use ourios_parquet::promoted::{RESOURCE_PREFIX, SERVICE_NAME_KEY};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::metadata::{ParquetMetaData, RowGroupMetaData};
use parquet::file::statistics::Statistics;

const DEFAULT_SUBSET_LINES: usize = 120_000;
const DEFAULT_COMPACTED_RG_BYTES: usize = 2 * 1024 * 1024;
const NS_PER_SEC: u64 = 1_000_000_000;

/// Resolve the v8 corpus path: `OURIOS_V8_CORPUS` first, else the default
/// gitignored capture locations under the workspace root. `None` ⇒ skip.
fn resolve_corpus() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("OURIOS_V8_CORPUS") {
        let path = PathBuf::from(p);
        return path.exists().then_some(path);
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)?
        .to_path_buf();
    let base = workspace.join("scratch/baseline/otel-demo-v8");
    for candidate in [
        "corpus/logs.jsonl",
        "logs.jsonl",
        "logs.jsonl.gz",
        "corpus/logs.jsonl.gz",
    ] {
        let path = base.join(candidate);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Stream the first `max_lines` non-blank lines of `src` (gzip-transparent)
/// into `dst` as a `.jsonl` corpus file. Returns the number written.
fn write_subset(src: &Path, dst: &Path, max_lines: usize) -> std::io::Result<usize> {
    let file = File::open(src)?;
    let reader: Box<dyn BufRead> = if src.extension().is_some_and(|x| x == "gz") {
        Box::new(BufReader::new(GzDecoder::new(file)))
    } else {
        Box::new(BufReader::new(file))
    };
    let mut out = BufWriter::new(File::create(dst)?);
    let mut written = 0usize;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        out.write_all(line.as_bytes())?;
        out.write_all(b"\n")?;
        written += 1;
        if written >= max_lines {
            break;
        }
    }
    out.flush()?;
    Ok(written)
}

/// Read a Parquet file's footer metadata.
fn read_meta(path: &Path) -> Arc<ParquetMetaData> {
    ParquetRecordBatchReaderBuilder::try_new(File::open(path).expect("open parquet"))
        .expect("read footer")
        .metadata()
        .clone()
}

/// Leaf index of the named top-level column in a row group.
fn leaf_index(rg: &RowGroupMetaData, name: &str) -> usize {
    (0..rg.num_columns())
        .find(|&i| rg.column(i).column_path().string() == name)
        .unwrap_or_else(|| panic!("column `{name}` not found in the row group schema"))
}

/// A row group's `service.name` min/max statistics as UTF-8.
fn service_min_max(rg: &RowGroupMetaData, svc_leaf: usize) -> (String, String) {
    let stats = rg
        .column(svc_leaf)
        .statistics()
        .expect("service.name statistics present");
    let Statistics::ByteArray(v) = stats else {
        panic!("service.name statistics are byte-array, got {stats:?}");
    };
    let min = v.min_opt().expect("min").as_utf8().expect("utf8 min");
    let max = v.max_opt().expect("max").as_utf8().expect("utf8 max");
    (min.to_owned(), max.to_owned())
}

/// A row group's `effective_time_unix_nano` min/max (physical `Int64` nanos).
/// This is the column the querier's `range(...)` prunes on (RFC 0002 §6.2 /
/// RFC 0005 §3.9 — effective := `time_unix_nano` else `observed`), and it is
/// populated for every row, so observed-only records don't drag the min to 0
/// the way the bare `time_unix_nano` column would.
fn time_min_max(rg: &RowGroupMetaData, time_leaf: usize) -> (u64, u64) {
    let stats = rg
        .column(time_leaf)
        .statistics()
        .expect("effective_time_unix_nano statistics present");
    let Statistics::Int64(v) = stats else {
        panic!("effective_time_unix_nano statistics are int64, got {stats:?}");
    };
    let min = u64::try_from(*v.min_opt().expect("min")).expect("non-negative time");
    let max = u64::try_from(*v.max_opt().expect("max")).expect("non-negative time");
    (min, max)
}

/// The queried service's bytes within the window: the summed compressed sizes
/// of the row groups a `service == target ∧ time ∈ window` scan can NOT prune.
/// The RFC 0036 §9 materialization diagnostic — the same shape as the querier
/// test's `window_service_bytes`, but keyed on `effective_time_unix_nano` (the
/// column the comparative store's querier prunes on) rather than
/// `time_unix_nano`. Returns `(survivor indices, byte sum)`.
fn window_service_bytes(
    meta: &ParquetMetaData,
    target: &str,
    window: &std::ops::Range<u64>,
) -> (Vec<usize>, u64) {
    let service_column = format!("{RESOURCE_PREFIX}{SERVICE_NAME_KEY}");
    let svc_leaf = leaf_index(meta.row_group(0), &service_column);
    let time_leaf = leaf_index(meta.row_group(0), columns::EFFECTIVE_TIME_UNIX_NANO);
    let mut overlapping = Vec::new();
    let mut b_sw = 0u64;
    for (i, rg) in meta.row_groups().iter().enumerate() {
        let (smin, smax) = service_min_max(rg, svc_leaf);
        let (tmin, tmax) = time_min_max(rg, time_leaf);
        let service_overlaps = smin.as_str() <= target && target <= smax.as_str();
        let time_overlaps = tmin < window.end && window.start <= tmax;
        if service_overlaps && time_overlaps {
            overlapping.push(i);
            b_sw += u64::try_from(rg.compressed_size()).expect("non-negative size");
        }
    }
    (overlapping, b_sw)
}

/// Recursively collect committed `*.parquet` object paths under `root/data`.
fn committed_parquet_files(root: &Path) -> Vec<PathBuf> {
    // The manifest is the authoritative live set (RFC 0005 §3.9): physical
    // enumeration can pick up orphaned superseded inputs (gc_failures) or a
    // lost-CAS output and skew the "busiest" pick. Prefer the manifest's
    // `files` when a partition dir has one; else (pre-compaction) fall back
    // to physical `*.parquet`.
    let mut out = Vec::new();
    let mut stack = vec![root.join("data")];
    while let Some(dir) = stack.pop() {
        let manifest = dir.join(ourios_parquet::MANIFEST_FILENAME);
        if let Ok(bytes) = std::fs::read(&manifest)
            && let Ok(m) = serde_json::from_slice::<ourios_parquet::Manifest>(&bytes)
        {
            out.extend(m.files.iter().map(|f| dir.join(f)));
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "parquet") {
                out.push(path);
            }
        }
    }
    out
}

/// The busiest compacted partition file — the one with the most row groups
/// (≥ 2, else there is nothing to prune) — plus its row-group count.
fn busiest_multigroup_file(root: &Path) -> Option<(PathBuf, usize)> {
    let mut best: Option<(PathBuf, usize)> = None;
    for path in committed_parquet_files(root) {
        let groups = read_meta(&path).num_row_groups();
        if groups >= 2 && best.as_ref().is_none_or(|(_, g)| groups > *g) {
            best = Some((path, groups));
        }
    }
    best
}

/// Choose the most-prunable real service in the compacted busy file: the
/// candidate (a real min/max value from the footer) contained in the fewest
/// row groups' `service.name` ranges — the one clustering into the tightest
/// contiguous run, so its window is the clearest pruning win. Ties break
/// lexicographically. `None` when every candidate spans all groups (no prune).
fn most_prunable_service(meta: &ParquetMetaData) -> Option<String> {
    let service_column = format!("{RESOURCE_PREFIX}{SERVICE_NAME_KEY}");
    let svc_leaf = leaf_index(meta.row_group(0), &service_column);
    let ranges: Vec<(String, String)> = meta
        .row_groups()
        .iter()
        .map(|rg| service_min_max(rg, svc_leaf))
        .collect();
    let total = ranges.len();
    let mut candidates: Vec<String> = ranges
        .iter()
        .flat_map(|(lo, hi)| [lo.clone(), hi.clone()])
        .collect();
    candidates.sort();
    candidates.dedup();
    candidates
        .into_iter()
        .map(|svc| {
            let count = ranges
                .iter()
                .filter(|(lo, hi)| lo.as_str() <= svc.as_str() && svc.as_str() <= hi.as_str())
                .count();
            (svc, count)
        })
        .filter(|(_, count)| *count >= 1 && *count < total)
        .min_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)))
        .map(|(svc, _)| svc)
}

/// The whole partition's time span across its row groups.
fn file_time_span(meta: &ParquetMetaData) -> (u64, u64) {
    let time_leaf = leaf_index(meta.row_group(0), columns::EFFECTIVE_TIME_UNIX_NANO);
    let mut lo = u64::MAX;
    let mut hi = 0u64;
    for rg in meta.row_groups() {
        let (tmin, tmax) = time_min_max(rg, time_leaf);
        lo = lo.min(tmin);
        hi = hi.max(tmax);
    }
    (lo, hi)
}

/// Format a nanosecond instant as a whole-second RFC 3339 UTC string (the DSL
/// `range(...)` grammar the existing RFC 0036 tests use).
fn rfc3339_secs(ns: u64) -> String {
    let secs = i64::try_from(ns / NS_PER_SEC).expect("seconds fit i64");
    chrono::DateTime::from_timestamp(secs, 0)
        .expect("valid instant")
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Run the L6-shape window query and return `(rows, scanned, pruned)`.
async fn run_window_query(
    bucket_root: &Path,
    tenant: &str,
    src: &str,
    now_unix_nano: u64,
) -> (u64, u64, u64) {
    let query = ourios_querier::dsl::parse(src).expect("parse DSL");
    let result = ourios_querier::Querier::new(bucket_root)
        .run_query(
            &query,
            &TenantId::new(tenant),
            now_unix_nano,
            365 * 24 * 3_600 * NS_PER_SEC,
            Some(&ourios_core::alias::AliasMap::new()),
        )
        .await
        .expect("run_query");
    (
        result.rows,
        result.stats.row_groups_scanned,
        result.stats.row_groups_pruned,
    )
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

// A measurement harness: subset → build both stores → pick the busy hour →
// choose the target/window → measure both. The steps are sequential and each
// feeds the next, so factoring them into helpers would only scatter the
// data-flow; the length is inherent to a self-contained before/after.
#[allow(clippy::too_many_lines)]
#[ignore = "heavy RFC 0036 §9.29 real-corpus measurement — run manually with the v8 capture present"]
#[tokio::test]
async fn rfc0036_realcorpus_window_materialization_before_after() {
    let Some(corpus_src) = resolve_corpus() else {
        eprintln!(
            "RFC0036 §9.29: SKIP — v8 capture not found. Set OURIOS_V8_CORPUS to a \
             logs.jsonl[.gz], or place it under scratch/baseline/otel-demo-v8/. \
             (The corpus is gitignored; this measurement is indicative and manual.)"
        );
        return;
    };
    let subset_lines = env_usize("OURIOS_V8_SUBSET_LINES", DEFAULT_SUBSET_LINES);
    let threshold = env_usize("OURIOS_V8_COMPACTED_RG_BYTES", DEFAULT_COMPACTED_RG_BYTES);

    // --- Subset the capture into a temp corpus dir ---
    let corpus_dir = tempfile::TempDir::new().expect("corpus dir");
    let subset_path = corpus_dir.path().join("subset.jsonl");
    let lines = write_subset(&corpus_src, &subset_path, subset_lines).expect("write subset");
    let subset_bytes = std::fs::metadata(&subset_path).expect("stat subset").len();
    eprintln!(
        "RFC0036 §9.29: corpus {} — subset {lines} LogsData batches ({subset_bytes} B); \
         compacted row-group threshold {threshold} B",
        corpus_src.display(),
    );

    // --- Build both stores from the identical subset ---
    let before_bucket = tempfile::TempDir::new().expect("before bucket");
    let before =
        build_comparative_store(corpus_dir.path(), before_bucket.path(), TxtSeverity::Fixed)
            .expect("before build (single-file, uncompacted)");

    let after_bucket = tempfile::TempDir::new().expect("after bucket");
    let after = build_comparative_store_compacted_with_threshold(
        corpus_dir.path(),
        after_bucket.path(),
        TxtSeverity::Fixed,
        Some(threshold),
    )
    .expect("after build (multi-file, compacted)");
    assert_eq!(
        before.rows, after.rows,
        "both builds mine the same subset → identical row totals",
    );
    let tenant = after.tenant;

    // --- Pick the busiest compacted partition (most row groups) ---
    let Some((after_path, after_groups)) = busiest_multigroup_file(after_bucket.path()) else {
        eprintln!(
            "RFC0036 §9.29: SKIP — no compacted partition rotated into ≥ 2 row groups at \
             threshold {threshold} B on this {lines}-batch subset (real per-hour v8 volume is \
             small; §9.28 finding 3). Raise OURIOS_V8_SUBSET_LINES or lower \
             OURIOS_V8_COMPACTED_RG_BYTES."
        );
        return;
    };
    let after_meta = read_meta(&after_path);

    // The same partition in the before store (its single ingest file): the
    // after file lives at <after_bucket>/data/.../hour=NN/<uuid>.parquet; the
    // before store's matching partition dir is the same relative path.
    let rel = after_path
        .strip_prefix(after_bucket.path())
        .expect("after path under bucket");
    let before_partition_dir = before_bucket.path().join(rel.parent().expect("hour dir"));
    let before_files: Vec<PathBuf> = std::fs::read_dir(&before_partition_dir)
        .expect("read before partition dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "parquet"))
        .collect();
    assert_eq!(
        before_files.len(),
        1,
        "the uncompacted store is one ingest file per partition, found {before_files:?}",
    );
    let before_meta = read_meta(&before_files[0]);

    // --- Choose the target service + window from the compacted footer ---
    let target = most_prunable_service(&after_meta)
        .expect("the sorted compacted file has a prunable service");
    let (t_lo, t_hi) = file_time_span(&after_meta);
    let start_ns = (t_lo / NS_PER_SEC) * NS_PER_SEC;
    let end_ns = (t_hi / NS_PER_SEC + 1) * NS_PER_SEC;
    let window = start_ns..end_ns;
    let src = format!(
        "service == \"{target}\" | range({}, {})",
        rfc3339_secs(start_ns),
        rfc3339_secs(end_ns),
    );
    let now = end_ns + 365 * 24 * 3_600 * NS_PER_SEC;

    // --- Materialization bytes (footer survivors) + live query, each store ---
    let (before_survivors, before_bytes) = window_service_bytes(&before_meta, &target, &window);
    let (after_survivors, after_bytes) = window_service_bytes(&after_meta, &target, &window);
    let before_total = before_meta.num_row_groups();

    let (before_rows, before_scanned, before_pruned) =
        run_window_query(before_bucket.path(), tenant, &src, now).await;
    let (after_rows, after_scanned, after_pruned) =
        run_window_query(after_bucket.path(), tenant, &src, now).await;

    // Integer ×100 ratio (avoids a float cast under clippy::pedantic): read
    // `NNN` as `N.NNx`. `u128` so the ×100 can't overflow at corpus scale.
    let win_x100 = u128::from(before_bytes) * 100 / u128::from(after_bytes.max(1));
    eprintln!(
        "RFC0036 §9.29 before/after (query `{src}`, target service `{target}`):\n  \
         before  — uncompacted single file: survivors {}/{} groups, {} materialization bytes \
         (query scanned {before_scanned}, pruned {before_pruned}), rows {before_rows}\n  \
         after   — compacted @ {threshold} B: survivors {}/{} groups, {} materialization bytes \
         (query scanned {after_scanned}, pruned {after_pruned}), rows {after_rows}\n  \
         materialization-bytes win {}.{:02}x",
        before_survivors.len(),
        before_total,
        before_bytes,
        after_survivors.len(),
        after_groups,
        after_bytes,
        win_x100 / 100,
        win_x100 % 100,
    );

    // --- Correctness + mechanism assertions (the win is real, not the answer) ---
    assert!(
        before_rows > 0,
        "the window selects some target-service rows"
    );
    assert_eq!(
        before_rows, after_rows,
        "the layout change is transparent to the answer",
    );
    assert!(
        after_survivors.len() < after_groups,
        "the compacted, sorted store prunes the one-service window to a subset of its \
         {after_groups} row groups (survivors {after_survivors:?})",
    );
    assert!(
        after_scanned < u64::try_from(after_groups).expect("group count fits u64"),
        "the live compacted query prunes (scanned {after_scanned} of {after_groups} groups)",
    );
    assert!(
        after_bytes < before_bytes,
        "compaction materialises strictly fewer bytes for the same window \
         (after {after_bytes} < before {before_bytes})",
    );
}
