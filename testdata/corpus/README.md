# `testdata/corpus/`

Plain-text log corpora used by the miner's property tests (RFC 0001
§5 — hazard H7.1) and, eventually, by `ourios-bench` for the
thesis-gate measurements in `docs/benchmarks.md` (A1 compression,
C2 template-count convergence).

## Format

One log line per file row. UTF-8. No trailing whitespace.
Empty rows are skipped by the loader.

The loader (`crates/ourios-miner/tests/hazards.rs`) reads every
`*.txt` file under this directory and wraps each line in an
`OtlpLogRecord` with `Body::String(line)` plus a default tenant
/ severity / scope. This is the minimum shape H7.1 needs — the
property test asserts byte-identical reconstruction for every
non-lossy record, which is body-level only.

**Roadmap migration.** `docs/roadmap.md` Phase 3 names the corpus
shape as "OTLP `LogsData` (canonical JSON or protobuf)" so the
bench exercises the same record shape an OTel deployment would
produce. The plain-text seed corpus here is interim; the
follow-up that lands `opentelemetry-proto`'s JSON / protobuf
deserialization swaps the loader without changing H7.1's
assertion.

## H7.1 and widening

H7.1 snapshots templates per `(template_id, template_version)`
after every ingest, so widening (§6.2 step 5, §6.4) is a
first-class part of what the corpus exercises. A record emitted
at `(id, v_emit)` is reconstructed against the v_emit-era
template even when a later ingest pushes the leaf to v+1.

There is no longer a no-widening contract on the corpus:
literal-divergent pairs that drive `<*>` widening are welcome,
and in fact are the most valuable lines to add — they exercise
the STR-fallback branch in `reconstruct` and the version-bump
path in the miner.

## Adding lines

- Drop them in an existing `*.txt` file or add a new file.
- Re-run `cargo test --all-features` to verify H7.1 still passes.
- A reconstruction-mismatch failure means a real bug in the
  miner or `reconstruct`, not a corpus design violation —
  investigate the diff, do not "fix" the corpus to avoid it.

## Anonymisation

Lines committed here must not contain real user data, real IPs
from production traffic, real internal hostnames, real pod
names, real commit SHAs, etc. Synthetic or public-domain log
shapes only.

- `seed.txt` is hand-curated synthetic.
- `tekton.txt` started as a real Tekton events-controller pod
  log dump; identifying fields were replaced with shape-
  preserving synthetics (pod hash → `aaaaaaaaaa-bbbbb`, leader
  UUID → `00000000-0000-0000-0000-000000000001`, commit SHA →
  `0000000`, internal collector DNS → `otel-collector...`,
  runtime pointer → `0xdeadbeef0000`, float wall-clocks
  normalised). The token classes the miner sees (NUM, UUID, IP,
  hex pointer, pod-name, JSON-vs-plaintext interleaving,
  multi-KB stack traces in `stacktrace` fields) are preserved.
