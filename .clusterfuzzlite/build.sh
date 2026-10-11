#!/bin/bash -eu
# Build the RFC 0015 cargo-fuzz targets and stage them (with seed corpora)
# for ClusterFuzzLite. Run inside the OSS-Fuzz base-builder-rust container
# by the build_fuzzers action; $SRC and $OUT are provided by that image.
cd "$SRC/ourios"

cargo fuzz build -O --debug-assertions

for f in fuzz/fuzz_targets/*.rs; do
  target=$(basename "${f%.*}")
  # Not staged, so batch, prune and coverage skip it: parquet 59's footer
  # decoder (read_thrift_vec) reserves an untrusted list count and OOMs
  # within minutes (#959). The daily smoke run still fuzzes it. Delete this
  # exclusion when #959 closes (parquet 60 via the DataFusion 56 upgrade, #846).
  if [ "$target" = audit_reader ]; then
    continue
  fi
  # cargo-fuzz writes to fuzz/target/<triple>/release/<target>; locate the
  # binary regardless of which target triple it selected (don't hardcode
  # the triple).
  bin=$(find fuzz/target -type f -path "*/release/$target" -print -quit)
  if [ -z "$bin" ]; then
    echo "build.sh: no built binary found for fuzz target '$target'" >&2
    exit 1
  fi
  cp "$bin" "$OUT/"
  # Stage committed seeds as <target>_seed_corpus.zip (OSS-Fuzz convention),
  # guarding against an absent or empty seed dir (an empty glob would abort
  # the build under `set -e`).
  if [ -d "fuzz/seeds/$target" ] && [ -n "$(ls -A "fuzz/seeds/$target")" ]; then
    zip -j "$OUT/${target}_seed_corpus.zip" "fuzz/seeds/$target"/* >/dev/null
  fi
  # Stage <target>.options (OSS-Fuzz convention): the libFuzzer engine reads
  # the `[libfuzzer]` section next to the binary and appends its flags.
  if [ -f "fuzz/$target.options" ]; then
    cp "fuzz/$target.options" "$OUT/${target}.options"
  fi
  # The `dict =` line in <target>.options names a file next to the binary.
  if [ -f "fuzz/dict/$target.dict" ]; then
    cp "fuzz/dict/$target.dict" "$OUT/${target}.dict"
  fi
done
