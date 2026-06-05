//! `ourios-bench` — RFC 0006 thesis-gate bench harness
//! (A1 compression, C1 reconstruction, C2 template-count
//! convergence).
//!
//! **Implementation status (PR-J4 — RFC 0006 `green`):** all
//! three writer-side gates — A1 (compression), C1
//! (reconstruction), C2 (template-count convergence) — are
//! live. [`run`] computes any combination in a single miner
//! pass and returns a populated [`ResultsFile`]. The CLI
//! (RFC 0006 §3.7) in `main.rs` drives `run`, writes the §3.6
//! JSON results file via [`write_results_json`], and — with
//! `--update-benchmarks-md` — folds the results into the
//! `docs/benchmarks.md` §9 table via [`update_status_section`].
//! Every §5 acceptance scenario has a passing test, so RFC 0006
//! is at maturity `green`. (The one heavy ≥ 1 M-line C2 test
//! is `#[ignore]`'d for the per-PR loop per §3.7 and runs
//! on-demand via `cargo test -- --ignored`; its convergence
//! math is covered by default by the colocated `c2` unit
//! tests.) `Validated` follows once the gates are measured on
//! a real corpus + the §1 hardware baseline.
//!
//! Per RFC 0006 §3.2 the module layout is `corpus`, `harness`,
//! `a1`, `c1`, `c2`, `report`. PR-I1 extracted `corpus`,
//! `harness`, `c1`; PR-I2 added `a1`; PR-J1 added `report`
//! (JSON half) + the CLI; PR-J2 added `c2`; PR-J3 added the
//! §9 appender.

#![deny(unsafe_code)]

use std::fmt;
use std::path::PathBuf;

mod a1;
mod c1;
mod c2;
mod corpus;
mod harness;
mod reference;
mod report;
mod store;

