//! RFC0006.7 — Bench is reproducible across runs.
//! See `docs/rfcs/0006-bench-harness.md` §5.
//!
//! Runs the bench twice on the same git checkout and same
//! corpus with no code or data changes in between; asserts
//! every measurement field of the results JSON is bit-identical
//! across the two runs.
//!
//! Per §3.6 the JSON carries no implementation-detail paths
//! (no `parquet_dir`, no `audit_dir`, no results filename), so
//! the only legitimate diff in the JSON is the `timestamp`
//! field. The test normalises that field then compares the
//! two `ResultsFile`s via the canonical-JSON byte form —
//! comparing serialised JSON bytes rather than `PartialEq` on
//! the struct itself rules out the `f64::PartialEq` edge case
//! where `-0.0 == 0.0` would mask a true bit-level
//! difference. JSON renders `-0.0` and `0.0` distinctly, so
//! the string compare *is* bit-identical for every float in
//! the §3.6 schema.

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
    // canonical-JSON serialisations byte-for-byte. Every
    // other field — `rfc`, `rfc_version`, `git_sha`,
    // `hardware_kind`, the corpus counters, ourios bytes,
    // zstd bytes, every gate's payload including
    // `convergence_curve` and the per-gate `pass` flags —
    // must be bit-identical under the §3.6 determinism
    // contract. Comparing serialised JSON (rather than
    // `assert_eq!` on the struct directly) sidesteps the
    // `f64::PartialEq` edge case where `-0.0 == 0.0` would
    // mask a true bit-level diff — JSON renders the two
    // distinctly. Comparing the canonical form also catches
    // nondeterminism in any field a future RFC amendment
    // adds to `ResultsFile` without the test needing a
    // follow-up.
    let pinned_timestamp = "2026-01-01T00:00:00.000Z".to_string();
    run_a.timestamp = pinned_timestamp.clone();
    run_b.timestamp = pinned_timestamp;
    let json_a = serde_json::to_string(&run_a).expect("serialise run_a");
    let json_b = serde_json::to_string(&run_b).expect("serialise run_b");
    assert_eq!(
        json_a, json_b,
        "RFC 0006 §3.6 determinism: canonical-JSON form of every non-timestamp field must \
         be bit-identical",
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
