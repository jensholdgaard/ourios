//! `ourios-bench` — RFC 0006 thesis-gate bench harness
//! (A1 compression, C1 reconstruction, C2 template-count
//! convergence).
//!
//! This file is the **Red-stage scaffold** for the RFC. The
//! public surface declared here is what the RFC0006.1–7
//! acceptance-criterion tests will exercise once they land in
//! a follow-up PR; today every entry point returns
//! [`BenchError::NotImplemented`] so the crate compiles, the
//! test stubs link, and the maturity-model gate (`specified`
//! → `red`: stubs exist and fail) is satisfied without
//! pre-committing to an implementation shape the tests would
//! then have to fight.
//!
//! Per RFC 0006 §3.2 the eventual module layout is `corpus`,
//! `harness`, `a1`, `c1`, `c2`, `report`. Those modules land
//! incrementally; this scaffold keeps them collapsed into
//! `lib.rs` so the first scaffold PR stays under CLAUDE.md
//! §5.2's "≤ 5 files per phase" rule. The first per-module
//! split is what flips the maturity gate from `red` → `green`.

#![deny(unsafe_code)]

use std::fmt;
use std::path::PathBuf;

/// Configuration for one bench invocation.
///
/// Built from the CLI in `main.rs`; passed to [`run`] either
/// directly (from `main`) or by integration tests under
/// `tests/`. Field order tracks the §3.7 flag list.
#[derive(Debug, Clone)]
pub struct BenchConfig {
    /// Directory of `*.txt` corpus files (RFC 0006 §3.3).
    pub corpus_dir: PathBuf,
    /// Directory the §3.6 results JSON lands in.
    pub results_dir: PathBuf,
    /// Parquet writer's `bucket_root`. `None` means
    /// "create a fresh temp dir and clean it up on exit".
    pub bucket_dir: Option<PathBuf>,
    /// If `true`, the temp bucket dir survives the run (debug
    /// inspection); otherwise it's removed in [`run`]'s cleanup
    /// path.
    pub keep_parquet: bool,
    /// §3.5 hardware-kind annotation. `None` means
    /// `--allow-unknown-hardware` was passed.
    pub hardware_kind: Option<String>,
    /// Append / rewrite the §9 sub-heading in
    /// `docs/benchmarks.md`. Off by default; CI runs without
    /// it, maintainers opt in to commit numbers.
    pub update_benchmarks_md: bool,
    /// Which gates to compute. Empty set is rejected by CLI
    /// parsing.
    pub gates: GateSet,
}

/// Subset of {A1, C1, C2} selected via `--gates`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateSet {
    pub a1: bool,
    pub c1: bool,
    pub c2: bool,
}

impl GateSet {
    /// All three gates enabled — the default when `--gates` is
    /// omitted.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            a1: true,
            c1: true,
            c2: true,
        }
    }
}

/// Top-level entry point. Loads the corpus, drives the miner
/// and writer pipeline, computes the §3.4 measurements for
/// every enabled gate, writes the §3.6 results JSON, and
/// optionally rewrites the `docs/benchmarks.md` §9 sub-heading
/// per `config.update_benchmarks_md`.
///
/// # Errors
///
/// Returns [`BenchError`] for any failure along the path. The
/// scaffold stage returns [`BenchError::NotImplemented`] from
/// every entry point — the Red-gate tests in
/// `tests/{a1,c1,c2,reproducibility}.rs` are `#[ignore]`'d
/// against this exact error.
pub fn run(_config: &BenchConfig) -> Result<ResultsFile, BenchError> {
    Err(BenchError::NotImplemented {
        what: "ourios_bench::run has no measurement code yet",
    })
}

