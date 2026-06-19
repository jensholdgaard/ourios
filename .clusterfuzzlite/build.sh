#!/bin/bash -eu
# Build the RFC 0015 cargo-fuzz targets and stage them (with seed corpora)
# for ClusterFuzzLite. Run inside the OSS-Fuzz base-builder-rust container
# by the build_fuzzers action; $SRC and $OUT are provided by that image.
cd "$SRC/ourios"

cargo fuzz build -O --debug-assertions

fuzz_target_output_dir=fuzz/target/x86_64-unknown-linux-gnu/release
for f in fuzz/fuzz_targets/*.rs; do
  target=$(basename "${f%.*}")
  cp "$fuzz_target_output_dir/$target" "$OUT/"
  # Stage committed seeds as <target>_seed_corpus.zip (OSS-Fuzz convention).
  if [ -d "fuzz/seeds/$target" ]; then
    zip -j "$OUT/${target}_seed_corpus.zip" "fuzz/seeds/$target"/* >/dev/null
  fi
done
