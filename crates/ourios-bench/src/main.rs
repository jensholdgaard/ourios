//! `ourios-bench` binary entry point.
//!
//! Once implemented, this binary will be a thin wrapper that
//! parses CLI arguments into a [`ourios_bench::BenchConfig`]
//! and calls [`ourios_bench::run`]. The CLI surface is pinned
//! by RFC 0006 §3.7. **Today this file is the Red-gate
//! scaffold**: `main()` prints a banner to stderr and exits
//! non-zero without touching the library. Argument parsing
//! and the call into `run` land in the PR-H2 follow-up
//! together with the test stubs that exercise them.

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