/// §3.6 JSON results file. Field order mirrors the RFC.
///
/// The struct is `serde::Serialize` + `serde::Deserialize` so
/// downstream analysis tooling can parse it without re-reading
/// the RFC; `rfc_version = "v1"` is the stability tag and
/// bumping it requires an amendment.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResultsFile {
    /// Always `"RFC 0006"`.
    pub rfc: String,
    /// Pinned by §3.6 to `"v1"` until the RFC is amended.
    pub rfc_version: String,
    /// RFC3339 with millisecond precision (§3.6 collision-avoidance rule).
    pub timestamp: String,
    /// Seven-character abbreviated git SHA at run time.
    pub git_sha: String,
    /// §3.5 hardware-kind annotation. `"unknown"` is allowed
    /// only when `--allow-unknown-hardware` was passed.
    pub hardware_kind: String,
    pub corpus: CorpusStats,
    pub ourios: OuriosStats,
    pub zstd: ZstdStats,
    /// `null` when A1 was skipped via `--gates` (§3.6 nullability rule).
    pub a1: Option<A1Result>,
    /// `null` when C1 was skipped via `--gates`.
    pub c1: Option<C1Result>,
    /// `null` when C2 was skipped via `--gates`, or when the
    /// corpus is `< 1 M lines` (§3.4.3 abstention).
    pub c2: Option<C2Result>,
}

/// §3.6 `corpus` block.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CorpusStats {
    pub directory: String,
    pub total_lines: u64,
    pub total_files: u32,
    pub raw_bytes: u64,
}

/// §3.6 `ourios` block. `total = data + audit`; A1 operates on
/// the total per §3.4.1.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OuriosStats {
    pub data_parquet_bytes: u64,
    pub audit_parquet_bytes: u64,
    pub total_parquet_bytes: u64,
}

/// §3.6 `zstd` block. `level` is pinned to 19 by §3.4.1.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ZstdStats {
    pub level: u8,
    pub compressed_bytes: u64,
}

/// §3.6 `a1` block (populated only when A1 ran).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct A1Result {
    pub ourios_ratio: f64,
    pub zstd_ratio: f64,
    pub delta: f64,
    pub target_delta: f64,
    pub pass: bool,
}

/// §3.6 `c1` block (populated only when C1 ran).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct C1Result {
    pub non_lossy_total: u64,
    pub non_lossy_reconstruct_ok: u64,
    pub rate: f64,
    pub lossy_flag_ratio: f64,
    pub pass: bool,
}

/// §3.6 `c2` block (populated only when C2 ran). `pass` is
/// `None` when the corpus is `< 1 M lines` (§3.4.3 abstention).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct C2Result {
    pub sample_cadence: u64,
    pub total_lines: u64,
    pub template_count_at_1m_lines: Option<u64>,
    pub template_count_at_end: u64,
    pub convergence_ratio: Option<f64>,
    pub convergence_curve: Vec<ConvergenceSample>,
    pub pass: Option<bool>,
    pub corpus_at_least_1m: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConvergenceSample {
    pub lines: u64,
    pub template_count: u64,
}

/// Errors produced by [`run`] and the §3.4 measurement helpers.
///
/// The Red-gate scaffold only ever returns
/// [`Self::NotImplemented`]; the other variants are the wire
/// shapes the eventual implementation will use, declared now so
/// downstream test code doesn't have to refactor when they start
/// firing. Marked `#[non_exhaustive]` so adding measurement-
/// surface variants (e.g. a future `Zstd { detail }` once the
/// §7 ZSTD-integration code lands) isn't a breaking change for
/// downstream `match` arms — mirrors the
/// `crates/ourios-miner/src/tokenize.rs::TokenizeError`
/// precedent.
#[derive(Debug)]
#[non_exhaustive]
pub enum BenchError {
    /// Red-gate placeholder.
    NotImplemented { what: &'static str },
    /// A required CLI argument was missing or malformed.
    Cli { detail: String },
    /// Corpus directory missing / empty / non-readable.
    Corpus { detail: String },
    /// Miner / writer / reader returned an error during ingest.
    Pipeline { detail: String },
    /// JSON serialisation, file write, or `benchmarks.md`
    /// rewrite failed.
    Report { detail: String },
}

impl fmt::Display for BenchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotImplemented { what } => write!(f, "RFC 0006 Red-gate scaffold: {what}"),
            Self::Cli { detail } => write!(f, "CLI: {detail}"),
            Self::Corpus { detail } => write!(f, "corpus: {detail}"),
            Self::Pipeline { detail } => write!(f, "pipeline: {detail}"),
            Self::Report { detail } => write!(f, "report: {detail}"),
        }
    }
}

