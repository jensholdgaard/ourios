//! `ourios-bench` binary entry point.
//!
//! Parses the RFC 0006 §3.7 flag surface into a
//! [`ourios_bench::BenchConfig`], drives
//! [`ourios_bench::run`], writes the §3.6 JSON results file,
//! prints a human summary, and maps the outcome to a process
//! exit code (a C1 reconstruction mismatch is a hard failure
//! per §3.4.2).
//!
//! The `--update-benchmarks-md` §9 markdown appender is not
//! implemented yet — the flag is accepted (so the surface
//! matches §3.7) but currently only warns; the JSON results
//! file is written regardless.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use ourios_bench::{BenchConfig, BenchError, GateSet, run, write_results_json};

/// RFC 0006 §1 hardware baseline tag, surfaced in the
/// `--allow-unknown-hardware` warning so an operator knows
/// what to run on for comparable numbers.
const BASELINE_HARDWARE_TAG: &str = "baseline-8vcpu-32gib (8 vCPU / 32 GiB / gp3-class SSD)";

/// RFC 0006 §3.7 thesis-gate bench harness CLI.
#[derive(Parser, Debug)]
#[command(
    name = "ourios-bench",
    about = "RFC 0006 thesis-gate bench harness (A1 compression / C1 reconstruction / C2 convergence)"
)]
struct Cli {
    /// Directory of `*.txt` corpus files to load.
    #[arg(long, default_value = "testdata/corpus")]
    corpus: PathBuf,
    /// Where the §3.6 JSON results file lands.
    #[arg(long, default_value = "benchmarks/results")]
    results_dir: PathBuf,
    /// Parquet writer `bucket_root`. Defaults to a fresh temp
    /// dir, cleaned up on exit unless `--keep-parquet`.
    #[arg(long)]
    bucket_dir: Option<PathBuf>,
    /// Keep the Parquet output for inspection. Requires
    /// `--bucket-dir` (a scratch dir's path isn't reported, so
    /// keeping it would be unfindable).
    #[arg(long, requires = "bucket_dir")]
    keep_parquet: bool,
    /// §3.5 hardware-kind annotation. Required unless
    /// `--allow-unknown-hardware`.
    #[arg(long, required_unless_present = "allow_unknown_hardware")]
    hardware_kind: Option<String>,
    /// Tag the results `hardware_kind = "unknown"` instead of
    /// requiring `--hardware-kind`.
    #[arg(long)]
    allow_unknown_hardware: bool,
    /// Append / rewrite the `docs/benchmarks.md` §9 sub-heading
    /// (not implemented yet — see crate docs).
    #[arg(long)]
    update_benchmarks_md: bool,
    /// Comma-separated subset of gates to compute. Default: all.
    #[arg(long, value_enum, value_delimiter = ',')]
    gates: Vec<Gate>,
}

/// One thesis gate, as named on the `--gates` flag.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Gate {
    #[value(name = "a1")]
    A1,
    #[value(name = "c1")]
    C1,
    #[value(name = "c2")]
    C2,
}

impl Cli {
    /// Collapse the `--gates` list into a [`GateSet`]. An empty
    /// list (flag omitted) means all gates, per §3.7.
    fn gate_set(&self) -> GateSet {
        if self.gates.is_empty() {
            return GateSet::all();
        }
        GateSet {
            a1: self.gates.contains(&Gate::A1),
            c1: self.gates.contains(&Gate::C1),
            c2: self.gates.contains(&Gate::C2),
        }
    }

