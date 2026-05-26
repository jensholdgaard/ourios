//! RFC0006.1 — A1 formula is well-defined on the seed corpus.
//! See `docs/rfcs/0006-bench-harness.md` §5.
//!
//! Asserts every formula leg of the §3.4.1 A1 measurement
//! against the corpus committed under `testdata/corpus/`:
//!
//! - `bytes(raw_corpus)` from `std::fs::metadata` on the
//!   `*.txt` inputs.
//! - `bytes(ourios_output)` from the bench's bucket dir
//!   (`data/...` plus `audit/...`, post-rename `*.parquet`
//!   files only).
//! - `bytes(zstd_corpus)` from the `zstd` crate at level 19
//!   (per the §7 ZSTD-integration resolution; not a shell-out
//!   to the system `zstd` binary).
//! - The reported `delta = ourios_ratio / zstd_ratio` rounded
//!   down to three significant figures.
//!
//! Un-`#[ignore]`'d in PR-I2 when the A1 measurement landed.
//! The test independently recomputes each byte count from the
//! artifacts on disk (corpus `*.txt`, bucket `*.parquet`,
//! per-file ZSTD-19) and asserts the bench's reported values
//! match — a buggy `run()` whose internal counters drift from
//! disk fails here.

use ourios_bench::{BenchConfig, BenchError, GateSet, run};
use std::path::PathBuf;

/// Scenario RFC0006.1 — A1 formula is well-defined on the seed corpus.
///
/// `#[allow]`-ed lints below cover test-only concerns:
/// - `cast_precision_loss`: byte counts are u64 but the A1
///   ratio is by definition a float; the corpus is well below
///   `2^52` bytes, so the cast is lossless in practice.
/// - `float_cmp`: `target_delta` is a literal `3.0` per the
///   §3.4.1 pin; the impl stores it as the exact `f64`
///   representation of `3.0`, which is bit-exact.
#[test]
#[allow(clippy::cast_precision_loss, clippy::float_cmp)]
fn rfc0006_1_a1_formula_well_defined_on_seed_corpus() {
    let bucket = tempfile::TempDir::new().expect("temp dir");
    let results = tempfile::TempDir::new().expect("temp dir");
    let config = BenchConfig {
        corpus_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("workspace root")
            .join("testdata/corpus"),
        results_dir: results.path().to_path_buf(),
        bucket_dir: Some(bucket.path().to_path_buf()),
        keep_parquet: true,
        hardware_kind: Some("dev-laptop".to_string()),
        update_benchmarks_md: false,
        gates: GateSet {
            a1: true,
            c1: false,
            c2: false,
        },
    };

    let results_file = run(&config).expect("bench runs once A1 is implemented");
    let a1 = results_file.a1.expect("a1 populated when --gates a1");

    // Independent byte verification — sum the corpus `*.txt`
    // sizes and the bucket's `*.parquet` (post-rename) sizes
    // directly via `fs::metadata`, then compare to the
    // bench-reported counters. A buggy `run()` whose internal
    // counters drift from what's actually on disk fails this
    // arm; reusing `results_file.corpus.raw_bytes` as the
    // expected would mask that drift.
    let actual_raw_bytes = sum_txt_bytes(&config.corpus_dir);
    let actual_parquet_total = sum_parquet_bytes(bucket.path());
    let actual_zstd_bytes = zstd_level_19_bytes(&config.corpus_dir);

    assert_eq!(
        results_file.corpus.raw_bytes, actual_raw_bytes,
        "corpus.raw_bytes must equal sum of `*.txt` file sizes per §3.4.1",
    );
    assert_eq!(
        results_file.ourios.total_parquet_bytes, actual_parquet_total,
        "ourios.total_parquet_bytes must equal sum of post-rename `*.parquet` sizes",
    );
    assert_eq!(
        results_file.ourios.total_parquet_bytes,
        results_file.ourios.data_parquet_bytes + results_file.ourios.audit_parquet_bytes,
        "total = data + audit per §3.6 field-relationship",
    );
    assert_eq!(
        results_file.zstd.compressed_bytes, actual_zstd_bytes,
        "zstd.compressed_bytes must match `zstd` level-19 output on the same corpus",
    );
    assert_eq!(results_file.zstd.level, 19, "§3.4.1 pins ZSTD-19");

    // Formula assertions. The §3.4.1 rounding rule says the
    // emitted ratios are rounded *down* to three significant
    // figures; we use a 1% relative tolerance — wide enough
    // for the 3-sigfig truncation to land inside (3-sigfig
    // precision is roughly 0.5% in the worst case), tight
    // enough to catch a formula-leg bug. The precise
    // truncation contract lands as a unit test in
    // `src/a1.rs::tests` when that module is extracted.
    let expected_ourios_ratio = actual_raw_bytes as f64 / actual_parquet_total as f64;
    let expected_zstd_ratio = actual_raw_bytes as f64 / actual_zstd_bytes as f64;
    let expected_delta = expected_ourios_ratio / expected_zstd_ratio;
    assert_within_1_percent(a1.ourios_ratio, expected_ourios_ratio, "ourios_ratio");
    assert_within_1_percent(a1.zstd_ratio, expected_zstd_ratio, "zstd_ratio");
    assert_within_1_percent(a1.delta, expected_delta, "delta");
    assert_eq!(a1.target_delta, 3.0, "§3.4.1 pins target ≥ 3×");
}

