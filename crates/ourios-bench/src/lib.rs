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
//! incrementally. PR-I1 extracts `corpus`, `harness`, and
//! `c1` to bring the C1 gate into the green column;
//! `a1` / `c2` / `report` remain collapsed (and unwritten)
//! until their respective implementation PRs.

#![deny(unsafe_code)]

use std::fmt;
use std::path::PathBuf;

mod c1;
mod corpus;
mod harness;

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
/// every enabled gate, and returns the §3.6 results in
/// memory; writing the JSON file to `config.results_dir` and
/// optionally rewriting the `docs/benchmarks.md` §9 sub-heading
/// per `config.update_benchmarks_md` are the binary's
/// responsibility (and not yet implemented).
///
/// **Implemented gates**: C1 (PR-I1). A1 and C2 still return
/// [`BenchError::NotImplemented`] — picking them via
/// `config.gates` fails the call. At least one gate must be
/// enabled; an empty `GateSet` returns [`BenchError::Cli`].
///
/// # Errors
///
/// - [`BenchError::NotImplemented`] when `config.gates.a1` or
///   `config.gates.c2` is set (their implementations land in
///   follow-up PRs).
/// - [`BenchError::Cli`] when no gates are enabled.
/// - [`BenchError::Corpus`] when the corpus directory is
///   unreadable, missing, or contributes no non-empty lines.
/// - [`BenchError::Pipeline`] when the miner's emit count
///   diverges from the input line count (RFC 0001 §6.1
///   one-record-per-line violation).
///
/// # Panics
///
/// Panics on a `usize → u64` conversion failure for
/// `harness_output.lines.len()`. The bound is documented
/// inline: `usize ≤ u64` holds on every Rust Tier 1 / 2
/// target, so this only fires on a hypothetical 128-bit
/// platform — at which point the panic is exactly the
/// "surface a real logic bug" behaviour the §3.6 results
/// shape needs.
pub fn run(config: &BenchConfig) -> Result<ResultsFile, BenchError> {
    if config.gates.a1 {
        return Err(BenchError::NotImplemented {
            what: "A1 measurement (lands with the next bench implementation PR)",
        });
    }
    if config.gates.c2 {
        return Err(BenchError::NotImplemented {
            what: "C2 measurement (lands with the next bench implementation PR)",
        });
    }
    if !config.gates.c1 {
        return Err(BenchError::Cli {
            detail: "no gates enabled; --gates must include at least one of a1, c1, c2".to_string(),
        });
    }

    // C1 path. Doesn't write Parquet (A1 isn't enabled), so
    // the `ourios.*` byte counts come out zero in this run's
    // results JSON. That's the §3.6 nullability rule applied
    // to the *gate* fields (`a1: None`), with the per-section
    // byte counters left as zero rather than wrapped in
    // `Option` — the schema pins them as required `u64` so
    // an A1-skipped run still serialises against the same
    // shape.
    let corpus_load = corpus::load(&config.corpus_dir)?;
    let directory = corpus_load.directory.clone();
    let total_files = corpus_load.total_files;
    let raw_bytes = corpus_load.raw_bytes;
    let harness_output = harness::run(corpus_load)?;
    // `usize → u64` is infallible on every Rust Tier 1 / 2
    // target (`usize ≤ u64`). The earlier
    // `.unwrap_or(u64::MAX)` formulation would silently bury
    // a real logic bug on a future 128-bit target rather
    // than surfacing it; `expect` names the assumption.
    let total_lines = u64::try_from(harness_output.lines.len())
        .expect("usize fits in u64 on every supported Rust target");
    let c1_result = c1::compute(&harness_output);

    Ok(ResultsFile {
        rfc: "RFC 0006".to_string(),
        rfc_version: "v1".to_string(),
        timestamp: timestamp_now(),
        git_sha: git_sha_short(),
        hardware_kind: config
            .hardware_kind
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        corpus: CorpusStats {
            directory,
            total_lines,
            total_files,
            raw_bytes,
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
        c1: Some(c1_result),
        c2: None,
    })
}

/// Format `SystemTime::now()` as a §3.6 millisecond-precision
/// RFC3339 string (`YYYY-MM-DDTHH:MM:SS.mmmZ`). Computed
/// inline (no `chrono` dep) because the bench only needs
/// emission, not parsing or arithmetic.
///
/// The casts in this function are bounded by physical time:
/// `total_seconds` is `u64` but only the low ~31 bits are
/// used in any realistic invocation (years in `[1970, 9999]`
/// fit easily); `seconds_of_day < 86_400` always fits in
/// `u32`. The `#[allow]`s below name the bounds explicitly
/// rather than introducing `TryFrom` plumbing for casts that
/// can't actually fail.
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn timestamp_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("wall clock before UNIX_EPOCH");
    let total_seconds = dur.as_secs();
    let millis = dur.subsec_millis();

    // Civil-time conversion (UTC). The algorithm comes from
    // Howard Hinnant's "date" paper; same shape as
    // `chrono::DateTime::<Utc>::from_timestamp` but inlined to
    // skip the dep.
    let days = (total_seconds / 86_400) as i64;
    let seconds_of_day = (total_seconds % 86_400) as u32;
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    let (year, month, day) = civil_from_days(days);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Days-since-1970-01-01 → (year, month, day) via Howard
/// Hinnant's algorithm. Returns Gregorian calendar
/// year / month / day for any signed-64-bit `days`.
///
/// The casts are bounded by the algorithm: `doe` is always
/// in `[0, 146_096]`, `yoe` in `[0, 399]`, `y + era * 400` is
/// bounded by the input `days` (so a wall-clock-now input
/// produces a year in `[1970, …]`, which fits comfortably in
/// `i32`). The `#[allow]`s name the bounds rather than
/// introducing fallible conversions.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::unnecessary_cast
)]
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    // Shift the epoch to 0000-03-01 so February's variable
    // length lands at the end of the year.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = i64::from(yoe) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (y + i64::from(m <= 2)) as i32;
    (year, m, d)
}

