# ourios-fuzz

The RFC 0015 cargo-fuzz harness: six libFuzzer targets over the miner, the
untrusted-input parsers and the persisted artifacts read back from object
storage, each asserting an invariant (not just "no panic") — see the target's module docs and `docs/rfcs/0015-fuzzing-harness.md`.

## Run locally

Needs the nightly pinned in `.github/workflows/fuzz.yml` (`NIGHTLY:`) and
`cargo-fuzz`:

```sh
rustup toolchain install nightly-2026-06-01 --profile minimal
cargo install cargo-fuzz --locked

# From the repo root. Seed the working corpus (gitignored) from the committed
# seeds, then run one target; the targets with a fuzz/dict/<target>.dict take it.
target=dsl_parse
mkdir -p fuzz/corpus/$target && cp -n fuzz/seeds/$target/* fuzz/corpus/$target/
dict=; [ -f fuzz/dict/$target.dict ] && dict=-dict=fuzz/dict/$target.dict
cargo +nightly-2026-06-01 fuzz run $target -- -max_total_time=60 $dict
```

`cargo +nightly-2026-06-01 fuzz build` compiles every target (ASan, debug
assertions). On a Linux host pass `--target x86_64-unknown-linux-gnu`;
cargo-fuzz otherwise picks the musl triple, which ASan cannot link.

## Layout

| Path | What |
|---|---|
| `fuzz_targets/<target>.rs` | one target per binary; the oracle is in its module doc |
| `common/*.rs` | code shared by `#[path]` include — the package has no library API (RFC 0015 §3.1) |
| `seeds/<target>/` | committed seed corpus, copied into the working corpus by CI |
| `dict/<target>.dict` | libFuzzer dictionaries for `otlp_json`, `dsl_parse` and `artifact_decode` |
| `corpus/`, `artifacts/`, `target/` | gitignored: the working corpus, crash reproducers, build output |

Binary seeds are generated, not hand-written: `seeds/otlp_protobuf/one-record.bin`
is the prost encoding (`encode_to_vec`) of `seeds/otlp_json/one-record.json`
after `decode_json`; `seeds/wal_frame/<kind>.bin` is `ourios_wal::frame::write_frame`
(the `fuzzing` feature) of a short payload for each `FrameKind`. Regenerate
with a throwaway binary against those two functions if either format moves.

The persisted-artifact seeds come from one throwaway binary over the real
encoders, for tenant `fuzz-tenant`:

- a `MinerCluster` (`max_node_children: 2`, so one leaf is routed through the
  wildcard child) with a `SharedAuditSink` and a fixed `TestClock` mines a
  handful of string lines (one widening) and one structured body, then
  `fold_through` stamps a horizon;
- `seeds/artifact_decode/miner-snapshot` is `snapshot::snapshot` of its
  `snapshot_state` with `wal_high_water` set to `folded_horizon`;
  `manifest` is `Manifest::to_json`; `template-ids-high-water` is
  `ourios_ingester::template_ids::fuzzing::encode` (the `fuzzing` feature);
- `seeds/artifact_decode/miner-snapshot-adopted` is the same capture from a
  second cluster with `upstream_templates: Adopt`, fed lines carrying a
  `log.record.template` attribute: one adoption converges on a mined leaf
  (tree-backed) and one interns its own id (owned);
- the drained audit events plus one `AliasAsserted` are written by
  `AuditWriter::open_in` to a `Store::in_memory()`;
- `seeds/artifact_decode/template-map-frame` is `derive_template_map` over
  that store, `to_artifact_bytes`; `template-map-json` is that frame
  zstd-decoded.

Each `artifact_decode` seed starts with its selector byte (the target's
module doc has the table): 0 snapshot, 1 manifest, 2 high-water, 3 frame,
4 JSON body.

`miner_roundtrip` reads its input as length-prefixed record bodies: a
little-endian `u16` length (capped at 4096), then that many bytes, repeated
(at most 64 records). Bodies may contain `\n`. To write a seed from a list of
bodies:

```sh
python3 -c 'import struct,sys
bodies=[b"user 42 logged in from 10.0.0.1", b"user 43 logged out\nfrom 10.0.0.2"]
sys.stdout.buffer.write(b"".join(struct.pack("<H",len(b))+b for b in bodies))' \
  > fuzz/seeds/miner_roundtrip/<name>
```

## Triage a crash

1. Reproduce: `cargo +nightly-2026-06-01 fuzz run <target> fuzz/artifacts/<target>/crash-…`.
   The panic message names the invariant (`RFC0015.N / CLAUDE.md §3.x`).
2. Minimise: `cargo +nightly-2026-06-01 fuzz tmin <target> fuzz/artifacts/<target>/crash-…`.
3. Commit the minimised input as `fuzz/seeds/<target>/regression_<issue>_<n>`,
   then fix the bug. The seed stays forever (RFC 0015 §3.5, `CLAUDE.md` §6.2);
   a run must never pass by removing it.
