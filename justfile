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
# receiver, a Collector is optional — but `just jaeger-up` runs one as the
# single front door (traces → Jaeger, logs → here), which is what
# `dogfood-env` prints. Open receiver (no auth section → open, per RFC 0026),
# local filesystem store + WAL under scratch/dogfood/ (gitignored).
# Ports: 24318 OTLP/HTTP, 24317 OTLP/gRPC — NOT the standard 4317/4318, which
# the Collector claims — and 4319 query API + /mcp.
# Ctrl-C to stop; `just dogfood-clean` to wipe the captured store.
#
# Run `just dogfood-env` in the other terminal for the source-side env block.
dogfood-server:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p scratch/dogfood/store scratch/dogfood/wal
    echo "OTLP logs → http://127.0.0.1:24318 (HTTP) · 127.0.0.1:24317 (gRPC)"
    echo "query API → http://127.0.0.1:4319  ·  MCP → http://127.0.0.1:4319/mcp"
    echo "store     → scratch/dogfood/  ·  promoting attr.{model,tool_name,decision}"
    # All settings live in dogfood-config.yaml (RFC 0020 file front-end), not
    # OURIOS_* env vars: `--config` is the *sole* config source, and
    # storage.promoted_attributes (RFC 0022) — which promotes the GenAI
    # attributes so a source can aggregate its own telemetry natively (e.g.
    # `count by attr.model`) — has no env-var form. The config binds loopback
    # only and runs open (no auth) + MCP on; safe only because it's local.
    # The file's paths resolve from ${env:OURIOS_DOGFOOD_ROOT}.
    export OURIOS_DOGFOOD_ROOT="$(pwd)/scratch/dogfood"
    cargo run -p ourios-server -- --config dogfood-config.yaml

# Print the env block that points a source's OTLP telemetry at the Collector
# (`jaeger-up`), which fans out: logs → `dogfood-server` (queryable as data),
# traces → Jaeger (browsable as spans). One endpoint covers both signals.
# Start both `jaeger-up` and `dogfood-server` first. The *enable* flag is
# tool-specific and printed as a per-tool comment rather than hard-coded, so
# the same block works for Claude Code, Copilot CLI, or any OTLP source; a
# source with no traces signal simply sends none, and its logs still flow.
# Telemetry is read at process startup, so `export` these and start a NEW
# session of the source. Content capture (prompts/tool output) is opt-in and
# off by default — that is where the wordy structured bodies live, so enable it
# only on data you're willing to retain, and scrub before freezing a corpus.
dogfood-env:
    #!/usr/bin/env bash
    cat <<'ENV'
    export OTEL_LOGS_EXPORTER=otlp
    export OTEL_METRICS_EXPORTER=none        # Ourios is logs-only (CLAUDE.md §1)
    export OTEL_TRACES_EXPORTER=otlp         # Collector routes these to Jaeger
    export OTEL_EXPORTER_OTLP_PROTOCOL=grpc
    export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317
    export OTEL_SERVICE_NAME=agent-dogfood   # your source identity -> the Ourios tenant
    # then enable telemetry on the source (per-tool flag):
    #   Claude Code:  export CLAUDE_CODE_ENABLE_TELEMETRY=1
    #                 export CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1  # required for spans (beta)
    #   Copilot CLI:  export COPILOT_OTEL_ENABLED=true
    # opt-in content capture (privacy: retains prompts/tool output):
    #   Claude Code:  export OTEL_LOG_USER_PROMPTS=1 OTEL_LOG_TOOL_DETAILS=1
    # query the ingested logs over HTTP (needs x-ourios-tenant; tenant == service.name):
    #   curl -sS http://127.0.0.1:4319/v1/query \
    #     -H 'x-ourios-tenant: agent-dogfood' \
    #     -H 'content-type: text/plain' \
    #     --data 'severity >= trace | range(-1h, now) | limit 20'
    # browse the traces at http://localhost:16686 (Jaeger UI)
    # close the loop — let the source query its own telemetry via Ourios's MCP
    # (open mode takes the tenant as a tool argument, no bearer):
    #   Claude Code:  claude mcp add --transport http ourios http://127.0.0.1:4319/mcp
    #   then ask it to read ourios://query-schema and query tenant "agent-dogfood"
    # no Collector? point straight at dogfood-server (logs only, no trace viewer):
    #   export OTEL_TRACES_EXPORTER=none
    #   export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:24317
    ENV

# Wipe the local dogfood store + WAL (the captured telemetry). Refuses while
# `dogfood-server` is still listening on 24318, so a `rm -rf` can't race the
# server mid-write and corrupt the capture. Stop the server first.
dogfood-clean:
    #!/usr/bin/env bash
    set -euo pipefail
    # Fail hard if lsof is missing rather than silently skip the guard (a failed
    # `if` condition is not caught by `set -e`), so a missing tool can't let the
    # wipe race a running server.
    command -v lsof >/dev/null || { echo "error: lsof not found — can't verify the server is stopped; stop dogfood-server, then 'rm -rf scratch/dogfood' by hand." >&2; exit 1; }
    if lsof -nP -iTCP:24318 -sTCP:LISTEN >/dev/null 2>&1; then
        echo "127.0.0.1:24318 is in use (dogfood-server still running?); stop it before cleaning." >&2
        exit 1
    fi
    rm -rf scratch/dogfood