    fn into_config(self) -> BenchConfig {
        let gates = self.gate_set();
        BenchConfig {
            corpus_dir: self.corpus,
            results_dir: self.results_dir,
            bucket_dir: self.bucket_dir,
            keep_parquet: self.keep_parquet,
            // `None` here ⇒ `run` tags `hardware_kind =
            // "unknown"`; reachable only with
            // `--allow-unknown-hardware` (clap enforces the
            // flag otherwise).
            hardware_kind: self.hardware_kind,
            update_benchmarks_md: self.update_benchmarks_md,
            gates,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run_bench(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ourios-bench: {e}");
            ExitCode::from(2)
        }
    }
}

fn run_bench(cli: Cli) -> Result<ExitCode, BenchError> {
    if cli.hardware_kind.is_none() {
        // Reachable only via --allow-unknown-hardware (clap
        // requires one of the two). Name the §1 baseline so an
        // operator knows what to run on for comparable numbers.
        eprintln!(
            "ourios-bench: warning: --allow-unknown-hardware set; results are tagged \
             hardware_kind=\"unknown\". For numbers comparable to the thesis gates, run on the \
             §1 baseline ({BASELINE_HARDWARE_TAG}) and pass --hardware-kind.",
        );
    }
    let keep_parquet_path = cli.keep_parquet.then(|| cli.bucket_dir.clone()).flatten();
    let update_md = cli.update_benchmarks_md;
    let results_dir = cli.results_dir.clone();

    let config = cli.into_config();
    let results = run(&config)?;
    let path = write_results_json(&results, &results_dir)?;
    eprintln!("ourios-bench: results written to {}", path.display());

    if let Some(bucket) = keep_parquet_path {
        eprintln!(
            "ourios-bench: --keep-parquet set; Parquet output retained at {}",
            bucket.display(),
        );
    }
    print_summary(&results);
    if update_md {
        eprintln!(
            "ourios-bench: warning: --update-benchmarks-md is not implemented yet (the §9 \
             markdown appender lands in a follow-up PR); JSON results written only.",
        );
    }

    // §3.4.2: a non-lossy reconstruction mismatch is a
    // correctness failure, not just a degraded number — exit
    // non-zero so CI / a `/bench` run surfaces it. A1 / C2
    // gate outcomes are *reported* (in the JSON + summary) but
    // don't fail the process; whether a missed compression
    // target pauses the project is the §7 escalation rule's
    // human judgment, not a build red.
    if let Some(c1) = &results.c1 {
        if !c1.pass {
            let failed = c1.non_lossy_total - c1.non_lossy_reconstruct_ok;
            eprintln!(
                "ourios-bench: C1 FAILED — {failed} of {} non-lossy row(s) did not reconstruct \
                 byte-for-byte (RFC 0006 §3.4.2 / CLAUDE.md §3.3)",
                c1.non_lossy_total,
            );
            return Ok(ExitCode::from(1));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Print a one-line-per-gate human summary to stdout. The
/// machine-readable form is the JSON results file; this is
/// just operator feedback.
fn print_summary(results: &ourios_bench::ResultsFile) {
    println!(
        "corpus {} — {} line(s), {} file(s), {} raw byte(s) [{}]",
        results.corpus.directory,
        results.corpus.total_lines,
        results.corpus.total_files,
        results.corpus.raw_bytes,
        results.hardware_kind,
    );
    if let Some(a1) = &results.a1 {
        println!(
            "  A1 compression: ourios {:.3}× vs zstd-19 {:.3}× → delta {:.3}× (target ≥ {:.1}×) — {}",
            a1.ourios_ratio,
            a1.zstd_ratio,
            a1.delta,
            a1.target_delta,
            if a1.pass { "PASS" } else { "FAIL" },
        );
    }
    if let Some(c1) = &results.c1 {
        println!(
            "  C1 reconstruction: {:.6} ({}/{} non-lossy rows; lossy ratio {:.4}) — {}",
            c1.rate,
            c1.non_lossy_reconstruct_ok,
            c1.non_lossy_total,
            c1.lossy_flag_ratio,
            if c1.pass { "PASS" } else { "FAIL" },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC0006.5 — `--hardware-kind` is required unless
    /// `--allow-unknown-hardware`. clap rejects the bare
    /// invocation at parse time, before any measurement runs.
    #[test]
    fn hardware_kind_required_unless_allow_unknown() {
        let bare = Cli::try_parse_from(["ourios-bench"]);
        assert!(
            bare.is_err(),
            "missing --hardware-kind must be a usage error"
        );

        let allowed = Cli::try_parse_from(["ourios-bench", "--allow-unknown-hardware"])
            .expect("--allow-unknown-hardware satisfies the requirement");
        assert!(
            allowed.hardware_kind.is_none(),
            "hardware_kind stays None under --allow-unknown-hardware (run tags it \"unknown\")",
        );

        let explicit =
            Cli::try_parse_from(["ourios-bench", "--hardware-kind", "baseline-8vcpu-32gib"])
                .expect("explicit --hardware-kind parses");
        assert_eq!(
            explicit.hardware_kind.as_deref(),
            Some("baseline-8vcpu-32gib")
        );
    }

    /// RFC0006.6 — `--gates` scopes the measurement; omitting
    /// it means all three.
    #[test]
    fn gates_flag_scopes_the_measurement() {
        let all = Cli::try_parse_from(["ourios-bench", "--allow-unknown-hardware"])
            .expect("parse")
            .gate_set();
        assert_eq!(all, GateSet::all(), "default is all gates");

        let c1_only =
            Cli::try_parse_from(["ourios-bench", "--allow-unknown-hardware", "--gates", "c1"])
                .expect("parse")
                .gate_set();
        assert_eq!(
            c1_only,
            GateSet {
                a1: false,
                c1: true,
                c2: false,
            },
            "--gates c1 selects only C1",
        );

        let a1_c2 = Cli::try_parse_from([
            "ourios-bench",
            "--allow-unknown-hardware",
            "--gates",
            "a1,c2",
        ])
        .expect("parse")
        .gate_set();
        assert_eq!(
            a1_c2,
            GateSet {
                a1: true,
                c1: false,
                c2: true,
            },
            "--gates a1,c2 selects A1 and C2 (comma-separated)",
        );
    }

    /// `--keep-parquet` requires `--bucket-dir` at the clap
    /// layer (a scratch dir's path isn't reported). Pins the
    /// early rejection so the friendlier `requires` message
    /// fires before `run`'s internal backstop.
    #[test]
    fn keep_parquet_requires_bucket_dir() {
        let err =
            Cli::try_parse_from(["ourios-bench", "--allow-unknown-hardware", "--keep-parquet"]);
        assert!(
            err.is_err(),
            "--keep-parquet without --bucket-dir is a usage error"
        );
    }
}
