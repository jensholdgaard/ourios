# Ourios developer command runner.
#
# Run `just` (no args) to see all available recipes.
# `just check` is the one-command pre-merge gate that mirrors
# `CLAUDE.md` §6.6 ("Forced verification before done").
#
# Install on macOS: `brew install just`
# Install elsewhere: https://github.com/casey/just#packages

# Pass recipe arguments as positional args ($1, $2, ...) to shebang recipes.
# just's `{{...}}` substitution is textual, so an argument interpolated into a
# shell line — even inside double quotes — would let embedded `$(...)`/backticks
# execute. Capturing `$1` into a shell variable instead keeps untrusted input
# (e.g. a release version) as data the shell never re-parses.
set positional-arguments

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
# `dist plan --tag` parses the version and rejects anything that isn't a valid
# release tag — we don't re-validate SemVer ourselves. Requires git-cliff
# (`brew install git-cliff`) + dist (cargo-dist). e.g. `just release-dry 0.1.0`.
release-dry version:
    #!/usr/bin/env bash
    set -euo pipefail
    version="$1"
    # `dist plan` accepts SemVer build metadata, but image.yml tags the release
    # via docker/metadata-action `type=semver` and a Docker tag cannot contain
    # `+` — reject it so the preview matches the real release constraints. This
    # is the one constraint the canonical parsers are blind to; everything else
    # (leading `v`, leading zeroes, non-numeric) `dist plan --tag` rejects below.
    case "$version" in *+*) echo "error: version must not contain '+build' metadata (not a legal container tag); got '$version'"; exit 1;; esac
    command -v git-cliff >/dev/null || { echo "error: git-cliff not installed (brew install git-cliff)"; exit 1; }
    command -v dist >/dev/null || { echo "error: dist (cargo-dist) not installed"; exit 1; }
    # `dist plan` first: it parses the tag and rejects an invalid one (e.g. a
    # stray leading `v`), so we fail fast before git-cliff prints a changelog for
    # a tag that can't actually be released. `--tag` previews the intended version
    # (not the current workspace version); `--force-tag` lets it do so unbumped.
    echo "=== dist plan (release artifacts) ==="
    dist plan --tag "v$version" --force-tag
    echo ""
    echo "=== CHANGELOG.md for v$version (git-cliff preview) ==="
    git-cliff --tag "v$version"

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
    version="$1"
    # cargo (below) accepts SemVer build metadata as a valid package version, but
    # image.yml tags the release via docker/metadata-action `type=semver` and a
    # Docker tag cannot contain `+` — reject it before mutating anything. cargo's
    # parser still rejects the rest (leading `v`, leading zeroes, non-numeric).
    case "$version" in *+*) echo "error: version must not contain '+build' metadata (not a legal container tag); got '$version'"; exit 1;; esac
    [ -z "$(git status --porcelain)" ] || { echo "error: working tree is not clean"; exit 1; }
    [ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || { echo "error: release from main"; exit 1; }
    command -v git-cliff >/dev/null || { echo "error: git-cliff not installed (brew install git-cliff)"; exit 1; }
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
    if git rev-parse -q --verify "refs/tags/v$version" >/dev/null 2>&1 \
        || git ls-remote --exit-code --tags origin "refs/tags/v$version" >/dev/null 2>&1; then
        echo "error: tag v$version already exists (local or origin)"; exit 1
    fi
    # The workspace version is the single source of truth — after every workspace
    # member crate switched to `version.workspace = true`, this is the only
    # literal `version = "..."` in the root manifest. Read it, and fail fast if
    # the requested version already matches (else the bump is a no-op and the
    # release "commit" would carry only a regenerated changelog/lock, or nothing).
    current="$(sed -nE 's/^version = "([^"]*)"/\1/p' Cargo.toml | head -1)"
    [ -n "$current" ] || { echo "error: could not read the current workspace version from Cargo.toml (expected a literal 'version = \"...\"')"; exit 1; }
    [ "$version" != "$current" ] || { echo "error: version $version is already the current workspace version"; exit 1; }
    # Capture the pristine starting commit (the clean-tree + HEAD==origin/main
    # checks above guarantee it is one) so any failure below rolls the whole
    # attempt back: a hard reset to this SHA reverts every mutation — Cargo.toml,
    # the synced Cargo.lock, the regenerated CHANGELOG.md, and the release commit
    # — then we drop the tag. Disarmed on success. Safer than restoring
    # individual files: a `git tag` failure after the commit would otherwise
    # leave the working tree inconsistent with an advanced HEAD.
    start_sha="$(git rev-parse HEAD)"
    trap 'git reset --hard "$start_sha" >/dev/null 2>&1 || true; git tag -d "v$version" >/dev/null 2>&1 || true; rm -f Cargo.toml.bak deploy/helm/ourios/Chart.yaml.bak' ERR
    # The anchored edit is precise (only the workspace version matches). sed needs
    # a backup suffix to edit in place portably. Keep the edit and the cleanup as
    # separate statements: in `sed ... && rm`, a sed failure is the non-final
    # command of an && list and so is exempt from `set -e`, which would let the
    # script continue with an unbumped Cargo.toml. git reset is the rollback (the
    # trap also drops a stray .bak); this rm just clears it on the success path.
    sed -i.bak -E "s/^version = \"[^\"]*\"/version = \"$version\"/" Cargo.toml
    rm -f Cargo.toml.bak
    # The Helm chart tracks the release too. `appVersion` is the app the chart
    # deploys, so it moves to the release version. The chart's OWN `version` is
    # patch-bumped: chart *feature* changes already bump it in their own PRs, so
    # at release time its only remaining delta is the new appVersion pointer —
    # a patch-level change. This bump is mandatory, not cosmetic: chart versions
    # are immutable in a Helm repo, so a new appVersion (a different default
    # image) under an unchanged chart version would republish different content
    # at the same version. Mirrors the v0.2.1 precedent (chore(release) 7a9d97e).
    chart_yaml="deploy/helm/ourios/Chart.yaml"
    chart_ver="$(sed -nE 's/^version: (.*)/\1/p' "$chart_yaml" | head -1)"
    # Plain X.Y.Z only. The first arm rejects any character outside [0-9.] —
    # so a pre-release/build suffix (`0.4.0-alpha`, `0.4.0+meta`) fails loudly
    # rather than feeding awk a non-numeric patch field; the second enforces the
    # three-segment shape.
    case "$chart_ver" in
        *[!0-9.]*) echo "error: chart version '$chart_ver' is not plain X.Y.Z (has a suffix); bump $chart_yaml by hand"; exit 1;;
        [0-9]*.[0-9]*.[0-9]*) : ;;
        *) echo "error: chart version '$chart_ver' is not plain X.Y.Z; bump $chart_yaml by hand"; exit 1;;
    esac
    chart_next="$(echo "$chart_ver" | awk -F. '{printf "%d.%d.%d", $1, $2, $3 + 1}')"
    sed -i.bak -E "s/^version: .*/version: $chart_next/" "$chart_yaml"
    sed -i.bak -E "s/^appVersion: .*/appVersion: \"$version\"/" "$chart_yaml"
    rm -f "$chart_yaml.bak"
    # Sync Cargo.lock to the new workspace-crate versions AND validate the
    # version: cargo parses `version = "..."` with its own SemVer parser, so a
    # malformed arg (leading `v`, leading zeroes, non-numeric) fails here — we
    # don't reimplement that check. A check (not `cargo update`) so third-party
    # deps can't churn into the release commit; it rewrites the lock for the
    # manifest version change + compile-verifies.
    cargo check --workspace
    # Regenerate the changelog so the new [X.Y.Z] section exists at the tagged
    # commit — cargo-dist reads it for the GitHub Release body (release.yml).
    git-cliff --tag "v$version" --output CHANGELOG.md
    git add Cargo.toml Cargo.lock CHANGELOG.md "$chart_yaml"
    git commit -m "chore(release): v$version"
    git tag -a "v$version" -m "v$version"
    # Success: disarm the rollback trap.
    trap - ERR
    echo ""
    echo "Tagged v$version locally (NOT pushed). Review the commit, then fire the"
    echo "release: git push --follow-tags origin main"

