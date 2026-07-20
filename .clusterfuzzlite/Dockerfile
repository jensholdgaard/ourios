# ClusterFuzzLite build container for the RFC 0015 cargo-fuzz targets
# (Phase 2). The OSS-Fuzz Rust base ships a nightly toolchain with
# cargo-fuzz preinstalled and the sanitizer flags wired up, so build.sh
# only has to `cargo fuzz build` and stage the binaries. Its presence is
# also what OpenSSF Scorecard's Fuzzing check detects (RFC0015.7).
FROM gcr.io/oss-fuzz-base/base-builder-rust@sha256:a427e739824ab8b42e626648e8f3f621b01173fc23272e7451f70e76fc89bfe3

COPY . $SRC/ourios
WORKDIR $SRC/ourios
COPY .clusterfuzzlite/build.sh $SRC/
