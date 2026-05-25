# Ourios developer command runner.
#
# Run `just` (no args) to see all available recipes.
# `just check` is the one-command pre-merge gate that mirrors
# `CLAUDE.md` §6.6 ("Forced verification before done").
#
# Install on macOS: `brew install just`
# Install elsewhere: https://github.com/casey/just#packages

# Default: list available recipes.
default:
    @just --list

# Run the full §6.6 verification suite. Bails on first failure.
check: fmt-check clippy test book
    @echo "All checks passed."

# Format check (CI-style; doesn't modify files).
fmt-check:
    cargo fmt --all --check

# Format in place. Use during local dev.
fmt:
    cargo fmt --all

# Run clippy with the project's lint level (warnings as errors).
clippy:
    cargo clippy --all-targets --all-features -- -D warnings

# Run the test suite.
test:
    cargo test --all-features

# Build the mdBook documentation. Output: book/.
book:
    mdbook build

# Serve the mdBook with live reload at http://localhost:3000.
book-serve:
    mdbook serve

# Run criterion benchmarks. No-op until benches exist.
bench:
    cargo bench

# Run the RFC 0006 thesis-gate bench harness (A1 / C1 / C2).
# Always release mode — RFC 0006 §3.7 pins `--release` as
# normative because debug-mode codec output understates A1.
# Implementation is in-flight; see `crates/ourios-bench/`.
thesis-bench *ARGS:
    cargo run -p ourios-bench --release -- {{ARGS}}

# Lint commit message (requires `committed`: cargo install committed).
lint-commits:
    committed --commit-file .git/COMMIT_EDITMSG

# Clean build artefacts (cargo target + mdBook output).
clean:
    cargo clean || true
    rm -rf book
