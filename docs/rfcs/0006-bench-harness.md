---
rfc: 0006
title: Bench harness — A1 / C1 / C2 thesis-gate measurement
status: green
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
DataFusion querier (`ourios-querier`, RFC 0007) and therefore
landed in follow-up extensions once the querier was live — both are now
measured authoritatively (`docs/benchmarks.md` §9.4; RFC 0007
is `validated`).

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
no shared code path and lives in RFC 0007 (since shipped and
`validated`).

## 3. Proposed design

### 3.1 Scope and what this RFC pins

This RFC pins:

- The `ourios-bench` crate's shape: a binary plus a small set
  of supporting modules (corpus loader, ingest harness, result
  writer).
- The corpus input format for v1 — plain-text `*.txt` files
  under `testdata/corpus/`, one line per row, UTF-8, the same
  on-disk shape `testdata/corpus/README.md` documents and the
  existing H7.1 property test in
  `crates/ourios-miner/tests/hazards.rs` reads. The bench
  reuses the on-disk *format*, not the test's `OtlpLogRecord`
  fixture defaults (see §3.3 for the bench-specific
  tenant / severity / scope envelope). **Amendment (PR-K2,
  2026-05-28):** the OTLP-LogsData migration has landed — the
  loader also reads `*.jsonl` / `*.json` files in the OTel
  File Exporter format (one `LogsData` per line). The
  measurement formulas are unchanged; the §3.3 plain-text
  envelope defaults still apply to text input.
- The A1 / C1 / C2 measurement formulas — what is divided by
  what, where the byte counts come from, what equality means.
- The "hardware baseline annotation" rule: every result line
  carries the machine kind the measurement ran on so deltas
  across hardware classes don't masquerade as code
  regressions.
- The output format: a per-run JSON results file under
  `benchmarks/results/<UTC-RFC3339-ms-colon-free>-<git-sha7>.json`
  (filename colons replaced by `-`; see §3.6), and a
  human-readable summary appended to `docs/benchmarks.md` §9
  under a date-stamped sub-heading.
- The invocation surface: `cargo run -p ourios-bench --` or
  `just thesis-bench`, with CLI flags pinning corpus
  selection, result-file output, and the optional
  "annotate-only mode" that runs measurements but does not
  write `benchmarks.md`. The recipe is *not* named
  `just bench` — that name is already taken by the
  criterion-micro-benchmark recipe in `justfile`; see §3.7.

This RFC does **not** pin:

- `B1` / `B2` measurement — both need
  `ourios-querier` (RFC 0007, where they since landed;
  authoritative results in `docs/benchmarks.md` §9.4).
- ~~The OTLP-LogsData corpus migration (`docs/roadmap.md`
  §4's "OTLP `LogsData` (canonical JSON or protobuf)"
  goal).~~ **Landed in PR-K2 (2026-05-28).** The loader now
  reads `*.jsonl` / `*.json` files in the OTel File Exporter
  format alongside the plain-text `*.txt` path. As predicted,
  the measurement formulas were unchanged; only the loader's
  parse step grew. The follow-up that swaps in the protobuf
  (`*.binpb`) `LogsData` decode (instead of JSON) remains
  out of scope for this RFC.
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
    │                  # tests/hazards.rs but factored for reuse)
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
applies to it only via the `report::ResultsFile` shape under
`benchmarks/results/<...>.json`.

### 3.3 Corpus format

For v1, the bench reads plain-text `*.txt` files under
`testdata/corpus/` per the format convention in
`testdata/corpus/README.md` (one log line per row, UTF-8, empty
rows skipped). Each non-empty line becomes one `OtlpLogRecord`
with `Body::String(line)`, a default tenant (`bench-tenant`),
severity (`9` / `INFO`), and scope (`None` / `None`); the
in-memory shape matches what `MinerCluster::ingest` expects
for `body_kind = String` records.

