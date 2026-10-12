# ClusterFuzzLite build container for the RFC 0015 cargo-fuzz targets
# (Phase 2). The OSS-Fuzz Rust base ships a nightly toolchain with
# cargo-fuzz preinstalled and the sanitizer flags wired up, so build.sh
# only has to `cargo fuzz build` and stage the binaries. Its presence is
# also what OpenSSF Scorecard's Fuzzing check detects (RFC0015.7).
FROM gcr.io/oss-fuzz-base/base-builder-rust@sha256:3428d0a90b1e7efbe2ba947bd1d0c9a70594986b4b6553804547f1fb24cfc92f

# The base image pins its own nightly through `RUSTUP_TOOLCHAIN`, which
# rustup honours ahead of any `rust-toolchain.toml` — upstream sets it
# precisely so a project's own pin cannot take effect. That nightly trails
# the workspace MSRV (the image tagged `latest` on 2026-07-30 still carries
# rustc 1.91.0-nightly), and DataFusion 55 raised the floor to 1.94 in RFC
# 0021 phase 2a, so `cargo fuzz build` fails resolution before it compiles
# anything. Bumping the image does not help; the pin has to be overridden
# here.
#
# This is the nightly the scheduled `Fuzz` workflow already runs, so both
# fuzzing paths build on one toolchain. Move the two together.
ENV RUSTUP_TOOLCHAIN=nightly-2026-06-01
RUN rustup toolchain install --profile minimal "$RUSTUP_TOOLCHAIN"

COPY . $SRC/ourios
WORKDIR $SRC/ourios
COPY .clusterfuzzlite/build.sh $SRC/
