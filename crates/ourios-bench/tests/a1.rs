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
//! - `bytes(zstd_corpus)` from the `zstd_safe` Rust crate at
//!   level 19 (per the §7 ZSTD-integration resolution; not a
//!   shell-out to the system `zstd` binary).
//! - The reported `delta = ourios_ratio / zstd_ratio` rounded
//!   down to three significant figures.
//!
//! Stubs are tagged `#[ignore]` so the default `cargo test`
//! is unaffected while the RFC is at the `red` maturity stage.
//! The implementation PR that lands the A1 measurement deletes
//! the `#[ignore]` attribute alongside its code change.

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
#[ignore = "RFC 0006 Red gate — implementation pending"]
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

    // bytes(raw_corpus) is the sum of *.txt file sizes under
    // the corpus directory — pinned by §3.4.1.
    let raw_bytes = results_file.corpus.raw_bytes;
    assert!(raw_bytes > 0, "raw_corpus must be non-empty");

    // bytes(ourios_output) = data_parquet_bytes +
    // audit_parquet_bytes. The §3.4.1 formula operates on the
    // sum; the split is reported for diagnostic transparency.
    let parquet_total = results_file.ourios.total_parquet_bytes;
    assert_eq!(
        parquet_total,
        results_file.ourios.data_parquet_bytes + results_file.ourios.audit_parquet_bytes,
        "total = data + audit per §3.6 field-relationship",
    );
    assert!(parquet_total > 0, "ourios_output must be non-empty");

    // bytes(zstd_corpus) at level 19 per the §7 resolution.
    assert_eq!(results_file.zstd.level, 19, "§3.4.1 pins ZSTD-19");
    assert!(
        results_file.zstd.compressed_bytes > 0,
        "zstd_corpus must be non-empty",
    );

    // delta = ourios_ratio / zstd_ratio. The §3.4.1 rounding
    // rule ("rounded down to three significant figures") is
    // exercised by the equality below — the impl truncates at
    // emit time, so this comparison sees the rounded value.
    let expected_ourios_ratio = raw_bytes as f64 / parquet_total as f64;
    let expected_zstd_ratio = raw_bytes as f64 / results_file.zstd.compressed_bytes as f64;
    let expected_delta = expected_ourios_ratio / expected_zstd_ratio;
    assert!(
        (a1.ourios_ratio - expected_ourios_ratio).abs() < 0.01,
        "ourios_ratio drift from formula > 1% (impl rounded to 3 sigfigs)",
    );
    assert!(
        (a1.zstd_ratio - expected_zstd_ratio).abs() < 0.01,
        "zstd_ratio drift from formula > 1%",
    );
    assert!(
        (a1.delta - expected_delta).abs() < 0.01,
        "delta drift from formula > 1%",
    );
    assert_eq!(a1.target_delta, 3.0, "§3.4.1 pins target ≥ 3×");
}

/// Sanity guard against `BenchError::NotImplemented` slipping
/// through the implementation PR. Confirms the scaffold-stage
/// return value is gone once A1 lands; the ignored attribute
/// above the gate test is the maturity-model marker, this
/// guard catches "we forgot to remove the placeholder".
#[test]
#[ignore = "RFC 0006 Red gate — implementation pending"]
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
        gates: GateSet::all(),
    };
    match run(&config) {
        Ok(_) => (),
        Err(BenchError::NotImplemented { what }) => {
            panic!("run() still scaffolded after A1 landed: {what}")
        }
        Err(other) => panic!("unexpected error from implemented run(): {other}"),
    }
}