The bench reuses the same corpus files and one-line-per-record
shape as the H7.1 loader in
`crates/ourios-miner/tests/hazards.rs`, but intentionally
differs on pipeline defaults: H7.1 uses tenant `"corpus"` and
severity `0` (unspecified), while the bench uses `"bench-tenant"`
and severity `9` (`INFO`). The divergence is deliberate —
H7.1 exercises the miner's body-reconstruction invariant where
tenant/severity are irrelevant, whereas the bench exercises
the full write path where a realistic severity aids coverage of
the Parquet writer's field encoding. Both loaders produce
`Body::String` records from the same `*.txt` files; they are
not code-shared because their purposes and default-filling
strategies differ.

Time stamps for the synthesised records are deterministic:
`time_unix_nano` starts at a fixed RFC 0005-friendly baseline
(`1_775_127_480_000_000_000`, i.e. 2026-04-02T10:58:00 UTC,
matching the existing test fixtures) and advances by a fixed
1 ms per line. The advancement is artificial; this RFC accepts
the artificiality because A1 / C1 / C2 are time-insensitive (no
gate measures throughput or query latency against a time
range). The RFC 0007 measurement extension to B1/B2 revisited
time-stamp synthesis as anticipated, since predicate-pushdown
latency depends on the time-range distribution: the B1/B2
real-corpus arms window on the records' real timestamps.

The default tenant means every record lands in the same
partition. This is a simplification — the writer's atomic
publish, row-group rotation, and §3.9 row-vs-path contract
have all been exercised on the multi-partition path through
PR-E2 / PR-F / PR-G. A1 / C1 / C2 are tenant-distribution
neutral. Multi-tenant bench scenarios land with future
multi-tenant integration work.

