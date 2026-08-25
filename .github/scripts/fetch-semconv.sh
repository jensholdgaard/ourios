#!/usr/bin/env bash
# Fetch the shared ourios-semconv registry repo at the ref pinned in
# semconv/REGISTRY_REF and print the checkout path. Used by the CI
# `semconv` + `live-check` jobs and the `just semconv-generate` recipe,
# so all consumers of the pin resolve it identically.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
ref="$(tr -d '[:space:]' < "$repo_root/semconv/REGISTRY_REF")"
[ -n "$ref" ] || { echo "error: semconv/REGISTRY_REF is empty" >&2; exit 1; }

dest="${SEMCONV_CHECKOUT_DIR:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}/ourios-semconv-$ref}"
if [ ! -d "$dest/registry" ]; then
    rm -rf "$dest"
    git clone --quiet --depth 1 --branch "$ref" \
        https://github.com/jensholdgaard/ourios-semconv.git "$dest" >&2
fi
echo "$dest"
