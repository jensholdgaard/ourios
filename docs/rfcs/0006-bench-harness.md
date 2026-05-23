---
rfc: 0006
title: Bench harness — A1 / C1 / C2 thesis-gate measurement
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-05-22
supersedes: —
superseded-by: —
---

# RFC 0006 — Bench harness: A1 / C1 / C2 thesis-gate measurement

## 1. Summary

Pins the contract for `ourios-bench`: a binary that drives the
shipped `ourios-miner` + `ourios-parquet` pipeline against a
corpus on disk, computes the three writer-side thesis-gate
numbers (`A1` compression, `C1` reconstruction, `C2`
template-count convergence) per `docs/benchmarks.md` §2 / §4,
and writes results into `docs/benchmarks.md` §9 in a
diff-reviewable shape. The RFC fixes the **methodology** —
what counts as a raw byte, what counts as a Parquet byte, when
plateau is plateau, what equals what in reconstruction — before
any code is written, because the difference between "the thesis
holds" being a real claim and a vibe lives in those
definitions. `B1` and `B2` (predicate-pushdown and
template-exact query latency) are excluded: they need the
DataFusion querier (`ourios-querier`, future RFC 0007) and
land in a follow-up extension PR once the querier is live.

## 2. Motivation

### 2.1 The honesty contract collapses without measurement

`CLAUDE.md` §1 declares the project's central claim and §2
names the three pillars that have to hold for the claim to be
true. `docs/benchmarks.md` §7's escalation rule is the
load-bearing consequence: *if two thesis-gates fail on any
representative corpus, we pause implementation and revisit the
pillars.* That rule is a no-op as long as no thesis-gate has
been measured. The §9 status line as of merge of RFC 0005
reads "no benchmark has been run; all targets are aspirational"
— which is fine for the storage layer's RFC, but cannot stay
true through the rest of MVP. RFC 0006 is the gate that flips
§9 from aspirational to measured.

### 2.2 Why bench-first, before the querier

`docs/roadmap.md` §4 Phase 3 names two crates: `ourios-bench`
and `ourios-querier`. Either could go first. Three of the five
thesis-gate goals (`A1`, `C1`, `C2`) need **only the bench** —
the writer and reader shipped through PR-D…PR-G are everything
those gates require on the storage side. The other two (`B1`,
`B2`) need DataFusion plumbing on top. Bench-first means three
thesis-gate signals land before any DataFusion code is written;
querier-first defers all five signals until the querier is
green.

The asymmetric value also runs the other direction: the
methodology RFC is the kind of document that tends to surface
gaps in the writer / reader contract while the storage code is
still fresh, when those gaps are cheaper to fix. Writing it
after the querier lands risks discovering A1-affecting writer
bugs at the same time we're debugging predicate pushdown.

### 2.3 Why an RFC and not just a PR