**Amendment (PR-K2, 2026-05-28):** OTLP-LogsData corpus
support has landed. The loader dispatches on file extension:
`*.txt` → the plain-text path above; `*.jsonl` / `*.json` →
OTLP/JSON Lines (one `LogsData` per line, the OTel File
Exporter format), parsed via `serde_json::from_str` against
the `opentelemetry-proto` types (the `with-serde` feature
gives the OTLP/JSON spec mapping for free). Each wire
`LogRecord` maps to one `OtlpLogRecord` per the RFC 0003 §6.6
shape — severity (clamped to OTLP's `0..=24`), scope,
attributes, resource attributes (copied per record), trace
context (length-validated `[u8;16]` / `[u8;8]`), body
(`StringValue` → `Body::String`, anything else →
`Body::Structured(AnyValue)` per RFC 0003 §6.4). Both formats
may coexist in the same corpus directory. Wire timestamps are
honoured for OTLP records (file-static = run-reproducible);
the §3.3 deterministic baseline still drives the text path.
The follow-up that decodes the protobuf (`*.binpb`)
`LogsData` form remains out of scope for this RFC.

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
  for every corpus file the loader consumed — recursively
  under the corpus directory. The loader dispatches on
  extension: `*.txt` (the §3.3 plain-text path), or `*.jsonl`
  / `*.json` (the §3.1 OTLP/JSON Lines path landed in PR-K2).
  `*.binpb` protobuf-encoded `LogsData` is reserved for a
  future follow-up and not counted today. No transformation:
  this is the byte count an operator measures with `find
  testdata/corpus/ \( -iname '*.txt' -o -iname '*.jsonl' -o
  -iname '*.json' \) -exec stat --printf='%s\n' {} + | awk
  '{s+=$1}END{print s}'` (or the platform equivalent). For
  OTLP/JSON corpora the byte count includes the envelope
  (camelCase keys, base64 bytes), so A1 ratios are not
  directly comparable across formats — a directory holding a
  mix of `*.txt` and `*.jsonl` produces one aggregate number
  that conflates both encodings. The §3.6 results JSON's
  `corpus.directory` field lets consumers locate the corpus
  and inspect its contents to interpret the result; cleanly
  comparable runs need a corpus directory of one encoding
  only. A per-format byte breakdown on the results JSON is a
  future enhancement (see §9 open questions).
  `bytes(zstd_corpus)` (below) covers the same extension set
  so the §3.4.1 math invariant — both sides processing the
  same input — holds across formats.
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
  `zstd -19 --no-progress` against each consumed input file
  individually — same extension set as `bytes(raw_corpus)`
  above (`*.txt` + `*.jsonl` + `*.json`). The two byte
  counts must cover identical input files; broadening one
  without the other would break the §3.4.1 math invariant
  (zero `bytes(zstd_corpus)` on an OTLP-only corpus would
  produce `zstd_ratio = 0` and an undefined A1 delta). Level
  19 (not 3) matches the Drain paper's published comparison
  and is the strictest competent byte codec; using ZSTD-3
  would make Ourios's A1 trivially pass and is dishonest.
  The `--no-progress` flag suppresses the progress bar so
  the bench is deterministic on reinvocation.
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

**Amendment (PR-K4, 2026-05-29):** `BodyKind::Structured`
rows are *also* excluded from the C1 denominator. Per RFC
0001 §6.4 / RFC 0003 §6.4, reconstruction for structured
bodies is a storage-layer round-trip (decode the stored
`AnyValue` bytes) — not a template + params reconstruction —
so the template-based equality C1 measures doesn't apply to
them. Structured ≠ lossy (the two are independent axes; a
structured record can be high-confidence). The harness
symmetrically skips the `templates_for()` snapshot lookup for
those records, because RFC 0001 §6.1 assigns them a sentinel
template id outside the Drain tree (no leaf to find).

Pinned definitions:

- **`reconstruct(record, template)`** is the function exposed
  by `ourios_miner::reconstruct::reconstruct`, signature
  `fn reconstruct(record: &MinedRecord, template: &[OwnedToken])
  -> Vec<u8>` — same function RFC 0001 §6.6 specifies and the
  H7.1 property test in `crates/ourios-miner/tests/hazards.rs`
  already exercises at unit scale. The function takes the
  emitted record **and** the leaf's template token slice at
  the record's emit-time `(template_id, template_version)`;
  template snapshots have to be captured separately because a
  later attach can widen the same leaf and rewrite the live
  template.
- **Template-snapshot capture** mirrors the H7.1 pattern:
  after each `MinerCluster::ingest`, the harness walks
  `cluster.templates_for(tenant)` and records the current
  template tokens into a `HashMap<(template_id,
  template_version), Vec<OwnedToken>>` via `or_insert_with`
  (so the first observation of a `(id, v)` pair wins and
  later widenings produce `(id, v+1)` entries without
  clobbering). At C1 evaluation time, each record's
  `(template_id, template_version)` looks up its
  emit-time-active snapshot. A record whose key is not in the
  map is a contract violation — the harness exits with
  non-zero before reporting C1.
- **`bytes`** is the original line bytes the loader handed
  `MinerCluster::ingest`, captured by the harness alongside
  each `MinedRecord`. The bench MUST capture the input line
  *before* `MinerCluster::ingest` borrows or transforms it;
  the comparison happens against the exact bytes the miner
  saw.
- **Equality** is byte-for-byte `==` between
  `reconstruct(record, template)` (a `Vec<u8>`) and
  `line.as_bytes()`. No trailing-newline normalisation, no
  case folding, no whitespace trimming.
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
  `N = max(1, ceil(lines_in_corpus / 1024))`. The cadence
  uses ceiling division so the curve never exceeds 1024
  samples regardless of corpus size; a 1 M-line corpus
  samples every 977 lines, a 10 k-line corpus samples every
  10 lines. **Sampling indices**: the curve records
  template count after processing line indices
  `N-1, 2N-1, 3N-1, …` (i.e. after every N-th line,
  zero-indexed). The final sample is always taken at
  `total_lines - 1` (the last line), regardless of whether
  it falls on a cadence boundary. The sample count is
  therefore `ceil(total_lines / N)` — at most 1024 entries.
- **Steady-state value (SS)**: the template count at the
  **last** sample (line index = `total_lines - 1`; always
  included by the final-sample rule above).
  Operationally, "where the curve ended
  up". Not the running max — see the rationale paragraph above.
- **Count at 1 M lines**: the template count at the sample
  whose line index is closest to `999_999` (the millionth
  line, zero-indexed). When two samples are equidistant, the
  earlier one wins (floor tie-break). Defined only on
  corpora of `≥ 1_000_000` lines.
- **Convergence ratio**: `count_at_1m / SS`. By monotonicity,
  this lives in `(0.0, 1.0]`.
- **Pass condition** (gate) — **per service** (amended for
  #444, maintainer-approved 2026-07-10): C2 is defined over
  "a corpus from a single **stable service**", so on a
  multi-service corpus the gate is evaluated **per
  `service.name`**, not on the whole corpus. Each service's
  ratio is `count_at_1m / SS` over *that service's* lines
  (template creation is a globally-monotonic event attributed
  to the minting service, so per-service creations partition
  the whole-corpus template count exactly). A corpus **passes**
  iff every service with `≥ 1_000_000` lines has ratio `≥ 0.5`;
  it **fails** if any such service is below `0.5`; it **abstains**
  (`c2.pass = null`) when no service reaches 1 M lines. A
  single-service corpus — including the plain-text `<unknown>`
  bucket (no `service.name`) — collapses to the whole-corpus
  verdict, so every historical text-corpus row is unchanged;
  only multi-service OTLP corpora differ. **Rationale**:
  running one whole-corpus ratio over a multi-service capture
  (e.g. the OTel-Demo) is a category error — it conflates a
  noisy infra service (a broker emitting high-cardinality
  offset/path tokens) with clean application services, so the
  whole-corpus number fails even when every application service
  converges perfectly (v8 §9.12). The whole-corpus
  `convergence_ratio` is retained as a **diagnostic** (the
  `by_service` breakdown is the gate basis). Note: token-level
  polishing of high-cardinality infra logs is an OTel Collector
  concern (a `transform`/`redaction` processor upstream), not
  the miner's — consistent with "format parsing is the
  Collector's job".
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
benchmarks/results/<UTC-RFC3339-ms-colon-free>-<git-sha7>[-N].json
```

The name embeds the run's millisecond-precision RFC3339
timestamp with the `:` separators replaced by `-` (so
`2026-05-22T14:30:00.123Z` becomes
`2026-05-22T14-30-00.123Z`). The colon substitution is
required: `:` is illegal in filenames on Windows and awkward
for shell / tooling elsewhere, so the on-disk *name* is
colon-free even though the `timestamp` **field inside** the
JSON keeps canonical RFC3339 (colons included). Two runs on
the same commit in the same wall-clock second still produce
distinct names via the millisecond component.

Even at millisecond precision two runs *can* theoretically
collide on a fast machine. The writer creates each candidate
with an atomic `create_new` ("create iff absent") open and,
on `AlreadyExists`, appends a numeric suffix (`-1`, `-2`, …)
until it finds a free name — rather than re-deriving the
timestamp. This closes the check-then-write race against a
concurrent run and never clobbers an existing file; if the
suffix budget is exhausted the write fails loudly rather than
overwriting. The directory `benchmarks/` will be created at the repo root by
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
    "total_lines": 1234567,
    "total_files": 2,
    "raw_bytes": 98765432
  },
  "ourios": {
    "data_parquet_bytes": 56789,
    "audit_parquet_bytes": 1024,
    "total_parquet_bytes": 57813
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
    "sample_cadence": 1206,
    "total_lines": 1234567,
    "template_count_at_1m_lines": 142,
    "template_count_at_end": 145,
    "convergence_ratio": 0.979,
    "convergence_curve": [
      {"lines": 1206, "template_count": 14},
      {"lines": 2412, "template_count": 27}
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
`--keep-parquet` is passed. The field relationship:
`total_parquet_bytes = data_parquet_bytes + audit_parquet_bytes`,
and `total_parquet_bytes` is the value §3.4.1 calls
`bytes(ourios_output)`. `data_parquet_bytes` is the sum of
`*.parquet` sizes under `data/…`; `audit_parquet_bytes` is the
sum under `audit/…`. The split is recorded for diagnostic
transparency (understanding how much of the footprint is audit
overhead) but the A1 formula operates on the total.

**Gate sections are nullable.** The `a1`, `c1`, and `c2` keys
are always present at the top level but their values are
`null` when the corresponding gate is skipped (via
`--gates` per §3.7) or abstains (e.g. `c2` on a corpus of
`< 1 M lines` — see §3.4.3). The example above shows all
three populated (the "all gates ran, all gates pass" case);
a `--gates c1` run produces `"a1": null, "c2": null` while
`"c1": { ... }` carries the populated payload. Downstream
analysis MUST handle the `null` case (rather than assuming
the object shape) — the §5 RFC0006.6 scenario asserts the
behaviour.

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
  corpus files the loader walks recursively. Files are
  dispatched on extension: `*.txt` (plain-text per §3.3) and
  `*.jsonl` / `*.json` (OTLP/JSON Lines per §3.1 — one
  `LogsData` per line, the OTel File Exporter format).
  Both formats may coexist in the same directory; any other
  extension is silently skipped.
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

Adds a `just thesis-bench` recipe wrapping `cargo run -p
ourios-bench --release --`. The recipe is *not* named
`just bench` — the existing `bench` recipe in `justfile`
already runs `cargo bench` (criterion micro-benchmarks; the
suite is empty today, but the recipe is reserved for the
follow-up that lands `crates/ourios-bench/benches/`).
`thesis-bench` makes the gate-vs-microbench distinction
greppable at the recipe level. The `--release` is normative —
A1 on a debug-mode writer would understate compression
because debug builds disable some `arrow` / `parquet`
optimisations the release writer relies on.

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
>   encoding policy, and the `zstd_safe` Rust crate is linked
>   (per the §7 resolution of the ZSTD-integration question)
> - **When** the bench runs the A1 measurement
> - **Then** `bytes(raw_corpus)` equals
>   `sum(std::fs::metadata(f).len())` over the consumed
>   corpus files (`*.txt`, `*.jsonl`, `*.json`) in the corpus
>   directory
> - **And** `bytes(ourios_output)` equals the sum of all
>   `*.parquet` (not `*.parquet.tmp`) file sizes under the
>   bench's output bucket, including the `audit/...` partition
> - **And** `bytes(zstd_corpus)` equals the sum of
>   `std::fs::metadata(f).len()` over the `*.zst` files
>   produced by `zstd -19 --no-progress` on each consumed
>   input (same extension set as `bytes(raw_corpus)`)
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
>   **template count** at the sample whose line index is
>   closest to `999_999` (zero-indexed; per §3.4.3)
> - **And** `template_count_at_end` is the integer
>   **template count** at the final sample (the §3.4.3 SS
>   definition)
> - **And** `convergence_ratio = template_count_at_1m_lines /
>   template_count_at_end ≥ 0.5` — the "within 2× of SS"
>   gate, made non-tautological by defining SS as the
>   end-of-corpus value rather than the running max
> - **And** `c2.pass = true`
> - **And** the convergence curve in the results JSON has
>   exactly `ceil(total_lines / sample_cadence)` samples (the
>   sampling rule pinned in §3.4.3: indices
>   `N-1, 2N-1, 3N-1, …` plus a guaranteed final sample at
>   `total_lines - 1`)
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
>   `corpus.total_files`, `ourios.data_parquet_bytes`,
>   `ourios.audit_parquet_bytes`, `ourios.total_parquet_bytes`,
>   `zstd.compressed_bytes`, `a1.delta`,
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
  (`raw_bytes` from `fs::metadata`, `total_parquet_bytes` from
  inspecting the output bucket, `zstd_bytes` from the
  `zstd_safe` crate per the §7 ZSTD-integration resolution).
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

- [x] **`zstd` integration.** **Resolved 2026-05-25: the
      bench links the `zstd_safe` Rust crate.** Already in the
      dep tree via `parquet`'s `zstd` feature, so the marginal
      build cost is zero. The decision turns on cross-platform
      reproducibility: shell-out requires `zstd` on PATH at
      runtime (not default on macOS or Windows, version varies
      across Linux distros), and version drift across hosts
      would mean the same Ourios commit produces different A1
      numbers on different machines. With the crate, the
      compressor version is pinned by `Cargo.lock` and the
      bundled C library builds on every Tier 1 Rust platform
      — A1 is reproducible across Linux / macOS / Windows / CI
      runners without a host-side install step. The
      Drain-paper apples-to-apples concern is small in
      practice: `zstd_safe` wraps the same C library at the
      same compression level, so the resulting bytes are
      identical to what the CLI binary produces. (RFC0006.1
      asserts the byte-count formula directly; if a future
      observer wants to spot-check against a CLI binary, the
      JSON results file records `zstd.level = 19` so a
      reproduction pipeline is unambiguous.)
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
