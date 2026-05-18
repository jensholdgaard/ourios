# `testdata/corpus/`

Plain-text log corpora used by the miner's property tests (RFC 0001
§5 — hazard H7.1) and, eventually, by `ourios-bench` for the
thesis-gate measurements in `docs/benchmarks.md` (A1 compression,
C2 template-count convergence).

## Format

One log line per file row. UTF-8. No trailing whitespace.
Empty rows are skipped by the loader.

The loader (`crates/ourios-miner/tests/corpus.rs`) reads every
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

## H7.1 design constraints on corpus lines

The MVP H7.1 implementation does **not** snapshot templates per
`template_version`, so it requires that no widening fire during
corpus ingestion. Practically: every line must either create a
fresh leaf or attach with `sim_seq = 1.0` (mask-emit-driven
variation only — numbers, UUIDs, IPs at variable positions;
identical literals everywhere else).

A line like `user 42 logged in` and `user 17 logged in` both
mask to `["user", "<NUM>", "logged", "in"]` — clean attach, no
widening. ✓

A line like `user 42 logged in` and `user 42 logged out` mask
to different shapes (`in` vs `out`), share a tree path, and
trigger a §6.2 step-5 widening. ✗ — would break H7.1 today by
mutating the leaf template between ingestion and reconstruction
lookup.

Future PRs that want to corpus-exercise widening must add
template-version snapshotting to the test before adding the
divergent literal pairs to the corpus.

## Adding lines

- Drop them in an existing `*.txt` file or add a new file.
- Re-run `cargo test --all-features` to verify H7.1 still passes.
- If a new line introduces widening with an existing line, the
  test will fail with a reconstruction mismatch — adjust until
  the corpus stays on the no-widening contract.

## Anonymisation

Lines committed here must not contain real user data, real IPs
from production traffic, real internal hostnames, etc. Synthetic
or public-domain log shapes only. The `seed.txt` file is
hand-curated synthetic.
