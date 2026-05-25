//! RFC0006.3 — C2 gate ("within 2× of SS by 1 M lines") on a stable corpus.
//! See `docs/rfcs/0006-bench-harness.md` §5.
//!
//! Asserts the §3.4.3 C2 measurement on a synthetic stable
//! corpus of ≥ 1 M lines whose template alphabet is bounded.
//! The corpus is generated into a temp directory on disk by
//! this test rather than being committed to
//! `testdata/corpus/` (per the §5 "constructed by the bench's
//! integration test; not committed to `testdata/corpus/`"
//! pin) — the bench loader reads `*.txt` files from a
//! directory path, so an on-disk temp dir is the smallest
//! shape that exercises the production path.
//!
//! What's pinned:
//!
//! - `c2.corpus_at_least_1m = true`.
//! - `template_count_at_1m_lines` = integer template count at
//!   the sample whose line index is closest to `999_999`.
//! - `template_count_at_end` = integer template count at the
//!   final sample (§3.4.3 SS definition).
//! - `convergence_ratio = template_count_at_1m_lines /
//!   template_count_at_end ≥ 0.5` — the "within 2× of SS"
//!   gate.
//! - `c2.pass = true`.
//! - Convergence curve has exactly
//!   `ceil(total_lines / sample_cadence)` samples.
//!
//! A second sub-test asserts abstention on a `< 1 M-line`
//! corpus: `c2.pass = null`, `c2.corpus_at_least_1m = false`,
//! `c2.template_count_at_1m_lines = null`.

use ourios_bench::{BenchConfig, GateSet, run};
use std::path::PathBuf;

/// Scenario RFC0006.3 — C2 gate on a ≥ 1 M-line stable corpus.
#[test]
#[ignore = "RFC 0006 Red gate — implementation pending"]
fn rfc0006_3_c2_gate_passes_on_stable_corpus() {
    let bucket = tempfile::TempDir::new().expect("temp dir");
    let results = tempfile::TempDir::new().expect("temp dir");
    let corpus = tempfile::TempDir::new().expect("temp dir");
    // The actual corpus generator (write 1.5 M synthetic lines
    // with a bounded template alphabet to `corpus/*.txt`)
    // lands with the C2 implementation PR; this stub
    // documents the shape it produces.
    let config = BenchConfig {
        corpus_dir: corpus.path().to_path_buf(),
        results_dir: results.path().to_path_buf(),
        bucket_dir: Some(bucket.path().to_path_buf()),
        keep_parquet: false,
        hardware_kind: Some("dev-laptop".to_string()),
        update_benchmarks_md: false,
        gates: GateSet {
            a1: false,
            c1: false,
            c2: true,
        },
    };

    let results_file = run(&config).expect("bench runs once C2 is implemented");
    let c2 = results_file.c2.expect("c2 populated when --gates c2");

    assert!(c2.corpus_at_least_1m, "synthetic corpus is 1.5 M lines");
    let count_1m = c2
        .template_count_at_1m_lines
        .expect("template_count_at_1m_lines populated on a ≥ 1 M corpus");
    assert!(
        c2.template_count_at_end >= count_1m,
        "monotonicity: end count must be ≥ 1m-line count",
    );

    let convergence_ratio = c2
        .convergence_ratio
        .expect("convergence_ratio populated on a ≥ 1 M corpus");
    assert!(
        convergence_ratio >= 0.5,
        "§3.4.3 gate: convergence_ratio = {convergence_ratio} must be ≥ 0.5 (within 2× of SS)",
    );

    assert_eq!(c2.pass, Some(true), "c2.pass = true on stable corpus");

    // §3.4.3 sample-count rule: ceil(total_lines / cadence).
    // Pin cadence > 0 explicitly so a future implementation
    // bug that produces `sample_cadence = 0` fails with an
    // actionable message rather than a generic
    // `div_ceil`-induced divide-by-zero panic.
    assert!(
        c2.sample_cadence > 0,
        "§3.4.3 pins sample_cadence = max(1, ceil(lines / 1024)); got 0",
    );
    let expected_samples = c2.total_lines.div_ceil(c2.sample_cadence);
    assert_eq!(
        c2.convergence_curve.len() as u64,
        expected_samples,
        "curve length must equal ceil(total_lines / sample_cadence) per §3.4.3",
    );
}

/// Scenario RFC0006.3 — C2 abstention on a corpus < 1 M lines.
/// §3.4.3 carves out the gate when there aren't enough samples
/// to reach the 1 M-line check; results JSON records
/// `c2.pass = null` rather than `true` or `false`.
#[test]
#[ignore = "RFC 0006 Red gate — implementation pending"]
fn rfc0006_3_c2_abstains_on_short_corpus() {
    let bucket = tempfile::TempDir::new().expect("temp dir");
    let results = tempfile::TempDir::new().expect("temp dir");
    let config = BenchConfig {
        // Seed corpus is < 1 M lines, exercising the
        // abstention path.
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
            a1: false,
            c1: false,
            c2: true,
        },
    };

    let results_file = run(&config).expect("bench runs once C2 is implemented");
    let c2 = results_file.c2.expect("c2 populated when --gates c2");

    assert!(
        !c2.corpus_at_least_1m,
        "seed corpus is well under 1 M lines",
    );
    assert_eq!(
        c2.template_count_at_1m_lines, None,
        "1m-line count abstains on a short corpus",
    );
    assert_eq!(
        c2.convergence_ratio, None,
        "convergence_ratio abstains on a short corpus",
    );
    assert_eq!(
        c2.pass, None,
        "c2.pass = null on short corpus (the gate is not asserted)",
    );
}