# Run ourios-server locally as an OTLP **log** sink for dogfooding — point any
# OTLP log source (Claude Code, Copilot CLI, an OpenTelemetry Collector) at it
# and query the ingested telemetry back. Since Ourios *is* an OTLP log
# receiver, no Collector or container is needed. Open receiver (no auth section
# → open, per RFC 0026), local filesystem store + WAL under scratch/dogfood/
# (gitignored). Ports: 4318 OTLP/HTTP, 4317 OTLP/gRPC, 4319 query API. Ctrl-C
# to stop; `just dogfood-clean` to wipe the captured store.
#
# Run `just dogfood-env` in the other terminal for the source-side env block.
dogfood-server:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p scratch/dogfood/store scratch/dogfood/wal
    echo "OTLP logs → http://localhost:4318 (HTTP) · localhost:4317 (gRPC)"
    echo "query API → http://localhost:4319   ·   store → scratch/dogfood/"
    OURIOS_STORAGE_BACKEND=local \
    OURIOS_BUCKET_ROOT="$(pwd)/scratch/dogfood/store" \
    OURIOS_WAL_ROOT="$(pwd)/scratch/dogfood/wal" \
    OURIOS_RECEIVER_ENABLED=1 \
    OURIOS_QUERIER_ENABLED=1 \
    cargo run -p ourios-server

# Print the env block that points a source's OTLP telemetry at the local
# `dogfood-server`. Ourios is logs-only (CLAUDE.md §1), so metrics/traces are
# disabled. Telemetry is read at process startup, so `export` these and start a
# NEW session of the source (e.g. a fresh `claude`). Content capture
# (prompts/tool output) is opt-in and off by default — that is where the wordy
# structured bodies live, so enable it only on data you're willing to retain,
# and scrub before freezing any of it as a corpus.
# Prints the telemetry env block for the local dogfood-server.
dogfood-env:
    @echo 'export CLAUDE_CODE_ENABLE_TELEMETRY=1'
    @echo 'export OTEL_LOGS_EXPORTER=otlp'
    @echo 'export OTEL_METRICS_EXPORTER=none        # Ourios is logs-only'
    @echo 'export OTEL_TRACES_EXPORTER=none'
    @echo 'export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf'
    @echo 'export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318'
    @echo 'export OTEL_SERVICE_NAME=claude-code     # -> the Ourios tenant'
    @echo '# opt-in content capture (privacy: retains prompts/tool output):'
    @echo '# export OTEL_LOG_USER_PROMPTS=1'
    @echo '# export OTEL_LOG_TOOL_DETAILS=1'

# Wipe the local dogfood store + WAL (the captured telemetry).
dogfood-clean:
    rm -rf scratch/dogfood

# Clean build artefacts (cargo target + mdBook output).
clean:
    cargo clean || true
    rm -rf book