/// Resolve the short (7-character) git SHA via
/// `git rev-parse --short=7 HEAD`. Falls back to `"unknown"`
/// when the bench isn't running from a git checkout (or git
/// isn't on PATH). The §3.5 hardware-kind annotation has the
/// same "explicit-unknown rather than silent-default" shape;
/// this mirrors it for the git SHA.
fn git_sha_short() -> String {
    use std::process::Command;
    Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| s.len() == 7)
        .unwrap_or_else(|| "unknown".to_string())
}

/// §3.6 JSON results file. Field order mirrors the RFC.
///
/// The struct is `serde::Serialize` + `serde::Deserialize` so
/// downstream analysis tooling can parse it without re-reading
/// the RFC; `rfc_version = "v1"` is the stability tag and
/// bumping it requires an amendment.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CorpusStats {
    pub directory: String,
    pub total_lines: u64,
    pub total_files: u32,
    pub raw_bytes: u64,
}

/// §3.6 `ourios` block. `total = data + audit`; A1 operates on
/// the total per §3.4.1.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OuriosStats {
    pub data_parquet_bytes: u64,
    pub audit_parquet_bytes: u64,
    pub total_parquet_bytes: u64,
}

/// §3.6 `zstd` block. `level` is pinned to 19 by §3.4.1.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ZstdStats {
    pub level: u8,
    pub compressed_bytes: u64,
}

/// §3.6 `a1` block (populated only when A1 ran).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct A1Result {
    pub ourios_ratio: f64,
    pub zstd_ratio: f64,
    pub delta: f64,
    pub target_delta: f64,
    pub pass: bool,
}

/// §3.6 `c1` block (populated only when C1 ran).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct C1Result {
    pub non_lossy_total: u64,
    pub non_lossy_reconstruct_ok: u64,
    pub rate: f64,
    pub lossy_flag_ratio: f64,
    pub pass: bool,
}

/// §3.6 `c2` block (populated only when C2 ran). `pass` is
/// `None` when the corpus is `< 1 M lines` (§3.4.3 abstention).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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

    /// PR-I1 marker — A1 and C2 still return
    /// `BenchError::NotImplemented` from `run()`, even though
    /// C1 now produces a real `ResultsFile`. This test pins
    /// the partial-implementation gate: a future PR that
    /// lands A1 (or C2) makes this assertion fail by
    /// returning `Ok(...)`, which is the maturity-model
    /// marker that the corresponding gate has graduated. The
    /// implementation PR rewrites this test alongside its
    /// code change.
    #[test]
    fn a1_and_c2_still_return_not_implemented() {
        for gates in [
            GateSet {
                a1: true,
                c1: false,
                c2: false,
            },
            GateSet {
                a1: false,
                c1: false,
                c2: true,
            },
        ] {
            let config = BenchConfig {
                corpus_dir: std::path::PathBuf::from("."),
                results_dir: std::path::PathBuf::from("."),
                bucket_dir: None,
                keep_parquet: false,
                hardware_kind: None,
                update_benchmarks_md: false,
                gates,
            };
            let err = run(&config).expect_err("A1 / C2 not yet implemented");
            assert!(
                matches!(err, BenchError::NotImplemented { .. }),
                "expected NotImplemented, got {err:?}",
            );
        }
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