pub use reference::ReferenceCorpus;
pub use report::{update_status_section, write_results_json};
pub use store::{BuiltStore, build_query_store};

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
    /// Parquet data-writer ZSTD level for A1. Defaults to
    /// `ourios_parquet::DEFAULT_ZSTD_LEVEL` (the production
    /// codec); raised via `--parquet-zstd-level` to sweep the A1
    /// space/CPU tradeoff against the §3.4.1 ZSTD-19 baseline.
    pub parquet_zstd_level: i32,
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
/// memory. Persisting them — writing the JSON file to
/// `config.results_dir` and, when `config.update_benchmarks_md`
/// is set, folding them into the `docs/benchmarks.md` §9 table
/// — is the binary's responsibility, via
/// [`write_results_json`] and [`update_status_section`]
/// respectively.
///
/// All three gates — A1, C1, C2 — are implemented and run in
/// any combination. At least one gate must be enabled; an
/// empty `GateSet` returns [`BenchError::Cli`].
///
/// The gates share a single miner pass: the harness streams
/// each emitted record to whichever accumulators are active
/// (A1 writes it to its partition's Parquet file, C1 checks
/// its reconstruction, C2 counts distinct templates), so
/// requesting several does not multiply the ingest cost.
///
/// # Errors
///
/// - [`BenchError::Cli`] when no gates are enabled.
/// - [`BenchError::Corpus`] when the corpus directory is
///   unreadable, missing, or contributes no non-empty lines.
/// - [`BenchError::Pipeline`] when the miner's emit count
///   diverges from the input line count (RFC 0001 §6.1
///   one-record-per-line violation), when the Parquet writer
///   fails, or when the host clock is set before the Unix
///   epoch (so `timestamp_now` can't produce a §3.6-shaped
///   timestamp).
///
/// # Panics
///
/// Panics on a `usize → u64` conversion failure for
/// `corpus_load.lines.len()`. The bound is documented
/// inline: `usize ≤ u64` holds on every Rust Tier 1 / 2
/// target, so this only fires on a hypothetical 128-bit
/// platform — at which point the panic is exactly the
/// "surface a real logic bug" behaviour the §3.6 results
/// shape needs.
pub fn run(config: &BenchConfig) -> Result<ResultsFile, BenchError> {
    if !config.gates.a1 && !config.gates.c1 && !config.gates.c2 {
        return Err(BenchError::Cli {
            detail: "no gates enabled; --gates must include at least one of a1, c1, c2".to_string(),
        });
    }

    let corpus_load = corpus::load(&config.corpus_dir)?;
    let directory = corpus_load.directory.clone();
    let total_files = corpus_load.total_files;
    let raw_bytes = corpus_load.raw_bytes;
    // `usize → u64` is infallible on every Rust Tier 1 / 2
    // target (`usize ≤ u64`). The earlier
    // `.unwrap_or(u64::MAX)` formulation would silently bury
    // a real logic bug on a future 128-bit target rather
    // than surfacing it; `expect` names the assumption.
    let total_lines = u64::try_from(corpus_load.lines.len())
        .expect("usize fits in u64 on every supported Rust target");

    // Resolve the Parquet output bucket — but only for A1.
    // C1 writes no Parquet, so a C1-only run needs no bucket
    // and must not be tripped up by bucket concerns (e.g. a
    // `--keep-parquet` flag, or an unwritable temp dir). The
    // `_bucket_guard` keeps a scratch `TempDir` alive until
    // end of scope; it's `None` for a caller-supplied dir or
    // when A1 is disabled.
    let _bucket_guard: Option<tempfile::TempDir>;
    let mut a1_acc = if config.gates.a1 {
        let (bucket_root, guard) = resolve_bucket(config)?;
        // A1 measures every `*.parquet` under the bucket; a
        // caller-supplied dir already holding Parquet (e.g. a
        // prior `--keep-parquet` run) would inflate
        // `bytes(ourios_output)`. A scratch `TempDir` is always
        // empty, so this only ever fires on a reused caller dir.
        ensure_bucket_has_no_parquet(&bucket_root)?;
        _bucket_guard = guard;
        Some(a1::A1Accumulator::new(
            &bucket_root,
            config.parquet_zstd_level,
        ))
    } else {
        _bucket_guard = None;
        None
    };

    let mut c1_acc = config.gates.c1.then(c1::C1Accumulator::new);
    let mut c2_acc = config.gates.c2.then(|| c2::C2Accumulator::new(total_lines));

    // Capture the audit stream only when A1 needs it
    // (`SharedAuditSink` is unbounded — buffering it on a
    // C1/C2-only run would retain the full event stream for
    // nothing).
    let harness_result = harness::run(&corpus_load, config.gates.a1, |input, emitted, snap| {
        if let Some(acc) = c1_acc.as_mut() {
            acc.record(input, emitted, snap);
        }
        if let Some(acc) = a1_acc.as_mut() {
            acc.record(emitted);
        }
        if let Some(acc) = c2_acc.as_mut() {
            acc.record(emitted);
        }
    })?;

    let c1_result = c1_acc.map(|acc| acc.finalize());
    // `C2Accumulator::finalize` takes `self` by value, so the
    // method path is accepted directly (no closure needed).
    let c2_result = c2_acc.map(c2::C2Accumulator::finalize);
    let (a1_result, ourios, zstd) = match a1_acc {
        Some(mut acc) => {
            acc.write_audit(harness_result.audit_events)?;
            let a1 = acc.finalize(raw_bytes, &config.corpus_dir)?;
            let ourios = OuriosStats {
                data_parquet_bytes: a1.data_parquet_bytes,
                audit_parquet_bytes: a1.audit_parquet_bytes,
                total_parquet_bytes: a1.total_parquet_bytes,
            };
            let zstd = ZstdStats {
                level: 19,
                compressed_bytes: a1.zstd_bytes,
            };
            (Some(a1.result), ourios, zstd)
        }
        None => (
            None,
            OuriosStats {
                data_parquet_bytes: 0,
                audit_parquet_bytes: 0,
                total_parquet_bytes: 0,
            },
            ZstdStats {
                level: 19,
                compressed_bytes: 0,
            },
        ),
    };

    Ok(ResultsFile {
        rfc: "RFC 0006".to_string(),
        rfc_version: "v1".to_string(),
        timestamp: timestamp_now()?,
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
        ourios,
        zstd,
        a1: a1_result,
        c1: c1_result,
        c2: c2_result,
    })
}

