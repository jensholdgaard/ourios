# ClusterFuzzLite build container for the RFC 0015 cargo-fuzz targets
# (Phase 2). The OSS-Fuzz Rust base ships a nightly toolchain with
# cargo-fuzz preinstalled and the sanitizer flags wired up, so build.sh
# only has to `cargo fuzz build` and stage the binaries. Its presence is
# also what OpenSSF Scorecard's Fuzzing check detects (RFC0015.7).
FROM gcr.io/oss-fuzz-base/base-builder-rust@sha256:90232e3639ae509d35edf27b3d2b6e64f880f5b33136b6c7847a06eef9d8fd7c

COPY . $SRC/ourios
WORKDIR $SRC/ourios
COPY .clusterfuzzlite/build.sh $SRC/