impl std::error::Error for BenchError {}

#[cfg(test)]
mod tests {
    //! Colocated unit tests pinning the contracts that are
    //! testable today: the `GateSet::all()` API shape, the
    //! Red-stage `run()` return value, and the §3.6
    //! `ResultsFile` serde round-trip. The integration tests in
    //! `tests/{a1,c1,c2,reproducibility}.rs` exercise the
    //! end-to-end pipeline; these unit tests pin the surface
    //! contracts independently so a future refactor that
    //! breaks them surfaces before the (slower) integration
    //! suite runs.
    use super::*;

    /// `GateSet::all()` is the default the CLI uses when
    /// `--gates` is omitted (RFC 0006 §3.7). Pinning the
    /// constructor against the literal struct catches an
    /// accidental change (e.g. a `c2: false` default) that
    /// would silently drop a thesis gate.
    #[test]
    fn gate_set_all_enables_a1_c1_and_c2() {
        assert_eq!(
            GateSet::all(),
            GateSet {
                a1: true,
                c1: true,
                c2: true,
            },
        );
    }

    /// Red-stage scaffold marker: every `run()` call returns
    /// `BenchError::NotImplemented`. This test exists to flip
    /// red the moment a future PR removes the placeholder and
    /// starts returning a `ResultsFile`; the implementation PR
    /// that lands the harness deletes (or rewrites) this test
    /// rather than weakening it.
    #[test]
    fn run_returns_not_implemented_in_red_stage() {
        let config = BenchConfig {
            corpus_dir: std::path::PathBuf::from("."),
            results_dir: std::path::PathBuf::from("."),
            bucket_dir: None,
            keep_parquet: false,
            hardware_kind: None,
            update_benchmarks_md: false,
            gates: GateSet::all(),
        };
        let err = run(&config).expect_err("red-stage scaffold must return NotImplemented");
        assert!(
            matches!(err, BenchError::NotImplemented { .. }),
            "expected NotImplemented, got {err:?}",
        );
    }

    /// `ResultsFile` is the §3.6 contract incarnated; its serde
    /// shape is what downstream analysis tooling parses. Pin a
    /// minimal round-trip so a stray field rename or omission
    /// fails this test before drift reaches a real `.json`
    /// file on disk. Field values are deliberately sentinel
    /// (`"sentinel"` / `0` / empty vec) — the shape is the
    /// contract, not the values.
    #[test]
    fn results_file_roundtrips_through_serde_json() {
        let original = ResultsFile {
            rfc: "RFC 0006".to_string(),
            rfc_version: "v1".to_string(),
            timestamp: "2026-05-25T00:00:00.000Z".to_string(),
            git_sha: "abc1234".to_string(),
            hardware_kind: "sentinel".to_string(),
            corpus: CorpusStats {
                directory: "sentinel".to_string(),
                total_lines: 0,
                total_files: 0,
                raw_bytes: 0,
            },
            ourios: OuriosStats {
                data_parquet_bytes: 0,
                audit_parquet_bytes: 0,
                total_parquet_bytes: 0,
            },
            zstd: ZstdStats {
                level: 19,
                compressed_bytes: 0,
            },
            a1: None,
            c1: None,
            c2: None,
        };

        let json = serde_json::to_string(&original).expect("serialise");
        let parsed: ResultsFile = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed.rfc, original.rfc);
        assert_eq!(parsed.rfc_version, "v1");
        assert_eq!(parsed.zstd.level, 19);
        assert!(parsed.a1.is_none(), "skipped-gate nullability holds");
        assert!(parsed.c1.is_none());
        assert!(parsed.c2.is_none());
    }
}