/// Resolve the Parquet output bucket. Returns the directory
/// plus an optional [`tempfile::TempDir`] guard whose `Drop`
/// removes a scratch bucket at end of `run`. When the caller
/// passed an explicit `bucket_dir`, the guard is `None` (the
/// caller owns the directory's lifetime); the scratch case
/// returns `Some` so the dir is cleaned up on exit.
///
/// `keep_parquet` requires an explicit `bucket_dir`: a scratch
/// dir the caller can't locate (paths are deliberately kept
/// out of [`ResultsFile`] per §3.6) would be useless to keep
/// and would just litter the temp filesystem. Requesting
/// `keep_parquet` without `bucket_dir` returns
/// [`BenchError::Cli`].
fn resolve_bucket(
    config: &BenchConfig,
) -> Result<(PathBuf, Option<tempfile::TempDir>), BenchError> {
    if let Some(dir) = &config.bucket_dir {
        return Ok((dir.clone(), None));
    }
    if config.keep_parquet {
        return Err(BenchError::Cli {
            detail: "--keep-parquet requires --bucket-dir: a scratch bucket's path isn't \
                     reported (paths are kept out of the results JSON), so keeping it would \
                     leave an unfindable directory behind."
                .to_string(),
        });
    }
    let tmp = tempfile::TempDir::new().map_err(|e| BenchError::Pipeline {
        detail: format!("create scratch bucket dir: {e}"),
    })?;
    let path = tmp.path().to_path_buf();
    Ok((path, Some(tmp)))
}

/// Error if the bucket's `data/` or `audit/` subtree already
/// holds a `*.parquet` file. A1 sums every Parquet file under
/// the bucket, so pre-existing artifacts (from a prior
/// `--keep-parquet` run into the same directory) would inflate
/// `bytes(ourios_output)`. Missing subtrees are fine (nothing
/// to collide with).
fn ensure_bucket_has_no_parquet(bucket_root: &std::path::Path) -> Result<(), BenchError> {
    if let Some(path) = find_published_parquet(bucket_root)? {
        return Err(BenchError::Cli {
            detail: format!(
                "bucket {} already contains a Parquet file ({}); A1 would \
                 count it in bytes(ourios_output). Point --bucket-dir at an \
                 empty directory or omit it to use a fresh scratch dir.",
                bucket_root.display(),
                path.display(),
            ),
        });
    }
    Ok(())
}

/// First committed `*.parquet` under the bucket's `data/` or
/// `audit/` subtree, or `None` if the bucket holds no Parquet.
/// Shared by [`ensure_bucket_has_no_parquet`] (the A1 path) and
/// [`store::build_query_store`] (the B2 path), which each wrap it
/// in a context-appropriate error — a reused bucket skews A1's
/// byte count and mixes corpora into the B2 query alike.
pub(crate) fn find_published_parquet(
    bucket_root: &std::path::Path,
) -> Result<Option<std::path::PathBuf>, BenchError> {
    for sub in ["data", "audit"] {
        let dir = bucket_root.join(sub);
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            let entries = match std::fs::read_dir(&d) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(BenchError::Pipeline {
                        detail: format!("scan bucket {}: {e}", d.display()),
                    });
                }
            };
            for entry in entries {
                let entry = entry.map_err(|e| BenchError::Pipeline {
                    detail: format!("scan bucket entry under {}: {e}", d.display()),
                })?;
                let path = entry.path();
                // `symlink_metadata` (fallible, does NOT
                // follow symlinks) rather than `Path::is_dir`
                // (false on metadata errors). A symlinked
                // entry is skipped, not descended into: the
                // bench writes real directories, so a symlink
                // in the output bucket is unexpected, and
                // following one risks an unbounded scan loop
                // (a symlinked subdir pointing at an
                // ancestor). Skipping is safe — there are no
                // legitimate symlinked Parquet artifacts to
                // miss.
                let meta = std::fs::symlink_metadata(&path).map_err(|e| BenchError::Pipeline {
                    detail: format!("scan bucket metadata({}): {e}", path.display()),
                })?;
                if meta.file_type().is_symlink() {
                    continue;
                }
                if meta.is_dir() {
                    stack.push(path);
                } else if path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("parquet"))
                {
                    return Ok(Some(path));
                }
            }
        }
    }
    Ok(None)
}

