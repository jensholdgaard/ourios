//! `ourios-bench` binary entry point.
//!
//! Thin wrapper that parses CLI arguments into a
//! [`ourios_bench::BenchConfig`] and calls
//! [`ourios_bench::run`]. The CLI surface is pinned by
//! RFC 0006 §3.7; this file is the Red-gate scaffold and the
//! argument parser is still a stub.

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!(
        "ourios-bench: RFC 0006 Red-gate scaffold — argument parser and harness are \
         not implemented yet. Track progress on the maturity-model bump in \
         `docs/rfcs/0006-bench-harness.md` §7.",
    );
    // Exit non-zero so a `just thesis-bench` invocation in the
    // scaffold window can't be mistaken for a successful
    // benchmark run.
    ExitCode::from(2)
}