# Start a local OTel Collector + Jaeger v2 (docker compose) as the single OTLP
# front door: a source points one endpoint here and the Collector fans out —
# traces → Jaeger (browse at http://localhost:16686), logs → `dogfood-server`
# (queryable as data). `ourios-server`'s own self-tracing (RFC 0038/0039/0040)
# points here too. See `dogfood-env` for the source-side block.
#
# Claims the standard 4317 (OTLP gRPC) / 4318 (OTLP HTTP); `dogfood-server`
# moved to 24317/24318 to free them. The log fan-out reaches the host-side
# `dogfood-server` via `host.docker.internal` (mapped to `host-gateway` below,
# which colima honours the same way Docker Desktop does) — logs are dropped
# with a Collector-side export error if `dogfood-server` isn't running, which
# is the intended failure mode: the viewer stays useful on its own.
#
# **macOS-verified; on native Linux the log fan-out needs one extra step** —
# `host-gateway` there is the docker bridge address, which cannot reach
# `dogfood-server`'s loopback bind. See the note in `dogfood-config.yaml` for
# the two remedies. Traces to Jaeger work on every runtime either way.
# Materialises the compose file + Collector config into gitignored
# `scratch/observability/` (this recipe is the source of truth; the scratch
# copy is disposable).
jaeger-up:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p scratch/observability
    cat > scratch/observability/otel-collector-config.yaml <<'YAML'
    receivers:
      otlp:
        protocols:
          grpc:
            endpoint: 0.0.0.0:4317
          http:
            endpoint: 0.0.0.0:4318
    processors:
      batch:
    exporters:
      otlp/jaeger:
        endpoint: jaeger:4317
        tls:
          insecure: true
      otlp/ourios:
        endpoint: host.docker.internal:24317
        tls:
          insecure: true
      debug:
        verbosity: basic
    service:
      pipelines:
        traces:
          receivers: [otlp]
          processors: [batch]
          exporters: [otlp/jaeger, debug]
        metrics:
          receivers: [otlp]
          processors: [batch]
          exporters: [debug]
        logs:
          receivers: [otlp]
          processors: [batch]
          exporters: [otlp/ourios, debug]
    YAML
    cat > scratch/observability/docker-compose.yaml <<'YAML'
    services:
      jaeger:
        image: jaegertracing/jaeger:2.20.0
        container_name: ourios-jaeger
        ports:
          - "127.0.0.1:16686:16686" # Jaeger UI — loopback only, unauthenticated
        networks:
          - otel
      otel-collector:
        image: otel/opentelemetry-collector-contrib:0.157.0
        container_name: ourios-otel-collector
        command: ["--config=/etc/otelcol/config.yaml"]
        volumes:
          - ./otel-collector-config.yaml:/etc/otelcol/config.yaml:ro
        ports:
          # Loopback only — an unauthenticated OTLP receiver on a LAN
          # interface would accept trace/log ingestion from any reachable peer.
          - "127.0.0.1:4317:4317" # OTLP gRPC — point OTEL_EXPORTER_OTLP_ENDPOINT here
          - "127.0.0.1:4318:4318" # OTLP HTTP
        extra_hosts:
          # Lets the log fan-out reach `dogfood-server` on the host.
          - "host.docker.internal:host-gateway"
        depends_on:
          - jaeger
        networks:
          - otel
    networks:
      otel:
        driver: bridge
    YAML
    docker compose -f scratch/observability/docker-compose.yaml up -d
    echo "Jaeger UI                 → http://localhost:16686"
    echo "OTLP endpoint (gRPC, :4317 — use as-is for OTEL_EXPORTER_OTLP_ENDPOINT)"
    echo "OTLP endpoint (HTTP, :4318 — browsable-looking but still OTLP, not a UI)"
    echo "Logs fan out to dogfood-server on :24317 — start it too, or they're dropped."
    echo "Run 'just dogfood-env' for the source-side env block."

# Stop the Jaeger + Collector stack `jaeger-up` started. A no-op (not an
# error) if `jaeger-up` was never run or `scratch/observability` was cleaned
# — `docker compose down` on a nonexistent compose file is the failure mode
# this guards, so teardown stays safe to run unconditionally.
jaeger-down:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -f scratch/observability/docker-compose.yaml ]; then
        docker compose -f scratch/observability/docker-compose.yaml down
    else
        echo "nothing to stop (scratch/observability/docker-compose.yaml not found)"
    fi

# Print a **traces-only** env block for Claude Code: spans go to Jaeger, and
# `OTEL_LOGS_EXPORTER=none` keeps prompts/tool output off disk entirely — the
# variant to use when you want the trace viewer without the log capture
# `dogfood-env` sets up. Traces are a Claude Code beta signal: both
# `CLAUDE_CODE_ENABLE_TELEMETRY` and the enhanced-beta flag are required, or
# no spans are emitted at all. Export these and start a NEW `claude` session
# (telemetry config is read at process startup).
jaeger-env:
    #!/usr/bin/env bash
    cat <<'ENV'
    export CLAUDE_CODE_ENABLE_TELEMETRY=1
    export CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1   # required — traces are opt-in beta
    export OTEL_TRACES_EXPORTER=otlp
    export OTEL_LOGS_EXPORTER=none                 # no log capture — traces only
    export OTEL_METRICS_EXPORTER=none
    export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317
    export OTEL_EXPORTER_OTLP_PROTOCOL=grpc
    export OTEL_SERVICE_NAME=claude-code
    # want the logs captured into Ourios too? use 'just dogfood-env' instead.
    # ourios-server's own self-tracing (RFC 0038/0039/0040) points at the same
    # Collector — it just needs the standard var, no beta flag:
    #   OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317 just dogfood-server
    # then browse both at http://localhost:16686 (separate traces — no shared
    # trace context between the two processes, just the same backend).
    ENV

# Clean build artefacts (cargo target + mdBook output).
clean:
    cargo clean || true
    rm -rf book
