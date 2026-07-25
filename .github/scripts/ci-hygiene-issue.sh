#!/usr/bin/env bash
# Open (or close) the tracking issue for one CI-hygiene canary signal.
#
# The canary is the scheduled run of ci.yml (see its `on:` block): the two
# hermetic jobs, clippy and cargo-deny, run nightly so that toolchain drift and
# new RustSec advisories surface as a tracking issue instead of as a surprise
# red check on an unrelated pull request.
#
# Adapted from the Tier-1 reporter in open-telemetry/opentelemetry-rust#3596
# (cijothomas), with two deliberate changes: the dedupe scans the labelled
# issues directly instead of using GitHub's search index (which lags behind
# issue creation, so two consecutive failing runs could each believe they were
# the first and file duplicates), and there is no auto-fix tier — `clippy --fix`
# cannot make the judgement calls pedantic lints require (CLAUDE.md §6.4).
#
# Usage: ci-hygiene-issue.sh <signature> <result> <title>
#   <signature>  stable key identifying the tracking issue, e.g. `clippy`
#   <result>     a `needs.<job>.result` value; `success` closes, anything else
#                opens (`skipped`/`cancelled` included — a canary that did not
#                actually run green is not evidence of green)
#   <title>      issue title, used only when opening
set -euo pipefail

if [[ $# -ne 3 ]]; then
    echo "usage: $0 <signature> <result> <title>" >&2
    exit 2
fi

signature="$1"
result="$2"
title="$3"

label="ci-hygiene"
marker="<!-- ci-hygiene:signature=${signature} -->"
repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
run_url="${GITHUB_SERVER_URL:-https://github.com}/${repo}/actions/runs/${GITHUB_RUN_ID:-0}"

# The open tracking issue for this signature, if any. Scanned locally rather
# than with `--search`: the search index lags issue creation by seconds to
# minutes, which is exactly the window two consecutive canary runs land in.
existing="$(
    gh issue list --repo "$repo" --label "$label" --state open \
        --limit 100 --json number,body |
        jq -r --arg marker "$marker" \
            '[.[] | select(.body != null and (.body | contains($marker)))] | .[0].number // empty'
)"

if [[ "$result" == "success" ]]; then
    if [[ -n "$existing" ]]; then
        gh issue comment "$existing" --repo "$repo" \
            --body "The \`${signature}\` canary is green again as of ${run_url} — closing."
        gh issue close "$existing" --repo "$repo"
        echo "closed #${existing} (${signature} recovered)"
    else
        echo "${signature} canary green, nothing open"
    fi
    exit 0
fi

if [[ -n "$existing" ]]; then
    echo "#${existing} already tracks the ${signature} canary; not filing a duplicate"
    exit 0
fi

# The label will not exist on the first failing run.
if ! gh label list --repo "$repo" --json name --jq '.[].name' | grep -qx "$label"; then
    gh label create "$label" --repo "$repo" --color FBCA04 \
        --description "Scheduled CI canary found drift (toolchain, advisories)"
fi

body_file="$(mktemp)"
trap 'rm -f "$body_file"' EXIT
cat >"$body_file" <<EOF
The scheduled CI-hygiene canary failed on \`main\`.

- Signal: \`${signature}\` (job result: \`${result}\`)
- Run: ${run_url}

This is the **same job PR CI runs**, just on a timer — so this failure will also
hit the next unrelated pull request until it is fixed. Nothing in the repository
changed to cause it; the usual causes are a new stable Rust release changing
\`clippy::pedantic\`, or a newly published RustSec advisory.

This issue closes itself on the next green canary run.

${marker}
EOF

gh issue create --repo "$repo" --title "$title" --label "$label" --body-file "$body_file"