/// Relative-tolerance comparison: `actual` must be within
/// 1% of `expected`. The message names the field and reports
/// both values so a failure diagnoses cleanly.
#[allow(clippy::cast_precision_loss, clippy::float_cmp)]
fn assert_within_1_percent(actual: f64, expected: f64, field: &str) {
    if expected == 0.0 {
        // Avoid divide-by-zero on the relative check — for an
        // expected zero, demand exact match.
        assert!(actual == 0.0, "{field}: expected 0.0 (exact), got {actual}");
        return;
    }
    let relative_diff = (actual - expected).abs() / expected.abs();
    assert!(
        relative_diff <= 0.01,
        "{field}: expected {expected}, got {actual} — relative drift {relative_diff:.4} > 1%",
    );
}

/// Sum `*.txt` file sizes under `dir` via `fs::metadata`,
/// **recursively** — the independent reference for
/// `bytes(raw_corpus)` per §3.4.1. The RFC describes the
/// formula as "sum of `std::fs::metadata(p).len()` for every
/// `*.txt` file in the corpus directory", which is naturally
/// recursive (`find … -name '*.txt'`). The committed seed
/// corpus is flat today but a future nested layout under
/// `testdata/corpus/<archetype>/...` would silently
/// undercount if this helper were single-level. Matches the
/// `sum_parquet_bytes` recursion shape below.
fn sum_txt_bytes(dir: &std::path::Path) -> u64 {
    fn walk(dir: &std::path::Path, total: &mut u64) {
        for entry in std::fs::read_dir(dir).expect("corpus dir readable") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                walk(&path, total);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("txt"))
            {
                *total += entry.metadata().expect("metadata").len();
            }
        }
    }
    let mut total = 0u64;
    walk(dir, &mut total);
    total
}

/// Sum every `*.parquet` (post-rename) file under `bucket`,
/// recursing through both `data/...` and `audit/...` partition
/// trees per §3.4.1. Skips `*.parquet.tmp` per RFC 0005 §7's
/// atomic-publish convention.
fn sum_parquet_bytes(bucket: &std::path::Path) -> u64 {
    fn walk(dir: &std::path::Path, total: &mut u64) {
        for entry in std::fs::read_dir(dir).expect("bucket dir readable") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                walk(&path, total);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("parquet"))
            {
                *total += entry.metadata().expect("metadata").len();
            }
        }
    }
    let mut total = 0u64;
    walk(bucket, &mut total);
    total
}

/// Compute `bytes(zstd_corpus)` independently by encoding each
/// `*.txt` file (recursive) with `zstd` at level 19 and
/// summing the compressed lengths. Per-file, not concatenated,
/// per §3.4.1 — the bench under test must produce the
/// identical value.
///
/// Uses the streaming encoder (`zstd::stream::copy_encode`) to
/// match the production path exactly: one-shot
/// `zstd::bulk::compress` can emit a different frame header
/// (it knows the content size up front, streaming doesn't), so
/// the byte counts would diverge by a few header bytes if the
/// two sides used different APIs.
fn zstd_level_19_bytes(dir: &std::path::Path) -> u64 {
    fn walk(dir: &std::path::Path, total: &mut u64) {
        for entry in std::fs::read_dir(dir).expect("corpus dir readable") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                walk(&path, total);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("txt"))
            {
                let file = std::fs::File::open(&path).expect("open txt");
                let mut compressed = Vec::new();
                zstd::stream::copy_encode(file, &mut compressed, 19).expect("zstd compress");
                *total += compressed.len() as u64;
            }
        }
    }
    let mut total = 0u64;
    walk(dir, &mut total);
    total
}