/// Format `SystemTime::now()` as a §3.6 millisecond-precision
/// RFC3339 string (`YYYY-MM-DDTHH:MM:SS.mmmZ`). Computed
/// inline (no `chrono` dep) because the bench only needs
/// emission, not parsing or arithmetic.
///
/// Returns [`BenchError::Pipeline`] if the host clock is set
/// before the Unix epoch (`SystemTime::duration_since` would
/// otherwise propagate `SystemTimeError`). The bench
/// surfaces clock misconfiguration through the same error
/// path it uses for other infrastructure failures, rather
/// than panicking inside a `Result`-returning API.
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
fn timestamp_now() -> Result<String, BenchError> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| BenchError::Pipeline {
            detail: format!("host clock is set before the Unix epoch: {e}"),
        })?;
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

    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
    ))
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
    /// `null` only when C2 was skipped via `--gates`. On a
    /// `< 1 M-line` corpus C2 still ran and this is
    /// `Some(C2Result)` with the *gate* abstaining — the
    /// abstention shows up as `pass` /
    /// `template_count_at_1m_lines` / `convergence_ratio`
    /// being `null` inside the block (§3.4.3), not as the
    /// whole `c2` field being absent.
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
    /// Per-row diagnostics for the first few non-lossy rows
    /// that failed to reconstruct (RFC0006.2: the bench emits
    /// each failing row's `template_id` / `template_version` /
    /// expected / actual to stderr). Bounded — see
    /// `c1::MISMATCH_SAMPLE_CAP`. **`#[serde(skip)]`**: these
    /// are stderr-only diagnostics, not part of the §3.6 JSON
    /// schema, so they never appear in the results file (and
    /// don't affect the RFC0006.7 reproducibility comparison,
    /// which is over the JSON form).
    #[serde(skip)]
    pub mismatches: Vec<C1Mismatch>,
}

/// One non-lossy row that failed C1 reconstruction — the
/// stderr diagnostic payload RFC0006.2 requires. Not
/// serialised (carried on [`C1Result::mismatches`], which is
/// `#[serde(skip)]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct C1Mismatch {
    pub template_id: u64,
    pub template_version: u32,
    /// The ingested line bytes (UTF-8 lossy for display).
    pub expected: String,
    /// What `reconstruct` produced (UTF-8 lossy for display).
    pub actual: String,
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
/// [`Self::NotImplemented`] is no longer returned by any code
/// path (every gate is implemented as of PR-J2) but is
/// retained as a reserved variant for future gates / modes.
/// Marked `#[non_exhaustive]` so adding measurement-surface
/// variants isn't a breaking change for downstream `match`
/// arms — mirrors the
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

    /// An empty `GateSet` (no gate enabled) is a usage error
    /// — there's nothing to measure. This replaces the old
    /// `c2_still_returns_not_implemented` marker (C2 graduated
    /// in PR-J2, so no gate returns `NotImplemented` anymore);
    /// the gate-selection contract is still worth pinning. The
    /// guard runs before any corpus work, so the bogus
    /// `corpus_dir` is never read.
    #[test]
    fn no_gates_enabled_is_a_cli_error() {
        let config = BenchConfig {
            parquet_zstd_level: ourios_parquet::DEFAULT_ZSTD_LEVEL,
            corpus_dir: std::path::PathBuf::from("."),
            results_dir: std::path::PathBuf::from("."),
            bucket_dir: None,
            keep_parquet: false,
            hardware_kind: None,
            update_benchmarks_md: false,
            gates: GateSet {
                a1: false,
                c1: false,
                c2: false,
            },
        };
        let err = run(&config).expect_err("no gates enabled is an error");
        assert!(
            matches!(err, BenchError::Cli { .. }),
            "expected Cli error, got {err:?}",
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

    /// `civil_from_days` is the inlined Howard-Hinnant
    /// algorithm `timestamp_now` uses to format the §3.6
    /// RFC3339 string. Pin a few well-known anchors so a
    /// regression in the date conversion surfaces here
    /// rather than as a §9 result row dated 1970 by accident.
    #[test]
    fn civil_from_days_anchors() {
        // 1970-01-01 — the Unix epoch.
        assert_eq!(civil_from_days(0), (1970, 1, 1));

        // 1969-12-31 — one day before the epoch (negative
        // input branch).
        assert_eq!(civil_from_days(-1), (1969, 12, 31));

        // 2026-04-02 — the §3.3 corpus baseline timestamp's
        // date. Days from 1970-01-01 (= day 0) to
        // 2026-04-02 = 20_545. Pinning the literal catches
        // an off-by-one drift in the algorithm.
        assert_eq!(civil_from_days(20_545), (2026, 4, 2));

        // 2024-02-29 — leap day. Days from 1970-01-01 to
        // 2024-02-29 = 19_782. Pins the leap-year branch
        // of the algorithm.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));

        // 2000-03-01 — day after the famous 2000 leap day
        // (divisible by 400). Days from 1970-01-01 to
        // 2000-03-01 = 11_017. Pins the 400-year cycle.
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
    }
}
