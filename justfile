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

# Preview a release WITHOUT changing anything (no bump, no tag): the CHANGELOG.md
# git-cliff would generate for vX.Y.Z, then the artifacts cargo-dist would build.
# Requires git-cliff (`brew install git-cliff`) + dist (cargo-dist). e.g.
# `just release-dry 0.1.0`.
release-dry version:
    @echo "=== CHANGELOG.md for v{{version}} (git-cliff preview) ==="
    git-cliff --tag v{{version}}
    @echo ""
    @echo "=== dist plan (release artifacts) ==="
    dist plan

# Cut a release: bump the single workspace version (every crate inherits it),
# regenerate CHANGELOG.md from the conventional-commit history (git-cliff),
# commit, and tag vX.Y.Z. Does NOT push — review, then fire the pipeline with
# `git push --follow-tags origin main` (the tag drives cargo-dist's signed
# release + image.yml's container image). Run `just release-dry X.Y.Z` first.
# Requires git-cliff; must run on a clean `main`. e.g. `just release 0.1.0`.
release version:
    #!/usr/bin/env bash
    set -euo pipefail
    [ -z "$(git status --porcelain)" ] || { echo "error: working tree is not clean"; exit 1; }
    [ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || { echo "error: release from main"; exit 1; }
    command -v git-cliff >/dev/null || { echo "error: git-cliff not installed (brew install git-cliff)"; exit 1; }
    # The workspace version is the single source of truth — after every crate
    # switched to `version.workspace = true`, this is the only literal
    # `version = "..."` in the root manifest, so this anchored edit is precise.
    sed -i.bak -E "s/^version = \"[^\"]*\"/version = \"{{version}}\"/" Cargo.toml && rm -f Cargo.toml.bak
    # Sync Cargo.lock to the new workspace-crate versions (workspace members
    # only — no dependency churn).
    cargo update --workspace
    # Regenerate the changelog so the new [X.Y.Z] section exists at the tagged
    # commit — cargo-dist reads it for the GitHub Release body (release.yml).
    git-cliff --tag v{{version}} --output CHANGELOG.md
    git add Cargo.toml Cargo.lock CHANGELOG.md
    git commit -m "chore(release): v{{version}}"
    git tag -a "v{{version}}" -m "v{{version}}"
    echo ""
    echo "Tagged v{{version}} locally (NOT pushed). Review the commit, then fire the"
    echo "release: git push --follow-tags origin main"

# Clean build artefacts (cargo target + mdBook output).
clean:
    cargo clean || true
    rm -rf book