/// Sanity guard against `BenchError::NotImplemented` slipping
/// through the implementation PR. Confirms the scaffold-stage
/// return value is gone once A1 lands. Scoped to `--gates a1`
/// only so a C1- or C2-not-yet-implemented `NotImplemented`
/// can't make this guard fail for the wrong reason — once A1
/// lands, this guard turns green even before C1/C2 do.
#[test]
fn rfc0006_1_run_no_longer_returns_not_implemented() {
    let bucket = tempfile::TempDir::new().expect("temp dir");
    let results = tempfile::TempDir::new().expect("temp dir");
    let config = BenchConfig {
        corpus_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("workspace root")
            .join("testdata/corpus"),
        results_dir: results.path().to_path_buf(),
        bucket_dir: Some(bucket.path().to_path_buf()),
        keep_parquet: false,
        hardware_kind: Some("dev-laptop".to_string()),
        update_benchmarks_md: false,
        gates: GateSet {
            a1: true,
            c1: false,
            c2: false,
        },
    };
    match run(&config) {
        Ok(_) => (),
        Err(BenchError::NotImplemented { what }) => {
            panic!("run() still scaffolded after A1 landed: {what}")
        }
        Err(other) => panic!("unexpected error from implemented run(): {other}"),
    }
}

/// A1 refuses to measure into a `--bucket-dir` that already
/// holds a Parquet file — a prior `--keep-parquet` run's
/// artifacts would otherwise inflate `bytes(ourios_output)`
/// and skew the ratio. The guard fires before the miner runs,
/// so a pre-seeded `data/.../x.parquet` is enough to trip it.
#[test]
fn a1_rejects_a_bucket_that_already_holds_parquet() {
    let bucket = tempfile::TempDir::new().expect("temp dir");
    let results = tempfile::TempDir::new().expect("temp dir");
    // Pre-seed a stray Parquet file under the data subtree.
    let stale = bucket.path().join("data").join("tenant_id=x");
    std::fs::create_dir_all(&stale).expect("mkdir");
    std::fs::write(stale.join("stale.parquet"), b"not really parquet").expect("seed");

    let config = BenchConfig {
        corpus_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("workspace root")
            .join("testdata/corpus"),
        results_dir: results.path().to_path_buf(),
        bucket_dir: Some(bucket.path().to_path_buf()),
        keep_parquet: false,
        hardware_kind: Some("dev-laptop".to_string()),
        update_benchmarks_md: false,
        gates: GateSet {
            a1: true,
            c1: false,
            c2: false,
        },
    };
    let err = run(&config).expect_err("dirty bucket must be rejected");
    assert!(
        matches!(&err, BenchError::Cli { detail } if detail.contains("already contains a Parquet file")),
        "expected a Cli error about a non-empty bucket, got {err:?}",
    );
}

/// `--keep-parquet` without `--bucket-dir` is rejected: a
/// scratch bucket's path isn't reported, so keeping it would
/// leave an unfindable directory behind. The guard fires in
/// bucket resolution, before any corpus work.
#[test]
fn keep_parquet_without_bucket_dir_is_rejected() {
    let results = tempfile::TempDir::new().expect("temp dir");
    let config = BenchConfig {
        corpus_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("workspace root")
            .join("testdata/corpus"),
        results_dir: results.path().to_path_buf(),
        bucket_dir: None,
        keep_parquet: true,
        hardware_kind: Some("dev-laptop".to_string()),
        update_benchmarks_md: false,
        gates: GateSet {
            a1: true,
            c1: false,
            c2: false,
        },
    };
    let err = run(&config).expect_err("keep_parquet without bucket_dir must be rejected");
    assert!(
        matches!(&err, BenchError::Cli { detail } if detail.contains("--keep-parquet requires --bucket-dir")),
        "expected a Cli error about --keep-parquet, got {err:?}",
    );
}