`docs/rfcs/README.md` requires an RFC for any new crate, which
covers `ourios-bench` mechanically. The methodology section is
the deeper reason: A1 in particular has subtle definitional
choices ("what does `bytes(raw_corpus)` mean for a corpus that
the miner wraps in OTLP envelopes?", "do we include the
audit-stream files in `bytes(parquet)`?", "is `zstd-alone` the
codec or the codec plus the comparable defaults the Drain
paper uses?") that are much easier to argue about in markdown
than in code review. Pinning them in an RFC means the resulting
numbers are not only reproducible but *meaningful* — the §9
Results section that grows out of this work cites the RFC by
section, and changes to the methodology require an amendment.

### 2.4 Why this is one RFC, not three

A natural split would be RFC 0006 (bench crate shape) /
RFC 0007 (A1 methodology) / RFC 0008 (C1+C2 methodology). All
three are co-designed: the crate shape exists *to* compute the
measurements, the A1 plain-text-vs-OTLP corpus decision affects
which loader the crate exposes, and C1/C2 share the per-line
ingest loop that A1 also runs to produce its Parquet output.
Splitting them optimises for short documents and loses the
cross-cutting constraints. The querier (and the B1/B2
methodology it carries) is a genuinely separate concern with
no shared code path and lives in RFC 0007 (future).

## 3. Proposed design

### 3.1 Scope and what this RFC pins

This RFC pins:

- The `ourios-bench` crate's shape: a binary plus a small set
  of supporting modules (corpus loader, ingest harness, result
  writer).
- The corpus input format for v1 — plain-text `*.txt` files
  under `testdata/corpus/`, one line per row, UTF-8, matching
  the existing `crates/ourios-miner/tests/corpus.rs` loader.
  An OTLP-LogsData migration is a future PR; A1 / C1 / C2 are
  meaningful on plain-text input today.
- The A1 / C1 / C2 measurement formulas — what is divided by
  what, where the byte counts come from, what equality means.
- The "hardware baseline annotation" rule: every result line
  carries the machine kind the measurement ran on so deltas
  across hardware classes don't masquerade as code
  regressions.
- The output format: a per-run JSON results file under
  `benchmarks/results/<UTC-RFC3339>-<git-sha>.json`, and a
  human-readable summary appended to `docs/benchmarks.md` §9
  under a date-stamped sub-heading.
- The invocation surface: `cargo run -p ourios-bench --` or
  `just bench`, with CLI flags pinning corpus selection,
  result-file output, and the optional "annotate-only mode"
  that runs measurements but does not write `benchmarks.md`.

This RFC does **not** pin:

- `B1` / `B2` measurement — both need
  `ourios-querier` (future RFC 0007).
- The OTLP-LogsData corpus migration (`docs/roadmap.md` §4's
  "OTLP `LogsData` (canonical JSON or protobuf)" goal). The
  migration is a follow-up PR; it doesn't change the bench's
  measurement formulas, only the loader's parse step.
- `criterion` micro-benchmarks for the miner / writer hot
  paths. `criterion` is the right tool *for* sub-measurements
  (e.g. per-line tokenize cost), but the thesis-gate harness
  is end-to-end. A follow-up may add a `crates/ourios-bench/
  benches/` directory; this RFC does not specify it.
- A `Validated`-stage flip for any RFC — this RFC lands
  `green` (test stubs exist, measurements compile and run on
  the existing seed corpus), with hardware-and-corpus-specific
  validation happening in a follow-up benchmarking session.

### 3.2 Crate shape

```text
crates/ourios-bench/
├── Cargo.toml
└── src/
    ├── main.rs        # CLI entry point, argument parsing
    ├── lib.rs         # public surface for integration tests
    ├── corpus.rs      # *.txt loader (mirrors ourios-miner's
    │                  # tests/corpus.rs but factored for reuse)
    ├── harness.rs     # ingest loop, per-line measurement
    │                  # callbacks (lines into miner, records
    │                  # into Parquet writer, samples C2)
    ├── a1.rs          # A1 compression-ratio computation
    ├── c1.rs          # C1 reconstruction-rate computation
    ├── c2.rs          # C2 template-count-convergence
    │                  # computation, including plateau detection
    └── report.rs      # JSON serialisation + benchmarks.md §9
                       # appender
```

`lib.rs` is non-empty so integration tests under `tests/` can
drive the bench without going through `main.rs` argument
parsing. The binary's `main()` is thin: parse args, configure
harness, call `harness.run()`, hand the result to `report`.

No trait abstraction over `Harness` or `Corpus` until a second
consumer exists. The crate is internal to the project; SemVer
applies to it only via the `WrittenFile` shape under
`benchmarks/results/<...>.json`.

### 3.3 Corpus format

For v1, the bench reads plain-text `*.txt` files under
`testdata/corpus/` per the existing convention in
`testdata/corpus/README.md` and `crates/ourios-miner/tests/
corpus.rs`. Each non-empty line becomes one `OtlpLogRecord`
with `Body::String(line)`, a default tenant (`bench-tenant`),
severity (`9` / `INFO`), and scope (`None` / `None`); the
in-memory shape matches what `MinerCluster::ingest` expects
for `body_kind = String` records.

Time stamps for the synthesised records are deterministic:
`time_unix_nano` starts at a fixed RFC 0005-friendly baseline
(`1_775_127_480_000_000_000`, i.e. 2026-04-02T10:58:00 UTC,
matching the existing test fixtures) and advances by a fixed
1 ms per line. The advancement is artificial; this RFC accepts
the artificiality because A1 / C1 / C2 are time-insensitive (no
gate measures throughput or query latency against a time
range). A future RFC 0007 measurement extension to B1/B2 will
revisit time-stamp synthesis since predicate-pushdown latency
depends on the time-range distribution.

The default tenant means every record lands in the same
partition. This is a simplification — the writer's atomic
publish, row-group rotation, and §3.9 row-vs-path contract
have all been exercised on the multi-partition path through
PR-E2 / PR-F / PR-G. A1 / C1 / C2 are tenant-distribution
neutral. Multi-tenant bench scenarios land with future
multi-tenant integration work.

OTLP-LogsData corpus support is a future PR. The loader
abstraction in `corpus.rs` exposes
`load_corpus(path: &Path) -> impl Iterator<Item = OtlpLogRecord>`;
the v1 implementation routes `*.txt` files through the
plain-text path. The follow-up that decodes `*.json` /
`*.binpb` `LogsData` files swaps the implementation behind the
same interface without touching the harness.

### 3.4 Measurement methodology

The **load-bearing section of this RFC**. The §5 acceptance
criteria assert each formula and the §9 status line cites this
section by sub-heading.

#### 3.4.1 A1 — Compression ratio

Per `docs/benchmarks.md` §2 / A1, the formula is:

```text
ourios_ratio = bytes(raw_corpus) / bytes(ourios_output)
zstd_ratio   = bytes(raw_corpus) / bytes(zstd_corpus)
A1_delta     = ourios_ratio / zstd_ratio
```

Targets: `A1_delta ≥ 3.0` on every corpus in `benchmarks.md`
§1; `≥ 10.0` on well-templated services.

Pinned definitions:

- **`bytes(raw_corpus)`**: sum of `std::fs::metadata(p).len()`
  for every `*.txt` file in the corpus directory the bench was
  invoked against. UTF-8 encoded; trailing newlines included.
  No transformation: this is the byte count an operator
  measures with `du -b testdata/corpus/`.
- **`bytes(ourios_output)`**: sum of
  `std::fs::metadata(p).len()` for every `*.parquet` file
  under the bench's output bucket directory, **including the
  audit-event file series** (`audit/...`). The audit stream is
  part of what Ourios stores about the corpus — excluding it
  would understate the on-disk footprint and inflate the
  ratio. The pre-rename `*.parquet.tmp` files are skipped (the
  writer's atomic-publish contract per RFC 0005 §7 means an
  open `*.parquet.tmp` indicates an in-flight write, not a
  durable artefact).
- **`bytes(zstd_corpus)`**: sum of `std::fs::metadata(p).len()`
  for every `*.zst` file produced by running
  `zstd -19 --no-progress` against each input `*.txt`
  file individually. Level 19 (not 3) matches the Drain
  paper's published comparison and is the strictest competent
  byte codec; using ZSTD-3 would make Ourios's A1 trivially
  pass and is dishonest. The `--no-progress` flag suppresses
  the progress bar so the bench is deterministic on
  reinvocation.
- **`A1_delta`** is the ratio of ratios; it has no units.
  Reported to three significant figures (`3.21×`,
  `12.4×`, etc.) and rounded *down* to that precision so
  reported numbers err pessimistic.

The bench logs `bytes(raw_corpus)`, `bytes(ourios_output)`,
`bytes(zstd_corpus)`, `ourios_ratio`, `zstd_ratio`, and
`A1_delta` for each corpus directory it processes. The §9
table summarises by corpus name + hardware kind.

#### 3.4.2 C1 — Bit-identical reconstruction rate

Per `docs/benchmarks.md` §4 / C1, the formula is:

```text
C1 = count(records WHERE !lossy_flag AND reconstruct == bytes)
   / count(records WHERE !lossy_flag)
```

Target: **`C1 = 1.000`** (100.000%) on every corpus.
`lossy_flag = true` rows are *excluded from both numerator
and denominator* — that's the definition of "non-lossy
reconstruction rate". A non-lossy row that reconstructs wrong
is a `CLAUDE.md` §3.3 violation and a blocker per §4 /
benchmarks.md C1; the bench reports such rows as a hard
failure (non-zero exit code) rather than a degraded gate.

Pinned definitions:

- **`reconstruct(record)`** is the function exposed by
  `ourios_miner::reconstruct::reconstruct` (the same one
  RFC 0001 §6.6 specifies and `crates/ourios-miner/tests/
  corpus.rs` already exercises at unit scale via H7.1).
- **`bytes`** is the original line bytes the loader handed
  `MinerCluster::ingest`, captured by the harness alongside
  each `MinedRecord`. The bench MUST capture the input line
  *before* `MinerCluster::ingest` borrows or transforms it;
  the comparison happens against the exact bytes the miner
  saw.
- **Equality** is byte-for-byte `==` between
  `reconstruct(record).as_bytes()` and the captured line
  bytes. No trailing-newline normalisation, no case folding,
  no whitespace trimming.
- Reported as a fraction with **six** decimal places
  (`1.000000` / `0.999998`). C1's `100.000%` target makes
  three-decimal precision insufficient — a single failing
  reconstruction out of 100 000 records is the difference
  between green and a blocker.

The bench also reports `lossy_flag_ratio = count(lossy=true) /
count(all)` as a quality signal per benchmarks.md C1, with the
≤ 5% / ≤ 20% targets surfaced but **not** gating.

#### 3.4.3 C2 — Template-count convergence

Per `docs/benchmarks.md` §4 / C2, the gate is "template count
grows sub-linearly and plateaus within 2× of its steady-state
value by 1 M lines". The formula needs three things pinned:
**when** to sample, **what** counts as plateau, and **what**
counts as "steady-state value".

The benchmarks.md C2 phrasing —
*"template count grows sub-linearly and plateaus within 2× of
its steady-state value by 1 M lines"* — operationalises to:
**at the 1 M-line mark, the template count is at least half
of the count the curve eventually converges to**. Since
template count is monotonic non-decreasing (the miner does not
unmerge templates), this is the cleanest formulation; if
`count(1M) ≥ SS / 2`, the curve cannot have more than doubled
between 1 M lines and end-of-corpus, i.e. it is within 2× of
its steady-state value. The phrasing reading where SS is
defined as `max(samples)` and the comparison is
`plateau_value ≤ 2 × max` is tautological — `plateau_value ≤
max` by definition — and was rejected after the first
copilot review of this RFC.

Pinned definitions:

- **Sample cadence**: every `N` lines, where
  `N = max(1, lines_in_corpus / 1024)`. The cadence is
  corpus-relative so the convergence curve has the same
  resolution (~1024 samples) regardless of corpus size; a
  1 M-line corpus samples every 977 lines, a 10 k-line corpus
  samples every 10 lines.
- **Steady-state value (SS)**: the template count at the
  **last** sample (line index = `total_lines - 1` rounded to
  the nearest sample). Operationally, "where the curve ended
  up". Not the running max — see the rationale paragraph above.
- **Count at 1 M lines**: the template count at the sample
  whose line index is closest to `1_000_000`. Defined only on
  corpora of `≥ 1_000_000` lines.
- **Convergence ratio**: `count_at_1m / SS`. By monotonicity,
  this lives in `(0.0, 1.0]`.
- **Pass condition** (gate): `convergence_ratio ≥ 0.5` on a
  corpus of `≥ 1_000_000` lines. This is the "within 2× of
  SS by 1 M lines" rule. Corpora smaller than 1 M lines are
  recorded as `c2.pass = null` (insufficient data); the §9
  row notes the corpus size and the gate is not asserted.
- **Plateau-detection diagnostic** (not a gate): the curve
  is "plateaued" at the sample where the trailing `K = 64`
  samples all lie within `± 5%` of the SS. The diagnostic is
  useful for understanding where the curve actually flattens
  (often well before 1 M lines), but it does not gate the
  RFC — the gate is the 2× rule above.

Reported as: `template_count_at_1m_lines` (integer; `null` for
corpora < 1 M lines), `template_count_at_end` (integer;
this is SS), `convergence_ratio` (three-decimal float; `null`
for short corpora), `pass` (bool or `null`),
`corpus_at_least_1m` (bool).

v1 records the convergence curve in the results JSON (as
`c2.convergence_curve`, an array of `{"lines": N,
"template_count": M}` objects at the sample cadence) but does
not plot it. A future RFC may add a plot artefact so the §9
Results section can include visualisations.

### 3.5 Hardware baseline and annotation

`docs/benchmarks.md` §1 pins the hardware baseline: "commodity
cloud VM, 8 vCPU, 32 GiB RAM, gp3-class SSD." Every bench run
captures the host's `--hardware-kind=<tag>` CLI argument
(required; defaults to `unknown` only when explicitly opted in
via `--allow-unknown-hardware`) and writes it into the results
JSON. The §9 Results table cites the hardware tag on every
row; a comparison across rows with different tags is a delta
between hardware *and* code, not code alone.

Hardware tags this RFC pins as known: `baseline-8vcpu-32gib`
(the §1 reference), `dev-laptop`, `ci-runner`. New tags can be
added without an RFC amendment — the value is operator
discipline, not a closed vocabulary — but unknown tags require
the explicit `--allow-unknown-hardware` opt-in so a forgotten
`--hardware-kind` doesn't silently land in §9 as `unknown`.

### 3.6 Result format

Each bench invocation writes one results JSON to:

```text
benchmarks/results/<UTC-RFC3339-ms>-<git-sha7>.json
```

The timestamp is RFC3339 with millisecond precision (e.g.
`2026-05-22T14:30:00.123Z`) so two runs on the same commit
within the same wall-clock second produce different filenames.
Even at millisecond precision two runs *can* theoretically
collide on a fast machine; the bench detects the conflict at
write time and retries with the next millisecond's timestamp,
emitting a warning to stderr. The directory `benchmarks/` will be created at the repo root by
the implementation PR that lands the `ourios-bench` crate. That
same PR adds a `.gitignore` entry ignoring `benchmarks/results/`
except for a `.gitkeep` and the specific runs the maintainer
chooses to commit (the §9 Results section then cites those by
file path).

The JSON shape is pinned by `report::ResultsFile` and looks
like:

```json
{
  "rfc": "RFC 0006",
  "rfc_version": "v1",
  "timestamp": "2026-05-22T14:30:00.123Z",
  "git_sha": "abc1234",
  "hardware_kind": "baseline-8vcpu-32gib",
  "corpus": {
    "directory": "testdata/corpus/",
    "total_lines": 12345,
    "total_files": 2,
    "raw_bytes": 1234567
  },
  "ourios": {
    "parquet_bytes": 89012,
    "audit_bytes": 1024
  },
  "zstd": {
    "level": 19,
    "compressed_bytes": 312345
  },
  "a1": {
    "ourios_ratio": 13.6,
    "zstd_ratio": 3.95,
    "delta": 3.44,
    "target_delta": 3.0,
    "pass": true
  },
  "c1": {
    "non_lossy_total": 12000,
    "non_lossy_reconstruct_ok": 12000,
    "rate": 1.000000,
    "lossy_flag_ratio": 0.0279,
    "pass": true
  },
  "c2": {
    "sample_cadence": 12,
    "total_lines": 1234567,
    "template_count_at_1m_lines": 142,
    "template_count_at_end": 145,
    "convergence_ratio": 0.979,
    "convergence_curve": [
      {"lines": 100000, "template_count": 98},
      {"lines": 200000, "template_count": 121}
    ],
    "pass": true,
    "corpus_at_least_1m": true
  }
}
```

The temp-directory paths the bench actually uses (the
`Writer`'s bucket root) are intentionally **not** in the
JSON. They're an implementation detail that differs across
runs and would otherwise break the §5 RFC0006.7 reproducibility
scenario. The byte counts are what downstream analysis cares
about; the paths are debug-only and logged to stderr when
`--keep-parquet` is passed.

`rfc_version` is a literal `"v1"` and tracks RFC 0006
amendments; bumping it requires an RFC amendment, and downstream
analysis tooling refuses unknown versions with a hard error.
This is the bench's own forward-compatibility policy — the
results JSON is a closed schema, unlike RFC 0005 §3.9's Parquet
reader which ignores unknown columns and surfaces unknown
ordinals as `ParamType::Unknown`.

A human-readable summary is appended to `docs/benchmarks.md`
§9 as a sub-heading per run, with the same numbers in a
markdown table. Repeated bench runs on the same `(git-sha,
hardware-kind)` pair update the existing sub-heading rather
than appending duplicates — the bench reads the §9 section,
finds the matching heading, and rewrites it in place.

### 3.7 Invocation

The CLI has two output-path concepts and they are spelled
differently to avoid the §3.4.1 "output bucket directory"
ambiguity:

- **`--results-dir`** is where the JSON results file from
  §3.6 lands. Default: `benchmarks/results/`.
- **`--bucket-dir`** is the `bucket_root` passed to the
  `ourios-parquet` writer — the directory the writer's
  `data/` and `audit/` partition trees grow under, and
  whose total byte size is `bytes(ourios_output)` in the
  §3.4.1 A1 formula. Default: a fresh temp dir under
  `std::env::temp_dir()` per invocation, cleaned up on exit
  unless `--keep-parquet` is passed.

CLI (`crates/ourios-bench/src/main.rs`):

```text
ourios-bench [--corpus <path>]
             [--results-dir <path>]
             [--bucket-dir <path>]
             [--keep-parquet]
             [--hardware-kind <tag>]
             [--allow-unknown-hardware]
             [--update-benchmarks-md]
             [--gates a1,c1,c2]
```

Flags:

- `--corpus <path>` (default `testdata/corpus/`): directory of
  `*.txt` files the bench loads.
- `--results-dir <path>` (default `benchmarks/results/`):
  where the §3.6 JSON file lands.
- `--bucket-dir <path>` (default: fresh temp dir): the
  Parquet writer's `bucket_root`. Cleaned up on exit unless
  `--keep-parquet` is passed.
- `--keep-parquet` (off by default): suppress the temp-dir
  cleanup so the Parquet partition tree is inspectable after
  the bench exits. Path is logged to stderr.
- `--hardware-kind <tag>` (required unless
  `--allow-unknown-hardware`): the §3.5 annotation.
- `--update-benchmarks-md` (off by default): append / rewrite
  the §9 sub-heading. CI runs without this flag; maintainers
  invoke with it to commit numbers.
- `--gates a1,c1,c2` (default all): comma-separated subset of
  gates to compute. Useful when iterating on a single
  measurement.

Adds a `just bench` recipe wrapping `cargo run -p
ourios-bench --release --`. The `--release` is normative — A1
on a debug-mode writer would understate compression because
debug builds disable some `arrow` / `parquet` optimisations
the release writer relies on.

CI cadence: not on every PR — too slow for the per-PR loop and
hardware-dependent in ways that would generate noise. The
bench runs on demand (PR comment `/bench`, future workflow)
and on the nightly schedule that `docs/rfcs/0005-parquet-
storage.md` §7's open-question on slow-test CI cadence will
formalise. RFC 0006 does not commit to a CI cadence — that's
the open question's domain.

## 4. Alternatives considered

### 4.1 `criterion` instead of a custom harness

`criterion` is the standard Rust micro-benchmarking framework
and `CLAUDE.md` §6.2 names it for the project's hot-path
benchmarks. Rejected for the thesis-gate harness: `criterion`
is statistically tuned for sub-microsecond function-level
measurements (per-iteration noise estimation, warmup loops,
bootstrapped confidence intervals), which is the wrong tool
for "ingest a 1 M-line corpus, write a Parquet partition, then
divide two file-tree sizes." The bench *also* runs `criterion`
benchmarks under `crates/ourios-bench/benches/` for the
per-line miner cost and the per-batch writer cost — but that's
a follow-up PR after the thesis-gate harness lands, not the
v1 shape.

### 4.2 Bench inside `ourios-parquet` as an `[[example]]`

A Cargo `[[example]]` under `crates/ourios-parquet/examples/`
could drive the writer + reader without a new crate. Rejected:
the bench needs the miner *and* the writer plus a custom
result-file writer; living under `ourios-parquet` would either
add a `ourios-miner` dependency to the storage crate
(architecturally wrong — storage has no business knowing about
template mining) or grow into a binary that's not really an
"example" anymore. The dedicated crate matches the
`docs/roadmap.md` §4 Phase 3 layout.

### 4.3 Quote A1 against the LogPAI corpora only

The Drain paper measures on LogPAI's HDFS / BGL / Spark /
Apache / OpenSSH / Windows corpora; we could pin A1 to the
same corpora exclusively and call any other corpus a "tuning"
measurement. Rejected: `docs/benchmarks.md` §1 already
commits to "every corpus in §1", including the self-collected
archetypes. Restricting v1 to LogPAI would leave the
self-collected work unmeasured and reintroduce the "we never
ran the bench on the data that matters" gap §1 is designed
to close. v1 measures on whatever corpora are committed; the
seed corpus is the floor, and additions are additive.

### 4.4 ZSTD level 3 for the reference

ZSTD-3 is the codec the writer itself uses per
RFC 0005 §3.5. Using ZSTD-3 *also* as the A1 reference would
make `ourios_ratio / zstd_ratio` an apples-to-apples
codec-vs-codec comparison instead of a structure-vs-codec one
(both sides use the same compressor; Ourios's win is purely
the template-mining pillar). Rejected because:

- The Drain paper compares against the strongest competent
  byte codec, and that's ZSTD-19 / level-max. Using ZSTD-3
  understates the codec's reachable ratio and inflates
  Ourios's A1 win.
- `CLAUDE.md` §1's central claim is "Parquet + template
  mining + DataFusion collapses [the layers]"; that claim is
  about the whole stack, not just the template-mining pillar.
  The reference should be the strongest *alternative*, not
  the same codec Ourios uses internally.

The downside — losing the codec-vs-codec isolation — is
captured as an open question (§7). A future RFC may add `A1'`
(prime, "codec-isolated") as an additional tuning-goal
measurement alongside the thesis-gate A1.

### 4.5 Defer the bench to after the corpus migration

The roadmap names "OTLP `LogsData` corpus" as the Phase 3 goal
and one could argue the bench should not land until the corpus
is in its target shape. Rejected: A1 / C1 / C2 are well-defined
on plain-text input today (the seed corpus is plain text and
the unit-scale H7.1 test already runs against it). Waiting on
the OTLP migration to produce A1 / C1 / C2 numbers couples a
mechanical loader change to a measurement deliverable for no
real reason. The bench's `corpus.rs` exposes the loader as an
abstraction so the OTLP migration drops in without touching
the harness or the formulas.

## 5. Acceptance criteria

> **Scenario RFC0006.1 — A1 formula is well-defined on the seed corpus**
> - **Given** the bench is invoked with `--corpus testdata/
>   corpus/`, the writer ships with the §3.5 / §3.6 RFC 0005
>   encoding policy, and a ZSTD level-19 reference
>   implementation is available (shell-out vs `zstd_safe` — §7
>   open question)
> - **When** the bench runs the A1 measurement
> - **Then** `bytes(raw_corpus)` equals
>   `sum(std::fs::metadata(f).len())` over the `*.txt` files
>   in the corpus directory
> - **And** `bytes(ourios_output)` equals the sum of all
>   `*.parquet` (not `*.parquet.tmp`) file sizes under the
>   bench's output bucket, including the `audit/...` partition
> - **And** `bytes(zstd_corpus)` equals the sum of
>   `std::fs::metadata(f).len()` over the `*.zst` files
>   produced by `zstd -19 --no-progress` on each input
> - **And** the reported `delta` equals
>   `ourios_ratio / zstd_ratio`, rounded down to three
>   significant figures

> **Scenario RFC0006.2 — C1 = 100% on the seed corpus, mismatch is a hard failure**
> - **Given** the bench is invoked with the seed corpus
>   committed under `testdata/corpus/`
> - **When** the bench runs the C1 measurement
> - **Then** `non_lossy_reconstruct_ok / non_lossy_total =
>   1.000000` (six-decimal precision)
> - **And** the results JSON records `c1.pass = true`
> - **And** if any non-lossy row has `reconstruct(record) !=
>   ingested_bytes`, the bench writes the failing row's
>   `template_id`, `template_version`, expected bytes, and
>   actual reconstruction to stderr and exits with non-zero,
>   and the results JSON records `c1.pass = false`
> - **And** the bench writes the results JSON irrespective of
>   `--update-benchmarks-md` — the JSON file always lands;
>   only the `docs/benchmarks.md` §9 mutation is gated by the
>   flag, so a failure run still leaves a machine-readable
>   record on disk

> **Scenario RFC0006.3 — C2 gate ("within 2× of SS by 1 M lines") on a stable corpus**
> - **Given** a synthetic stable corpus of `≥ 1_000_000` lines
>   whose template alphabet is bounded (constructed by the
>   bench's integration test; not committed to
>   `testdata/corpus/`)
> - **When** the bench runs the C2 measurement
> - **Then** `c2.corpus_at_least_1m = true`
> - **And** `template_count_at_1m_lines` is the integer
>   sample-count at the sample whose line index is closest to
>   `1_000_000`
> - **And** `template_count_at_end` is the integer sample-count
>   at the final sample (the §3.4.3 SS definition)
> - **And** `convergence_ratio = template_count_at_1m_lines /
>   template_count_at_end ≥ 0.5` — the "within 2× of SS"
>   gate, made non-tautological by defining SS as the
>   end-of-corpus value rather than the running max
> - **And** `c2.pass = true`
> - **And** the convergence curve in the results JSON has
>   exactly the `sample_cadence`-derived number of samples
>   (`total_lines / cadence`, rounded)
> - **And** on a corpus of `< 1_000_000` lines,
>   `c2.corpus_at_least_1m = false`, `c2.pass = null`, and
>   `c2.template_count_at_1m_lines = null` — the gate
>   abstains rather than passing or failing

> **Scenario RFC0006.4 — Result file shape is stable and the §9 update is reversible**
> - **Given** the bench has run and written its results JSON
>   to `benchmarks/results/<...>.json`
> - **When** a downstream consumer (or a future RFC's bench)
>   reads the file
> - **Then** the JSON parses against `report::ResultsFile`
>   with `rfc_version = "v1"`
> - **And** the file contains the §3.6 schema's required keys
>   (`rfc`, `rfc_version`, `timestamp`, `git_sha`,
>   `hardware_kind`, `corpus`, `ourios`, `zstd`, `a1`, `c1`,
>   `c2`)
> - **And** when `--update-benchmarks-md` is passed and the
>   §9 section already contains a sub-heading for the same
>   `(git_sha, hardware_kind)` pair, the bench rewrites that
>   sub-heading in place — running the bench twice on the
>   same commit / hardware does not duplicate §9 rows

> **Scenario RFC0006.5 — Hardware-kind annotation is required**
> - **Given** the bench is invoked **without** a
>   `--hardware-kind` flag and **without**
>   `--allow-unknown-hardware`
> - **When** the bench parses CLI arguments
> - **Then** the bench exits with a usage error before any
>   measurement runs
> - **And** if `--allow-unknown-hardware` is passed, the
>   resulting JSON carries `hardware_kind = "unknown"` and
>   stderr emits a warning naming the §1 baseline tag for
>   reference

> **Scenario RFC0006.6 — `--gates` flag scopes the measurement**
> - **Given** the bench is invoked with `--gates c1`
> - **When** the bench runs
> - **Then** only the C1 measurement executes; A1 and C2 are
>   skipped
> - **And** the results JSON contains `c1` populated and
>   `a1`, `c2` set to `null`
> - **And** the §9 update path (when `--update-benchmarks-md`
>   is passed) leaves the existing A1 / C2 numbers for the
>   `(git_sha, hardware_kind)` pair untouched

> **Scenario RFC0006.7 — Bench is reproducible across runs**
> - **Given** the bench is invoked twice on the same git
>   checkout and the same corpus, with no code or data changes
>   in between
> - **When** the two runs complete
> - **Then** every measurement field of the results JSON is
>   bit-identical across the two runs — specifically
>   `corpus.raw_bytes`, `corpus.total_lines`,
>   `corpus.total_files`, `ourios.parquet_bytes`,
>   `ourios.audit_bytes`, `zstd.compressed_bytes`, `a1.delta`,
>   `c1.rate`, `c1.non_lossy_total`,
>   `c1.non_lossy_reconstruct_ok`, `c2.template_count_at_end`,
>   and (when the corpus is `≥ 1 M lines`)
>   `c2.template_count_at_1m_lines` / `c2.convergence_ratio`
> - **And** the only fields that legitimately differ are
>   `timestamp` (wall-clock) and the output JSON file's path
>   (derived from `timestamp`). The temp-dir bucket the writer
>   used is **not** in the JSON per §3.6, so it cannot
>   contribute to a spurious diff

## 6. Testing strategy

Per `CLAUDE.md` §6.2 / `docs/verification.md` §2:

- **RFC0006.1** — integration test in
  `crates/ourios-bench/tests/a1.rs`. Calls
  `ourios_bench::run` against a fixture corpus committed under
  `crates/ourios-bench/tests/fixtures/`, captures the
  resulting JSON, and asserts each formula leg
  (`raw_bytes` from `fs::metadata`, `parquet_bytes` from
  inspecting the output bucket, `zstd_bytes` from the
  `zstd_safe` crate or a shell-out — see §7 open question on
  the codec wrapper choice).
- **RFC0006.2** — integration test in
  `crates/ourios-bench/tests/c1.rs`. Drives the bench against
  the seed corpus; asserts `c1.rate == 1.0`. A second
  sub-test injects a synthetic record whose `reconstruct()`
  disagrees with the input (built by hand, not by the miner)
  and asserts the bench exits with a non-zero code and emits
  the mismatch diagnostics to stderr.
- **RFC0006.3** — integration test in
  `crates/ourios-bench/tests/c2.rs`. Builds a synthetic
  corpus in memory (no committed `testdata/`) of 1.5 M lines
  with a known small template alphabet; asserts
  `convergence_ratio ≥ 0.5` and the convergence curve has
  exactly `total_lines / sample_cadence` entries (rounded). A
  second sub-test feeds a non-plateauing corpus (every line
  introduces a new template structure) and asserts
  `c2.pass = false`.
- **RFC0006.4** — colocated unit test in
  `crates/ourios-bench/src/report.rs`. Serialises a
  hand-built `ResultsFile`, parses the JSON back, asserts
  field-by-field equality. A second sub-test exercises the
  in-place §9 update via a temp markdown file.
- **RFC0006.5** — colocated unit test in
  `crates/ourios-bench/src/main.rs` (`#[cfg(test)] mod
  tests`) for the CLI parser. Asserts the missing
  `--hardware-kind` flag without `--allow-unknown-hardware`
  produces a usage error before `Harness::run` is invoked.
- **RFC0006.6** — same test file as RFC0006.5; covers the
  `--gates` filtering.
- **RFC0006.7** — `crates/ourios-bench/tests/
  reproducibility.rs`. Runs the bench twice against a fixed
  fixture corpus and asserts the relevant fields bit-equal.

A criterion bench under `crates/ourios-bench/benches/` is
deferred to a follow-up PR. The thesis-gate harness this RFC
specifies is correctness-first; per-line miner microbenchmarks
are a separate measurement category.

## 7. Open questions

- [ ] **`zstd` integration.** Two options for invoking the
      reference codec: (a) shell out to the system `zstd`
      binary, requiring it on PATH for every bench invocation,
      versus (b) link the `zstd_safe` Rust crate (already in
      the dep tree via `parquet`'s `zstd` feature). Option (b)
      is more reproducible (no dependency on the host `zstd`
      version) but adds a wrapper layer between the formula
      and the published "ZSTD-19" tag — a slight Drain-paper
      apples-to-apples concern. Resolution before `green`
      stage.
- [ ] **Convergence curve plotting.** The results JSON
      carries the full sample series. Should the §9
      sub-heading also render a tiny SVG / ASCII plot of the
      C2 curve, or is the curve only for downstream analysis?
      Defer until at least one real run exists.
- [ ] **CI cadence.** When (or whether) the bench runs on a
      `schedule:` trigger is the RFC 0005 §7 open question on
      slow-test CI cadence. This RFC inherits the question;
      resolution is the workflow PR that lands the cadence.
- [ ] **Result-file retention policy.** `benchmarks/
      results/*.json` will be gitignored by default (§3.6);
      specific runs are committed when the maintainer cites
      them in §9.
      Open: should there be a `benchmarks/results/baseline/`
      sub-directory whose contents are *always* committed, so
      regression detection has a stable reference even when
      the §9 markdown is hand-pruned?
- [ ] **Out-of-tree corpora.** A `--corpus <external-path>`
      invocation against, say, a downloaded LogPAI corpus
      runs but the results JSON points to a path the repo
      doesn't carry. Should the JSON record a content hash of
      the corpus directory (`sha256` of the concatenated
      files) so future readers can verify they're comparing
      against the same input? Probably yes; defer the
      mechanics until at least one out-of-tree corpus is
      actually being measured.

## 8. References

- `CLAUDE.md` §1 (project charter), §2 (pillars), §3.3
  (bit-identical reconstruction), §6.2 (testing discipline),
  §10 (`docs/hazards.md` reading rule).
- `docs/benchmarks.md` §1 (corpora + methodology), §2 (A1),
  §4 (C1, C2), §7 (thesis-gate summary), §9 (Status).
- `docs/roadmap.md` §4 Phase 3 (bench + querier scope), §5
  (deferred capabilities).
- `docs/rfcs/README.md` (RFC process and maturity model).
- `docs/rfcs/0001-template-miner.md` §6.6 (`reconstruct`),
  §6.4 (audit-event contract that C2's plateau exercises).
- `docs/rfcs/0005-parquet-storage.md` §3.5 (row-group sizing
  the A1 measurement implicitly depends on), §3.6 (encoding
  policy that affects compressed bytes), §7 (open question on
  slow-test CI cadence inherited here).
- `docs/verification.md` §2 (scenario-id greppability
  convention), §3 (maturity-stage gates).
