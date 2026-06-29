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
    # `--tag` so the plan previews the intended version (not the current 0.0.0);
    # `--force-tag` lets it do so without bumping the manifest first.
    dist plan --tag v{{version}} --force-tag

# Cut a release: bump the single workspace version (every workspace member crate
# inherits it; the excluded `fuzz/` harness is a separate workspace and is not
# released), regenerate CHANGELOG.md from the conventional-commit history
# (git-cliff), commit, and tag vX.Y.Z. Does NOT push — review, then fire the
# pipeline with `git push --follow-tags origin main` (the tag drives cargo-dist's
# signed release + image.yml's container image). Run `just release-dry X.Y.Z`
# first. Requires git-cliff; must run on a clean `main`. e.g. `just release 0.1.0`.
release version:
    #!/usr/bin/env bash
    set -euo pipefail
    [ -z "$(git status --porcelain)" ] || { echo "error: working tree is not clean"; exit 1; }
    [ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || { echo "error: release from main"; exit 1; }
    command -v git-cliff >/dev/null || { echo "error: git-cliff not installed (brew install git-cliff)"; exit 1; }
    # The arg is a BARE SemVer (e.g. 0.1.0) — the recipe adds the `v` for the tag.
    # Reject a leading `v` (would make `vv0.1.0`) or any non-SemVer-ish value
    # before touching anything.
    echo "{{version}}" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$' \
        || { echo "error: version must be a bare SemVer like 0.1.0 (no leading 'v'), got '{{version}}'"; exit 1; }
    # Refresh remote refs so the checks below see the real state of origin.
    git fetch --quiet --tags origin
    # Release only from a `main` that exactly matches `origin/main` — never a
    # stale or diverged tree (else the release commit would build on the wrong
    # base and the eventual push could be rejected or rebased).
    [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "error: local main is not in sync with origin/main — pull/push first"; exit 1; }
    # Fail fast if the tag already exists locally OR on origin — BEFORE mutating
    # the manifest / changelog — so a re-run can't advance `main` with a release
    # commit that `git tag` (or the later push) then refuses, leaving the tag the
    # workflow expects missing.
    if git rev-parse -q --verify "refs/tags/v{{version}}" >/dev/null 2>&1 \
        || git ls-remote --exit-code --tags origin "refs/tags/v{{version}}" >/dev/null 2>&1; then
        echo "error: tag v{{version}} already exists (local or origin)"; exit 1
    fi
    # The workspace version is the single source of truth — after every workspace
    # member crate switched to `version.workspace = true`, this is the only
    # literal `version = "..."` in the root manifest. Read it, and fail fast if
    # the requested version already matches (else the bump is a no-op and the
    # release "commit" would carry only a regenerated changelog/lock, or nothing).
    current="$(sed -nE 's/^version = "([^"]*)"/\1/p' Cargo.toml | head -1)"
    [ "{{version}}" != "$current" ] || { echo "error: version {{version}} is already the current workspace version"; exit 1; }
    # The anchored edit is precise (only the workspace version matches).
    sed -i.bak -E "s/^version = \"[^\"]*\"/version = \"{{version}}\"/" Cargo.toml && rm -f Cargo.toml.bak
    # Sync Cargo.lock to the new workspace-crate versions. A check (not
    # `cargo update`) so third-party deps can't churn into the release commit:
    # it rewrites the lock for the manifest version change + compile-verifies.
    cargo check --workspace
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
