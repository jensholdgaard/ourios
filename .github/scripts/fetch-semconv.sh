#!/usr/bin/env bash
# Fetch the shared ourios-semconv registry repo at the ref pinned in
# semconv/REGISTRY_REF and print the checkout path. Used by the CI
# `semconv` + `live-check` jobs and the `just semconv-generate` recipe,
# so all consumers of the pin resolve it identically.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
ref="$(tr -d '[:space:]' < "$repo_root/semconv/REGISTRY_REF")"
# The pin must be a tag or branch NAME (git clone --branch takes no
# bare SHA). Commit a TAG: a branch pin is for local development only —
# it re-syncs on every run, and because the generated output is
# committed and no-diff-gated, a moved branch fails CI loudly rather
# than drifting silently. The ref is interpolated into a path handed
# to rm -rf —
# constrain it to a conservative charset so a malformed pin can never
# traverse out of the checkout dir or read as a git/rm option.
case "$ref" in
    ''|-*|*[!A-Za-z0-9._-]*|*..*)
        echo "error: semconv/REGISTRY_REF must be a tag/branch name matching [A-Za-z0-9._-]+ (no leading '-', no '..'); got '$ref'" >&2
        exit 1;;
esac

# SEMCONV_CHECKOUT_DIR is a BASE directory (a cache root); the
# ref-named checkout always lives one level below it, so the rm -rf
# on a partial checkout can never touch the base itself.
base="${SEMCONV_CHECKOUT_DIR:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}}"
dest="$base/ourios-semconv-$ref"
if [ ! -d "$dest/.git" ] || [ ! -d "$dest/registry" ] || [ ! -d "$dest/templates" ]; then
    rm -rf "$dest"
    git clone --quiet --depth 1 --branch "$ref" \
        https://github.com/jensholdgaard/ourios-semconv.git "$dest" >&2
else
    # A reused checkout re-syncs to the pin's current remote state, so
    # a branch-name pin can never serve a stale cache (a tag re-fetch
    # is a cheap no-op).
    git -C "$dest" fetch --quiet --depth 1 origin "$ref" >&2
    git -C "$dest" reset --quiet --hard FETCH_HEAD >&2
fi
echo "$dest"
