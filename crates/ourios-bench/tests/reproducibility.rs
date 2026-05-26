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
//! two `ResultsFile`s via their stable `serde_json` byte form
//! — comparing serialised JSON bytes rather than `PartialEq`
//! on the struct itself rules out the `f64::PartialEq` edge
//! case where `-0.0 == 0.0` would mask a true bit-level
//! difference. JSON renders `-0.0` and `0.0` distinctly, so
//! the string compare *is* bit-identical for every float in
//! the §3.6 schema.
//!
//! "Stable" here means `serde_json::to_string` on a struct:
//! field order is fixed by the struct definition and the
//! impl is deterministic. This is *not* canonical JSON
//! (RFC 8785 — sorted keys, normalised numbers) — `ResultsFile`
//! is a struct, not a map, so the stronger canonical-form
//! property isn't needed.

use ourios_bench::{BenchConfig, GateSet, run};
use std::path::PathBuf;

/// Scenario RFC0006.7 — two consecutive runs produce bit-identical measurements.
#[test]
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
    // stable serde_json serialisations byte-for-byte. Every
    // other field — `rfc`, `rfc_version`, `git_sha`,
    // `hardware_kind`, the corpus counters, ourios bytes,
    // zstd bytes, every gate's payload including
    // `convergence_curve` and the per-gate `pass` flags —
    // must be bit-identical under the §3.6 determinism
    // contract. Comparing serialised JSON (rather than
    // `assert_eq!` on the struct directly) sidesteps the
    // `f64::PartialEq` edge case where `-0.0 == 0.0` would
    // mask a true bit-level diff — JSON renders the two
    // distinctly. Comparing the serialised form also catches
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
        "RFC 0006 §3.6 determinism: stable serde_json form of every non-timestamp field \
         must be bit-identical",
    );
}

/// Validate that the timestamp matches the §3.6 millisecond-
/// precision RFC3339 form exactly: `YYYY-MM-DDTHH:MM:SS.mmmZ`.
/// Checks every position — the four separator characters at
/// indices 4 / 7 (`-`), 10 (`T`), 13 / 16 (`:`), 19 (`.`),
/// and 23 (`Z`), plus the seventeen digit positions
/// (0–3, 5–6, 8–9, 11–12, 14–15, 17–18, 20–22) are ASCII
/// digits. Tight enough to reject a string like
/// `"aaaaaaaaaaTaaaaaaaa.aaaZ"` (which a length + separator-
/// only check would let through) without taking a `chrono`
/// dependency for full RFC3339 parsing.
///
/// Validation goes through `as_bytes()` (not `&s[i..j]`) so a
/// non-ASCII byte surfaces as the "shape mismatch" assertion
/// message rather than a `panicked at 'byte index … is not a
/// char boundary'` generic panic.
fn parse_rfc3339(s: &str) {
    let bytes = s.as_bytes();
    let shape_ok = bytes.len() == 24
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && bytes[23] == b'Z'
        && [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18, 20, 21, 22]
            .iter()
            .all(|&i| bytes[i].is_ascii_digit());
    assert!(
        shape_ok,
        "RFC 0006 §3.6 pins millisecond-precision RFC3339 (`YYYY-MM-DDTHH:MM:SS.mmmZ`); got {s:?}",
    );
}
