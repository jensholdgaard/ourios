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
//! - A non-zero exit code and stderr diagnostics when any
//!   non-lossy row's `reconstruct(record, template)` doesn't
//!   equal the ingested bytes (covered by the second sub-test,
//!   which constructs a forged record path).
//!
//! Stubs are tagged `#[ignore]` so the default `cargo test`
//! is unaffected while the RFC is at the `red` maturity stage.

use ourios_bench::{BenchConfig, GateSet, run};
use std::path::PathBuf;

/// Scenario RFC0006.2 — C1 = 100% on the seed corpus.
#[test]
#[ignore = "RFC 0006 Red gate — implementation pending"]
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

    let results_file = run(&config).expect("bench runs once C1 is implemented");
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

/// Scenario RFC0006.2 — forged reconstruct mismatch is a hard
/// failure. The bench injects a synthetic record whose
/// `reconstruct(record, template)` disagrees with the input
/// (built by hand, not by the miner) and asserts the bench
/// exits with a non-zero code, emits the failing row's
/// `(template_id, template_version)` + expected / actual bytes
/// to stderr, and records `c1.pass = false` in the results JSON.
///
/// The implementation that actually constructs the forged
/// record needs the harness's per-line capture path; this stub
/// pins the contract until that path exists.
#[test]
#[ignore = "RFC 0006 Red gate — implementation pending"]
fn rfc0006_2_reconstruct_mismatch_is_a_hard_failure() {
    // The forged-record fixture and the bench's
    // non-zero-exit-on-mismatch path land together in the C1
    // implementation PR. This stub exists so the §5 acceptance
    // criterion has a test surface to grow into.
    unimplemented!(
        "RFC 0006 Red gate — fixture for forged reconstruction \
         mismatch lands with the C1 implementation PR"
    )
}
