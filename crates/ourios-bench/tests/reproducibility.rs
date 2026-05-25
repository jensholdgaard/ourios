//! RFC0006.7 — Bench is reproducible across runs.
//! See `docs/rfcs/0006-bench-harness.md` §5.
//!
//! Runs the bench twice on the same git checkout and same
//! corpus with no code or data changes in between; asserts
//! every measurement field of the results JSON is bit-identical
//! across the two runs.
//!
//! Per §3.6, temp-directory paths are intentionally NOT in
//! the JSON, so the only legitimate diffs are `timestamp`
//! (wall-clock) and the derived output filename. This test
//! pins that the bench has no other source of nondeterminism.

use ourios_bench::{BenchConfig, GateSet, run};
use std::path::PathBuf;

/// Scenario RFC0006.7 — two consecutive runs produce bit-identical measurements.
#[test]
#[ignore = "RFC 0006 Red gate — implementation pending"]
fn rfc0006_7_two_runs_produce_bit_identical_measurements() {
    let make_config = || {
        let bucket = tempfile::TempDir::new().expect("temp dir");
        let results = tempfile::TempDir::new().expect("temp dir");
        (
            bucket,
            results,
            BenchConfig {
                corpus_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .and_then(std::path::Path::parent)
                    .expect("workspace root")
                    .join("testdata/corpus"),
                results_dir: PathBuf::new(),
                bucket_dir: None,
                keep_parquet: false,
                hardware_kind: Some("dev-laptop".to_string()),
                update_benchmarks_md: false,
                gates: GateSet::all(),
            },
        )
    };

    let (b1, r1, mut c1) = make_config();
    c1.results_dir = r1.path().to_path_buf();
    c1.bucket_dir = Some(b1.path().to_path_buf());
    let mut run_a = run(&c1).expect("first run");

    let (b2, r2, mut c2) = make_config();
    c2.results_dir = r2.path().to_path_buf();
    c2.bucket_dir = Some(b2.path().to_path_buf());
    let mut run_b = run(&c2).expect("second run");

    // Both timestamps must be RFC3339-parseable per §3.6 — but
    // we do *not* assert they differ. Two runs landing in the
    // same millisecond is rare but possible on a fast machine,
    // and §3.6's collision-retry rule means the *filenames*
    // still disambiguate via the retry counter. The
    // reproducibility contract is "non-timestamp fields are
    // bit-identical", not "timestamps necessarily disagree".
    parse_rfc3339(&run_a.timestamp);
    parse_rfc3339(&run_b.timestamp);

    // Normalise the one volatile field, then compare the
    // entire `ResultsFile`. Every other field — `rfc`,
    // `rfc_version`, `git_sha`, `hardware_kind`, the corpus
    // counters, ourios bytes, zstd bytes, every gate's
    // payload including `convergence_curve` and the per-gate
    // `pass` flags — must be bit-identical under the §3.6
    // determinism contract. Comparing the full struct (rather
    // than enumerating fields by hand as the earlier draft
    // did) catches nondeterminism in any field a future RFC
    // amendment adds to `ResultsFile` without the test
    // needing a follow-up.
    let pinned_timestamp = "2026-01-01T00:00:00.000Z".to_string();
    run_a.timestamp = pinned_timestamp.clone();
    run_b.timestamp = pinned_timestamp;
    assert_eq!(
        run_a, run_b,
        "RFC 0006 §3.6 determinism: every non-timestamp field must be bit-identical",
    );
}

/// Sanity-check that the timestamp looks like an RFC3339
/// string with millisecond precision per §3.6
/// (`YYYY-MM-DDTHH:MM:SS.mmmZ`). The bench doesn't depend on
/// `chrono` for parsing — string-shape validation is enough
/// here, since the determinism contract is what the
/// `assert_eq!` above pins.
fn parse_rfc3339(s: &str) {
    assert!(
        s.len() == 24 && s.ends_with('Z') && &s[10..11] == "T" && &s[19..20] == ".",
        "RFC 0006 §3.6 pins millisecond-precision RFC3339 (`YYYY-MM-DDTHH:MM:SS.mmmZ`); got {s:?}",
    );
}
