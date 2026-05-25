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
    let run_a = run(&c1).expect("first run");

    let (b2, r2, mut c2) = make_config();
    c2.results_dir = r2.path().to_path_buf();
    c2.bucket_dir = Some(b2.path().to_path_buf());
    let run_b = run(&c2).expect("second run");

    // Corpus-level invariants
    assert_eq!(run_a.corpus.raw_bytes, run_b.corpus.raw_bytes);
    assert_eq!(run_a.corpus.total_lines, run_b.corpus.total_lines);
    assert_eq!(run_a.corpus.total_files, run_b.corpus.total_files);

    // Ourios-output byte counts (writer is deterministic given
    // identical input + identical encoding policy)
    assert_eq!(
        run_a.ourios.data_parquet_bytes,
        run_b.ourios.data_parquet_bytes,
    );
    assert_eq!(
        run_a.ourios.audit_parquet_bytes,
        run_b.ourios.audit_parquet_bytes,
    );
    assert_eq!(
        run_a.ourios.total_parquet_bytes,
        run_b.ourios.total_parquet_bytes,
    );

    // Reference codec byte count (zstd_safe at level 19 is
    // deterministic per the §7 resolution)
    assert_eq!(run_a.zstd.compressed_bytes, run_b.zstd.compressed_bytes);

    // Gate measurements
    let a1_a = run_a.a1.as_ref().expect("a1 populated");
    let a1_b = run_b.a1.as_ref().expect("a1 populated");
    assert!((a1_a.delta - a1_b.delta).abs() < f64::EPSILON);

    let c1_a = run_a.c1.as_ref().expect("c1 populated");
    let c1_b = run_b.c1.as_ref().expect("c1 populated");
    assert_eq!(c1_a.non_lossy_total, c1_b.non_lossy_total);
    assert_eq!(c1_a.non_lossy_reconstruct_ok, c1_b.non_lossy_reconstruct_ok,);
    assert!((c1_a.rate - c1_b.rate).abs() < f64::EPSILON);

    let c2_a = run_a.c2.as_ref().expect("c2 populated");
    let c2_b = run_b.c2.as_ref().expect("c2 populated");
    assert_eq!(c2_a.template_count_at_end, c2_b.template_count_at_end);
    assert_eq!(
        c2_a.template_count_at_1m_lines,
        c2_b.template_count_at_1m_lines,
    );

    // `timestamp` and the output filename derived from it are
    // the ONLY legitimate diffs — temp-dir paths aren't in
    // the JSON per §3.6, so they can't contribute. The
    // bucket-dir paths above are different between the two
    // runs (different `tempfile::TempDir`s) but don't show up
    // in `run_a` / `run_b`.
    assert_ne!(
        run_a.timestamp, run_b.timestamp,
        "timestamp is the only legitimate diff and must differ",
    );
}
