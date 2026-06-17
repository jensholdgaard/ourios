#!/usr/bin/env bash
# Reclaim disk on GitHub-hosted ubuntu runners before a heavy Cargo build.
#
# `cargo test --all-features` (and `cargo llvm-cov`) link the full
# DataFusion-heavy workspace plus every integration-test binary with debug
# info; the resulting target/ outgrew ubuntu-latest's ~14 GiB root disk and
# failed mid-link with "No space left on device (os error 28)". The big
# preinstalled SDKs below (Android alone is ~9 GiB) are unused by this Rust
# workspace, so removing them buys ample headroom without a third-party
# action (keeping the SHA-pinned-actions / Scorecard posture intact).
set -euo pipefail

echo "Disk before:"
df -h /

# `|| true`: the set is best-effort — a missing dir on a future image must
# not fail the build (the goal is headroom, not a specific layout).
sudo rm -rf \
  /usr/local/lib/android \
  /usr/share/dotnet \
  /opt/ghc \
  /usr/local/.ghcup \
  /opt/hostedtoolcache/CodeQL \
  /usr/local/share/boost || true

echo "Disk after:"
df -h /
