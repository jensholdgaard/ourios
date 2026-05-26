//! RFC0006.2 — C1 = 100% on the seed corpus, mismatch is a hard failure.
//! See `docs/rfcs/0006-bench-harness.md` §5.
//!
//! Asserts:
//!
//! - `non_lossy_reconstruct_ok / non_lossy_total = 1.000000`
//!   on the committed seed corpus (six-decimal precision per
//!   §3.4.2; three decimals are insufficient for 100% target).
//! - The results JSON records `c1.pass = true` when all
//!   non-lossy rows reconstruct exactly.
//!
//! The mismatch sub-criterion (a non-lossy row whose
//! `reconstruct` ≠ the ingested bytes must fail the gate) is
//! covered by the colocated unit test
//! `c1::tests::reconstruction_mismatch_is_counted_as_failure`
//! in `src/c1.rs`: the real miner never produces a non-lossy
//! mismatch (the H7.1 property), so the path is only reachable
//! via a hand-forged record, which is a unit-level concern.
//! `main.rs` maps the resulting `pass = false` to a non-zero
//! process exit (§3.4.2).

use ourios_bench::{BenchConfig, GateSet, run};
use std::path::PathBuf;

/// Scenario RFC0006.2 — C1 = 100% on the seed corpus.
#[test]
fn rfc0006_2_c1_is_100_percent_on_seed_corpus() {
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
            a1: false,
            c1: true,
            c2: false,
        },
    };

    let results_file = run(&config).expect("C1 implemented in PR-I1");
    let c1 = results_file.c1.expect("c1 populated when --gates c1");

    assert_eq!(
        c1.non_lossy_reconstruct_ok, c1.non_lossy_total,
        "RFC 0006 §3.4.2: every non-lossy row must reconstruct byte-for-byte",
    );
    assert!(
        (c1.rate - 1.0).abs() < 1e-7,
        "rate must equal 1.000000 to six-decimal precision, got {}",
        c1.rate,
    );
    assert!(c1.pass, "c1.pass must be true when rate = 1.000000");
}

// The mismatch sub-criterion is exercised by
// `c1::tests::reconstruction_mismatch_is_counted_as_failure`
// (a colocated unit test in `src/c1.rs`), which replaced the
// `#[ignore]`'d end-to-end stub that lived here: forging a
// non-lossy mismatch can't be driven through the real miner
// (the H7.1 property guarantees non-lossy rows reconstruct),
// so it's a unit-level fixture concern, not an integration
// test.
