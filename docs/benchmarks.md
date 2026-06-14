# Benchmarks

> Referenced from `CLAUDE.md` §6.2 ("regressions block merges") and from
> `docs/rfcs/0001-template-miner.md` §8. Flat-file, living document,
> parallel to `docs/hazards.md`. Updated with measured results as they
> come in.

This document is an **honesty contract with ourselves**. The thesis
(`CLAUDE.md` §2) claims that Parquet + Drain-derived template mining +
DataFusion beats the naive alternative of byte-level compression over
flat text. That claim is falsifiable. This file lists the measurements
that would falsify it.

The thresholds were pinned before any number was measured; if we miss
them on representative corpora, the thesis is wrong and a pillar
changes. As of **2026-06-14** the four gating thesis-gates **B1, B2, C1,
C2 all pass** on the §1 hardware baseline (§9.4/§9.6). **A1 fails but no
longer gates** — RFC 0011 (`accepted`) reclassified the
compression-vs-zstd ratio as a recorded diagnostic (its failure is
structural; see §2 / the §7 table).

## 0. How to read this document

Every goal below carries two labels.

- **Scope** — `thesis-gate`, `tuning-goal`, or `diagnostic`.
  - A `thesis-gate` failing on representative corpora means a pillar
    (`CLAUDE.md` §2) is wrong. The response is an RFC, not a sprint.
  - A `tuning-goal` failing means the design is sound but the
    implementation needs work. The response is a PR.
  - A `diagnostic` is measured and recorded but gates nothing — it
    characterises a property or guards against regression. A1 was
    reclassified here by RFC 0011 (`accepted`); see §2.
- **Bar** — `must-win`, `should-win`, `stretch`, or `informational`.
  - `must-win` — shipping without it is shipping a broken claim.
  - `should-win` — expected on representative corpora; explained when
    missed.
  - `stretch` — aspirational; missing is not a bug.
  - `informational` — a `diagnostic`'s bar: the number is recorded for
    insight, never blocks.

A goal with scope `thesis-gate` and bar `must-win` is load-bearing for
the whole project. **Four** of those below are gating — B1, B2, C1, C2,
each marked `[THESIS]`. A1 also carries the `[THESIS]` scope but RFC 0011
(`accepted`) demoted it to a recorded **diagnostic**: it no longer gates
(see its section below and the §7 table).

## 1. Corpora and methodology

Before any goal is meaningful, the corpora and methodology must be
pinned — otherwise we will argue about numbers instead of about
architecture.

- **Public**: LogPAI corpora (HDFS, BGL, Spark, Apache, OpenSSH,
  Windows) — the same corpora the Drain paper reports on. Lets us
  reproduce published claims as a sanity floor.
  - **LogHub HDFS_v1** is the first of these wired in, as a
    *bench-time-fetched* corpus for the query gates:
    `.github/workflows/query-bench.yml` downloads `HDFS_v1.zip`
    from the official Zenodo record (record 8196385, DOI
    `10.5281/zenodo.8196385`, md5-pinned in the workflow), uses
    the extracted `HDFS.log` (~1.47 GiB plain text — above §8's
    ≥ 1 GiB canonical minimum) in-job, and discards it with the
    runner. It is **never redistributed**: not committed (the
    `testdata/corpus/README.md` anonymisation gate — LogHub data
    is explicitly not sanitised), not attached to a release, not
    uploaded as an artifact; only aggregate numbers leave the job.
    LogHub's license notice, included here as it requires: "The
    datasets are freely available for research or academic work.
    For any usage or distribution of the datasets, please refer
    to the loghub repository URL
    (<https://github.com/logpai/loghub>) and cite the loghub
    paper: Jieming Zhu, Shilin He, Pinjia He, Jinyang Liu,
    Michael R. Lyu. Loghub: A Large Collection of System Log
    Datasets for AI-driven Log Analytics. In IEEE International
    Symposium on Software Reliability Engineering (ISSRE), 2023.
    The above license notice shall be included in all copies."
- **Self-collected (deferred)**: at least one anonymised corpus per
  target deployment archetype. Proposed set:
  1. Structured Java/Spring service (well-templated, low entropy).
  2. Go service under Kubernetes (heterogeneous, mid entropy).
  3. Heterogeneous k8s aggregate across many services (high entropy,
     mixed formats).
- **Hardware baseline**: a commodity 8 vCPU / 32 GiB RAM host with
  gp3-class SSD. All `must-win` numbers are quoted against this
  baseline; scaling to larger hardware is a separate question. The
  realised baseline (the `baseline-8vcpu-32gib` hardware tag, first
  used for the §9.4 authoritative run) is a dedicated host with
  8 dedicated vCPU, 32 GiB RAM, and a local NVMe-class SSD — at or
  above the spec on every axis, so numbers quoted against the tag
  satisfy this baseline. It is identified only by the tag.
- **Reference system**: `zstdcat <file.zst> | grep <pattern>`. The
  "naive alternative" the thesis beats or does not beat. Everything is
  quoted *relative to this*, not in absolute terms.

Goals quoted below assume this setup. When a goal is measured on a
different setup, the measurement is annotated.

## 2. Compression goals (Category A)

The core claim that template mining does useful work *before* byte
codecs run.

### A1 `[THESIS]` — End-to-end compression ratio vs. zstd-alone

> **Demoted to a diagnostic (RFC 0011, `accepted`).** A1 is refuted on
> every corpus class — including the maximally-templated one — for
> structural reasons, so it no longer gates any RFC's `validated`. It is
> still measured and recorded (§7 table / §9 series) as the columnar
> queryability premium and a codec-regression guard. The scope, bar,
> target, and falsifier below are retained as the diagnostic's reference
> line — now **informational**, not gating.

- **Scope**: diagnostic (RFC 0011; originally `thesis-gate`).
- **Bar**: informational (RFC 0011; originally `must-win`).
- **Metric**: `bytes(raw_corpus) / bytes(ourios_parquet_directory)`
  compared to `bytes(raw_corpus) / bytes(zstd_compressed_corpus)`.
- **Target**: Ourios ratio ≥ **3×** the zstd-alone ratio, on every
  corpus in §1. Best-case corpora (well-templated services) should
  show ≥ **10×**.
- **Falsifier**: if any representative corpus yields ≤ 2× improvement
  over zstd-alone, the template-mining pillar is not pulling its
  weight on that class of logs. Open an RFC.
- **Why this bar**: the project's headline claim (§2, CLAUDE.md) is
  "50–200× over raw, ≥ 5× over a competent byte codec." Below 3× the
  headline is dishonest.

### A2 — Bytes per line, amortised

- **Scope**: tuning-goal.
- **Bar**: should-win.
- **Metric**: total Parquet bytes for tenant / line count for tenant.
- **Target**:
  - Structured service logs: ≤ **30 B/line**.
  - Heterogeneous k8s: ≤ **100 B/line**.
  - Stretch: ≤ **15 B/line** on high-repetition corpora.
- **Why**: makes A1 legible to operators, who think in bytes-per-line,
  not ratios.

## 3. Query performance goals (Category B)

Why not `zstdcat | grep`? Because the query layer is supposed to
exploit structure the tree extracted.

### B1 `[THESIS]` — Predicate-pushdown queries

- **Scope**: thesis-gate.
- **Bar**: must-win.
- **Query shape**: `count events WHERE tenant=X AND ts BETWEEN t1 AND t2
  AND level='ERROR'`.
- **Reference**: `zstdcat files_in_range.zst | grep ERROR | wc -l` on
  the same corpus, same time window.
- **Target**: Ourios ≥ **10×** faster at 1 GiB corpus, widening to
  ≥ **100×** at 100 GiB.
- **Falsifier**: if Ourios is not materially faster than the zstdcat
  pipeline on predicate queries, DataFusion + Parquet statistics are
  not delivering on the "skip row groups via footer reads" pillar
  (`CLAUDE.md` §2.1). Open an RFC.
- **Instruments**: B1 is proven *structurally* (deterministically)
  by `ourios-querier`'s `rfc0007_1_*` tests. The `criterion` bench
  `crates/ourios-bench/benches/b1.rs` adds the *wall-clock* ratio:
  a `b1/synthetic` group (controlled pruning instrument vs. an
  in-process `zstdcat | grep` reference) and a `b1/real-corpus`
  group (set `OURIOS_B1_CORPUS_DIRS` to a comma-separated list of
  corpus dirs; skipped when unset). The real arm runs **OTLP
  corpora only** (`corpus/otel-demo-v*`, which carry real
  per-record severity): B1's predicate filters on severity, and
  the RFC 0006 §3.3 plain-text loader assigns every line a fixed
  severity (`9` / `INFO`), so a severity predicate over a
  plain-text corpus has no selectivity and such dirs are skipped
  with a note. CI runs land via
  `.github/workflows/query-bench.yml` on `ci-runner` — indicative
  only. The **authoritative** numbers are the
  `baseline-8vcpu-32gib` run of 2026-06-12 (§9.4): **PASS** at
  **34.2× / 25.4×** on the two ~1 GB OTel-Demo corpora, with exact
  row-count agreement against the reference pipeline. Open quality
  improvement (non-blocking): the measured error bands are
  ultra-thin (11 / 28 rows), which flatters pruning — a denser
  error band is the remaining methodological wish.

### B2 `[THESIS]` — Template-exact queries

- **Scope**: thesis-gate.
- **Bar**: must-win.
- **Query shape**: `SELECT * WHERE template_id = X AND ts BETWEEN …`.
- **Target**: latency **proportional to result cardinality**, not to
  corpus size, above a corpus size of ~10 GiB. Concretely: median
  latency ≤ 200 ms for a query returning 10 000 rows, *regardless of
  whether the corpus is 10 GiB or 10 TiB*.
- **Falsifier**: if template-exact queries scan proportionally to
  corpus size, template mining is buying compression but not query
  locality — the inverted-index collapse thesis (`CLAUDE.md` §2) is
  wrong in practice. Open an RFC.
- **Instruments**: B2 is proven *structurally* (deterministically)
  by `ourios-querier`'s `rfc0007_2_*` test — for a fixed result the
  scanned row groups + bytes stay flat as the corpus grows. The
  `criterion` bench `crates/ourios-bench/benches/b2.rs` adds the
  *wall-clock* view: a `b2/synthetic` group (result held constant,
  corpus scaled 1×/10×/50×) and a `b2/real-corpus` group over real
  corpora (set `OURIOS_B2_CORPUS_DIRS` to a comma-separated list of
  corpus dirs; skipped when unset, since the corpora aren't
  committed). Both loader formats feed it: the OTLP/JSON
  `corpus/otel-demo-v*` releases and the bench-time-fetched plain-
  text LogHub HDFS_v1 (§1). Run with
  `cargo bench -p ourios-bench --bench b2`. CI runs land via
  `.github/workflows/query-bench.yml` on `ci-runner` — indicative
  only. The **authoritative** numbers are the
  `baseline-8vcpu-32gib` run of 2026-06-12 (§9.4): **PASS** — the
  windowed template-exact scan stays at 1 row group with a flat
  ~4.2–5.9 ms latency band across every corpus, including the
  first reading from a second corpus family (LogHub HDFS_v1,
  11.2 M rows: 1/14 row groups, 5.92 ms), while the full-span variant grows with
  corpus size. The formal target speaks above ~10 GiB, which
  remains a future scale extension; the flat shape holding at
  11.2 M rows across two corpus families is the operative
  evidence.

### B3 — Substring queries (the hard case)

- **Scope**: tuning-goal.
- **Bar**: must-match; stretch: beat.
- **Query shape**: `SELECT * WHERE body LIKE '%<substring>%'` or
  equivalent.
- **Target**: not slower than the reference system. Stretch: faster on
  well-templated corpora by searching the template text rather than
  every line.
- **Why this is only tuning-goal, not thesis-gate**: substring search
  is the case where the *tree* does not help directly. We are allowed
  to match the reference system here; losing against it is a bug but
  not a pillar failure.

## 4. Miner correctness goals (Category C)

Correctness is not a performance goal, but it belongs here because
these are the properties the benchmark harness actually measures on
every run.

### C1 `[THESIS]` — Bit-identical reconstruction rate

- **Scope**: thesis-gate.
- **Bar**: must-win.
- **Metric**: of all non-lossy-flagged rows, fraction whose
  `reconstruct(template, params)` equals the ingested bytes exactly.
- **Target**: **100.000%**.
- **Falsifier**: a single row that reconstructs wrong without a lossy
  flag is a violation of `CLAUDE.md` §3.3 and a blocker, not a
  benchmark regression. Accompanied by: the lossy-flagged fraction
  should be ≤ 5% on structured corpora, ≤ 20% on heterogeneous ones,
  as a quality signal (not a gate).
- **Why this is a thesis-gate**: if we cannot promise
  reconstruction, the honesty contract (lecture §6) collapses.

### C2 `[THESIS]` — Template count convergence

- **Scope**: thesis-gate.
- **Bar**: must-win.
- **Metric**: template count as a function of lines ingested, on a
  corpus from a single stable service.
- **Target**: template count grows **sub-linearly** and plateaus
  within **2×** of its steady-state value by 1 M lines. Steady-state
  value is corpus-specific but is on the order of 10²–10⁴ templates
  for a normal service.
- **Falsifier**: if template count grows linearly with corpus size,
  Drain has failed to abstract — we are storing one template per
  line, which means the tree is providing compression only
  accidentally. That is the inverse of the thesis. Open an RFC.

### C3 — Merge rate

- **Scope**: tuning-goal.
- **Bar**: should-win.
- **Metric**: `merges_total / lines_ingested`.
- **Target**: ≤ **1 merge per 10⁵ lines** on stable corpora, with
  every merge carrying an audit event. Spikes above this rate are
  investigated; they usually indicate a new service version.
- **Why only tuning-goal**: merge rate depends on corpus stability
  more than on algorithm quality. The *auditing* is the invariant
  (§3.1); the *rate* is a signal.

### C4 — Parameter overflow rate

- **Scope**: tuning-goal.
- **Bar**: must-win.
- **Metric**: fraction of rows where any `params` slot hit the 256 B
  limit.
- **Target**: ≤ **1%** on representative corpora, per
  `CLAUDE.md` §3.2.
- **Falsifier (tuning sense)**: if >1% on a common archetype, either
  the limit is too tight for that workload or a masking rule is
  missing. The response is tuning, not an RFC.

## 5. Ingest goals (Category D)

The hot path must keep up with real deployments; otherwise none of
the above matters.

### D1 — OTLP → WAL throughput

- **Scope**: tuning-goal.
- **Bar**: must-win.
- **Metric**: lines/second/core sustained, with WAL fsync batched at
  100 ms (the `CLAUDE.md` §3.4 default).
- **Target**: ≥ **100 000 lines/s/core**, with **p99 ingest-ack
  latency ≤ 200 ms**.
- **Falsifier (tuning sense)**: below this we cannot ingest a
  meaningful share of production traffic per node, which makes the
  operational story uninteresting.

### D2 — WAL → Parquet compaction keeps up

- **Scope**: tuning-goal.
- **Bar**: must-win.
- **Metric**: WAL backlog (bytes, segments) as a function of time
  under sustained ingest at D1's rate.
- **Target**: bounded; backlog returns to zero during any one-hour
  window of sustained load.
- **Falsifier (tuning sense)**: a growing backlog under steady-state
  load means compaction is the bottleneck — a correctness-adjacent
  bug because it lets the WAL grow unboundedly.

### D3 — Small-file count under sustained load

- **Scope**: tuning-goal.
- **Bar**: should-win.
- **Metric**: number of Parquet files per tenant per day after
  background compaction has settled.
- **Target**: file sizes cluster in the **256 MiB–2 GiB** band per
  `CLAUDE.md` §4 / hazard 4. Fewer than **5%** of files below 128 MiB
  at steady state.
- **Why**: the small-file problem is a named hazard, not a nice-to-have.

## 6. Honesty goals (Category E)

Not performance. Not falsifiable by a benchmark in the usual sense.
Listed here because the benchmark harness asserts them on every run.

### E1 — Zero silent merges

- **Scope**: correctness invariant (not a benchmark).
- **Metric**: in the corpus-test suite, for every row whose
  `template_id` changed over its lifetime in the tree, an audit event
  exists with matching timestamp and tenant.
- **Target**: 100%. This is a proptest, not a measurement.

### E2 — Zero cross-tenant leakage

- **Scope**: correctness invariant (not a benchmark).
- **Metric**: no template mined under tenant A ever appears in
  tenant B's tree or in a row for tenant B.
- **Target**: 100%. Asserted via corpus tests that interleave lines
  from two synthetic tenants and verify complete isolation.

## 7. The thesis-gate summary

The five `[THESIS]`-tagged goals, consolidated:

| # | Goal | Failing means |
|---|------|---------------|
| A1 | Compression ≥ 3× over zstd-alone — **diagnostic, not gating** (RFC 0011) | Recorded for the columnar queryability premium + codec-regression guard; does **not** block any RFC's `validated`. Refuted on every corpus class incl. max-templated HDFS_v1 (§9.5) for structural reasons — template mining's compression is logical/query-pruning, captured by B1/B2 |
| B1 | Predicate queries ≥ 10× faster than `zstdcat \| grep` | Parquet statistics pillar not delivering |
| B2 | Template-exact queries scale with result size, not corpus size | Inverted-index-collapse thesis is wrong in practice |
| C1 | 100% bit-identical reconstruction on non-lossy rows | Honesty contract with user violated |
| C2 | Template count plateaus sub-linearly | Drain has failed to abstract |

**Policy**: if **one** thesis-gate fails on **one** representative
corpus, that is a corpus-specific tuning RFC. If **two or more**
thesis-gates fail on **any** representative corpus, that is a
pillar-level RFC — we pause implementation and revisit
`CLAUDE.md` §2 before continuing.

This escalation rule is the point of the whole document. The worst
failure mode for a greenfield project is shipping something whose
central claim quietly fails on real data and then papering over it
with more implementation. These goals exist so we cannot do that to
ourselves without noticing.

## 8. What is deliberately out of scope

- **SIEM-style full-text search latency** — explicitly out of scope
  (`CLAUDE.md` §1).
- **Cross-tenant aggregation queries** — tenancy is isolation-first
  (`CLAUDE.md` §3.7). Aggregations that cross tenants are an RFC
  topic, not a benchmark.
- **LLM-based parser comparisons** — interesting, deferred. Listed
  in RFC 0001 §7 as an alternative. Benchmarking it would be a
  separate RFC.
- **Cold-start query latency** — below a corpus size of ~1 GiB the
  overhead of Parquet metadata dominates, and the thesis is
  uninteresting. Benchmarks start at 1 GiB.

## 9. Status

First measurements landed **2026-06-01** (the writer-side gates
A1 / C1 / C2 — see §9.1). They are **diagnostic, not canonical**:
they ran on a GitHub-hosted runner (`ci-runner`), not the §1
hardware baseline (`baseline-8vcpu-32gib`), against an OTel-Demo
corpus that is **shape-representative** (real multi-service
template + envelope diversity) but **not size-representative** —
every corpus is well below §8's ≥ 1 GiB canonical minimum, so
this run is intentionally diagnostic, not a thesis verdict. The
query-side gates now have instruments — B1 and B2 are proven
structurally in `ourios-querier`, and both have `criterion` latency
benches with real-corpus arms (§B1/§B2 "Instruments"; OTel-Demo for
B1, OTel-Demo + the bench-time-fetched LogHub HDFS_v1 for B2, run
on `ci-runner` via `.github/workflows/query-bench.yml` as
indicative numbers). **2026-06-11** extended the writer-side scale
series to ~1 GB (§9.2) and landed the **first B1/B2 query
readings** (§9.3) — recorded here as indicative `ci-runner`
entries per the maintainer's 2026-06-12 authorization.
**2026-06-12** landed the **authoritative baseline run** (§9.4):
every gate measured on the §1 hardware (`baseline-8vcpu-32gib`),
recorded per the maintainer's 2026-06-12 authorization. B1, B2,
C1, and C2 **pass authoritatively**; on that basis RFC 0007
flipped to `validated` (its gates, per `docs/verification.md` §3,
are the querier-pillar ones — B1/B2). **A1 fails authoritatively**
and is the open thesis gate; it gates the compression pillar
(RFC 0006's remit), not RFC 0007, and carries a new
hardware-sensitivity caveat (§9.4).

Reviewers: a PR that materially affects the hot path must either
(a) cite the benchmark result and its delta against the relevant
goal, or (b) explain why the hot-path effect is bounded below
measurability. "I did not run the benchmarks" is a PR rejection, per
`CLAUDE.md` §6.6.

<!-- BENCH-RESULTS:BEGIN (managed by `ourios-bench --update-benchmarks-md`; do not edit by hand) -->

_No `ourios-bench --update-benchmarks-md` run has populated this
region yet. It is the **bench-managed** results area — automated
runs replace everything between these markers with one table per
`(git-sha, hardware)`. The hand-written §9.1 below is the
**curated** diagnostic narrative and lives outside the region so
automated runs never touch it. (This empty region is pre-placed so
the first `--update-benchmarks-md` run replaces it in place rather
than appending a second results section at end-of-file.)_

<!-- BENCH-RESULTS:END -->

### 9.1 Results — 2026-06-01 (diagnostic, `ci-runner`)

**Corpus.** `corpus/otel-demo-v{1..4}` — OTel Demo 2.2.0 logs
captured via the collector fileexporter (workflow
`.github/workflows/capture-otel-demo-corpus.yml`), business-service
logs only (collector self-telemetry + load-generator filtered out),
OTLP/JSON. Sizes 30 / 136 / 272 / 547 MiB — all below §8's ≥ 1 GiB
canonical benchmark minimum (this run is deliberately sub-minimum,
to chart the trend, hence diagnostic).
**Hardware.** `ci-runner` (hosted, ~4 vCPU) — **not** the §1
baseline, so deltas are indicative, not authoritative.

**A1 — compression (target: ourios ≥ 3.0× zstd-19).**

*Scale series* (ourios at the production ZSTD-3 default):

| corpus | size | ourios | zstd-19 | A1 delta |
|---|---|---|---|---|
| v1 | 30 MiB | 15.5× | 33.3× | 0.465 |
| v2 | 136 MiB | 21.5× | 32.3× | 0.666 |
| v3 | 272 MiB | 23.4× | 32.3× | 0.725 |
| v4 | 547 MiB | 24.6× | 32.4× | 0.758 |

*Codec sweep* (v4 = 547 MiB, ourios ZSTD level varied):

| ourios ZSTD | ourios | A1 delta |
|---|---|---|
| 3 (prod default) | 24.6× | 0.758 |
| 9 | 26.2× | 0.808 |
| 15 | 26.4× | 0.816 |
| 19 | 26.9× | 0.829 |

**A1 verdict: FAIL** (target 3.0×; best observed 0.829). Both
levers are bounded. Scale lifts the delta but plateaus ~0.78
(ourios asymptotes ~25×; zstd-19 is flat ~32× — the logs are
locally repetitive, so zstd compresses them well at any size, not
via a whole-corpus window). Raising ourios's codec to ZSTD-19
adds only ~+0.07 and saturates by level 9. Even at **equal** codec
strength, ourios stays ~17% larger than monolithic zstd-19: a
**structural** cost of columnar Parquet (per-column/per-chunk
framing, page indexes, row-group metadata, bloom filters) versus
zstd-19 over one concatenated stream. That same chunking is what
enables row-group skipping — so the ~17% space premium **is the
price of queryability**, not an optimisation target. On pure
compression of this corpus, ourios ≈ 0.83× zstd-19; the thesis
rests on query performance (B1/B2), not on beating a byte codec.

**C1 — reconstruction (target: 100% bit-identical or flagged lossy).
PASS** at every size: 1.0 reconstruct rate, ~1.1% of records
flagged lossy (structured/`kvlist` bodies) and retained verbatim
per `CLAUDE.md` §3.3.

**C2 — template-count convergence (target: sub-linear). PASS
(supportive).** Templates grew 282 → 429 → 722 → 1322 while
records grew 38k → 183k → 366k → 735k — sub-linear throughout. The
formal gate abstains below 1 M lines (§3.4.3), but the curve shape
is the strongest evidence yet for the template-mining premise.

**Escalation (§7).** One gate (A1) fails, on a *size-non-representative*
corpus (all < §8's 1 GiB minimum) and *non-baseline* hardware — so
this is "corpus-specific," not the two-gate pillar-level pause. C1 + C2 support the thesis.
The production ZSTD-3 default is retained: the codec gain is small,
saturates by level 9, and the residual gap is structural, so a
higher default isn't worth the ingest-CPU.

### 9.2 Results — 2026-06-11 (diagnostic, `ci-runner`) — A1 / C1 / C2 at ~1 GB

**Corpus.** `corpus/otel-demo-v5` (1,042,274,219 B) and
`corpus/otel-demo-v6` (1,034,615,505 B) — same capture pipeline as
§9.1, extending the scale series to ~1 GB (both within 4% of, but
still just under, §8's ≥ 1 GiB binary minimum). v6 was captured
with the OTel Demo failure flags enabled (`adFailure cartFailure
productCatalogFailure`), so it carries a real error band; v5 is an
unflagged capture.
**Hardware.** `ci-runner` — indicative, not the §1 baseline.
**Runs.** `bench.yml` 27370641352 (v5), 27373716667 (v6).

**A1 — compression (target: ourios ≥ 3.0× zstd-19).**

| corpus | size | run | ourios | zstd-19 | A1 delta |
|---|---|---|---|---|---|
| v5 | 1,042,274,219 B | 27370641352 | 26.3× | 31.7× | 0.828 |
| v6 | 1,034,615,505 B | 27373716667 | 26.0× | 31.5× | 0.824 |

**A1 verdict: FAIL** (target 3.0×). The scale series now reads
0.465 (v1, 30 MiB) → 0.666 (v2) → 0.725 (v3) → 0.758 (v4) →
0.828 (v5) / 0.824 (v6): the delta is size-driven and still
rising, but decelerating — the crossover is not reached at ~1 GB,
consistent with §9.1's structural reading (ourios asymptotes
~26×; zstd-19 stays flat ~32×). v5 ≈ v6 shows the failure-flag
error band does not perturb A1. This is the first A1 miss at
(essentially) canonical size, so §9.1's "size-non-representative"
mitigation no longer applies; it remains a single-gate fail (no
§7 two-gate pause), the §9.1 structural explanation stands, and
the thesis-deciding counterpart — B1/B2 — now passes indicatively
(§9.3). Whether the §7 corpus-specific tuning-RFC response
triggers is a maintainer decision, sensibly taken once an
authoritative `baseline-8vcpu-32gib` run confirms the number.
*(Resolved 2026-06-12: the §9.4 baseline run confirms — and
slightly worsens — the deltas; the decision is now live with the
maintainer.)*

**C1 — reconstruction (target: 100% bit-identical or flagged
lossy). PASS** on both: 1.000000 — v5 reconstructs
1,213,004 / 1,213,004 non-lossy rows exactly (lossy ratio
0.0114); v6 1,208,323 / 1,208,323 (lossy 0.0112).

**C2 — template-count convergence (target: ratio ≥ 0.5 at 1 M
lines). PASS** on both — and for the first time on ≥ 1 M-line
corpora, so the formal gate applies rather than §9.1's
abstention: v5 convergence ratio 0.756 (end count 1605, sample
cadence 1336); v6 ratio 0.760 (end count 1606, cadence 1329).

### 9.3 Results — 2026-06-11 (indicative, `ci-runner`) — first B1 / B2 query readings

**Corpus.** `corpus/otel-demo-v{4,5,6}` (the §9.1 / §9.2
captures). The LogHub HDFS_v1 B2 arm did not run (`fetch_hdfs`
off — memory-bound on the hosted runner), so only one corpus
family has fed the query gates.
**Hardware.** `ci-runner` — indicative, not the §1 baseline.
**Runs.** `query-bench.yml` 27379085890 (B1 + the B2 structural
metrics, after the effective-timestamp stack #178/#179) and
27357104694 (the prior run; its windowed / full-span latencies
are quoted where noted).
**Recording.** B1/B2 entries land in §9 per the maintainer's
2026-06-12 authorization. RFC 0006 never reserved §9 (its §1
anticipated B1/B2 landing "in a follow-up extension PR once the
querier is live" — RFC 0007); the workflow itself never writes
§9 — every entry here is curated by hand.

**B1 — predicate pushdown vs `zstdcat | grep` (target: ≥ 10× at
1 GiB).** Query: severity `ERROR`, full corpus span. Run
27379085890:

| corpus | rows | RGs scanned | ourios bytes | reference bytes (zstd) | ourios | reference | speedup |
|---|---|---|---|---|---|---|---|
| v5 | 11 | 3/6 | 326,102 | 1,403,025 | 6.14 ms | 245.5 ms | 40.0× |
| v6 | 28 | 5/6 | 764,082 | 1,455,912 | 8.50 ms | 258.5 ms | 30.4× |

Row counts agree **exactly** with the reference pipeline on both
corpora. v4 is skipped: the unflagged 100-user capture genuinely
contains zero error-band rows, so the predicate selects nothing.

**B1 verdict: PASS (indicative)** — both corpora clear the ≥ 10×
bar at 3–4× margin, on the first real-corpus reading. Caveats,
stated plainly: `ci-runner`, not the §1 baseline; the error
bands are ultra-thin (11 / 28 rows — extreme selectivity is the
friendliest case for pruning); both corpora sit just under the
§8 1 GiB minimum. An authoritative `baseline-8vcpu-32gib` rerun
(ideally with a denser error band) is required before this
counts as the canonical B1 number.

**B2 — template-exact latency ∝ result, not corpus.** Windowed
1-hour template-exact query, result roughly constant as the
corpus grows. Structural metrics (run 27379085890): scanned row
groups stay **flat at 1** — v4 1/5, v5 1/6 (17,632 rows,
1.86 MB), v6 1/6 (11,750 rows, 1.59 MB). Wall-clock (prior run
27357104694): windowed latencies sit in a flat ~3.4–4.1 ms band
(v4 3.59 / v5 4.13 / v6 3.40 ms) while the full-span variant
grows with corpus size (7.3 / 10.6 / 10.6 ms) — exactly the
result-bound-vs-corpus-bound split the gate asks for.

**B2 verdict: PASS (supportive, indicative)** — the flat shape
is confirmed on real corpora at ~1 GB; the formal target speaks
above ~10 GiB, which remains unmeasured, and the second corpus
family (HDFS_v1) hasn't fed the arm yet.

**RFC 0007 validated assessment.** These are the measurements
the RFC 0007 `green → validated` gate needs, but not yet in the
form the ladder requires (§1 quotes must-win numbers against
`baseline-8vcpu-32gib`): see the status note in
`docs/rfcs/0007-querier.md`. The RFC stays `green` with a
validated-pending note — authoritative baseline rerun required;
denser error band and a second corpus family supporting.
*(Resolved 2026-06-12: the §9.4 authoritative run delivered the
baseline rerun **and** the second corpus family (HDFS_v1);
RFC 0007 is `validated`. The denser error band remains an open
quality improvement.)*

### 9.4 Results — 2026-06-12 (authoritative, `baseline-8vcpu-32gib`)

**Corpus.** `corpus/otel-demo-v{1..6}` (the §9.1 / §9.2
captures; 30 MiB → ~1 GB) for A1 / C1 / C2 and B1/B2's OTel-Demo
arms, plus — for the first time — the bench-time-fetched LogHub
**HDFS_v1** (§1; ~1.47 GiB plain text, 11,175,629 rows ingested
across 5 files) feeding the B2 arm as the **second corpus
family**.
**Hardware.** `baseline-8vcpu-32gib` — the §1 baseline
(8 dedicated vCPU, 32 GiB RAM, local NVMe-class SSD). These are
the **authoritative** numbers the §1 methodology quotes must-win
gates against; the §9.1–§9.3 `ci-runner` entries remain
indicative history.
**Runs.** Dedicated baseline host (no CI run id): one
`ourios-bench` run per corpus (A1/C1/C2) plus one query-bench
run (B1 + B2), executed 2026-06-11/12; raw logs retained by the
maintainer. Recorded per the maintainer's 2026-06-12
authorization.

**A1 — compression (target: ourios ≥ 3.0× zstd-19).**

| corpus | size | ourios | zstd-19 | A1 delta |
|---|---|---|---|---|
| v1 | 30 MiB | 14.6× | 33.3× | 0.439 |
| v2 | 136 MiB | 19.9× | 32.3× | 0.615 |
| v3 | 272 MiB | 21.4× | 32.3× | 0.665 |
| v4 | 547 MiB | 22.5× | 32.4× | 0.693 |
| v5 | 994 MiB | 23.8× | 31.7× | 0.751 |
| v6 | 987 MiB | 23.6× | 31.5× | 0.749 |

**A1 verdict: FAIL (authoritative)** (target 3.0×; best observed
0.751). The delta is monotonic with corpus size and the crossover
is unobserved, consistent with §9.1's structural reading. One
finding must be recorded honestly: the authoritative deltas sit
**below** the `ci-runner` series (0.465 → 0.828) at every size —
the ourios side compressed *less* effectively on this hardware
(e.g. v5: 23.8× vs CI's 26.3×) while zstd-19 stayed essentially
stable (31.7× on both) — i.e. the ourios writer's output is
**environment-sensitive** (suspected row-group sizing / threading
effects on the resulting encodings). That is now an **open A1
investigation item** alongside the structural gap itself. A1
gates the compression pillar (RFC 0006's remit); the §7
escalation response is with the maintainer.

**C1 — reconstruction (target: 100% bit-identical or flagged
lossy). PASS (authoritative)** on every corpus: 1.000000
throughout — v5 reconstructs 1,213,004 / 1,213,004 non-lossy rows
exactly (lossy ratio 0.0114), v6 1,208,323 / 1,208,323 (lossy
0.0112); v1–v4 likewise 1.000000 (lossy 0.0097–0.0112). The
formal ≥ 1 M-line gate passes on the baseline.

**C2 — template-count convergence (target: ratio ≥ 0.5 at 1 M
lines). PASS (authoritative)** on both ≥ 1 M-line corpora: v5
ratio 0.756 (end template count 1605, sample cadence 1336), v6
ratio 0.760 (end count 1606, cadence 1329). v1–v4 abstain
(< 1 M lines), as in §9.1.

**B1 — predicate pushdown vs `zstdcat | grep` (target: ≥ 10× at
1 GiB).** Query: severity `ERROR`, full corpus span. v4 is
skipped (zero error-band rows, as in §9.3).

| corpus | rows | RGs scanned | ourios bytes | reference bytes (zstd) | ourios | reference | speedup |
|---|---|---|---|---|---|---|---|
| v5 | 11 | 3/6 | 326,102 | 1,403,025 | 5.86 ms | 200.27 ms | **34.2×** |
| v6 | 28 | 5/6 | 764,082 | 1,455,912 | 8.03 ms | 203.87 ms | **25.4×** |

Row counts agree **exactly** with the reference pipeline on both
corpora (11 and 28).

**B1 verdict: PASS (authoritative)** — both corpora clear the
≥ 10× bar at 2.5–3.4× margin on the §1 baseline. Remaining
caveat, non-blocking: the error bands are still ultra-thin
(11 / 28 rows — the friendliest case for pruning); a denser
error band stays an open quality improvement.

**B2 — template-exact latency ∝ result, not corpus.**

*Full-span template-exact* (result grows with the corpus, so
latency may too):

| corpus | rows returned | RGs scanned | bytes | latency |
|---|---|---|---|---|
| v4 | 89,382 | 5/5 | 5,514,033 | 6.84 ms |
| v5 | 168,487 | 6/6 | 6,785,714 | 9.57 ms |
| v6 | 168,313 | 6/6 | 6,801,255 | 9.69 ms |
| hdfs-v1 | 1,723,232 | 14/14 | 16,523,421 | 30.19 ms |

*Windowed 1-hour template-exact* (the gate's shape: result
roughly constant as the corpus grows):

| corpus | corpus rows | rows returned | RGs scanned | bytes | latency |
|---|---|---|---|---|---|
| v4 | 735,377 | 12,854 | 1/5 | 1,674,718 | 4.39 ms |
| v5 | 1,367,532 | 17,632 | 1/6 | 1,857,999 | 5.07 ms |
| v6 | 1,360,040 | 11,750 | 1/6 | 1,592,279 | 4.19 ms |
| hdfs-v1 | 11,175,629 | 28,207 | 1/14 | 1,737,852 | **5.92 ms** |

The HDFS_v1 row is the **first reading from the second corpus
family** (plain-text, the template-diversity case): the corpus is
8–15× the OTel-Demo row counts, yet the windowed scan still
touches 1 row group (13 pruned) and stays inside the same flat
latency band, while the full-span variant grows with the corpus
(6.84 → 30.19 ms) — exactly the result-bound-vs-corpus-bound
split the gate asks for.

**B2 verdict: PASS (authoritative)** — windowed ~10–28 k-row
results answer in 4.2–5.9 ms (gate: ≤ 200 ms for ~10 k rows),
flat from 735 k to 11.2 M rows across two corpus families. The
formal target's ≥ 10 GiB regime remains a future scale
extension; the measured shape is the operative evidence.

**RFC 0007 `green → validated` (resolved).** The
`docs/verification.md` §3 ladder reads: *"Every thesis-gate in
`benchmarks.md` §7 that the RFC's pillars touch passes on
representative corpora."* RFC 0007's pillar is the query engine
(pillar #3); its gates are **B1 and B2**, both now passing
authoritatively on the §1 baseline over ~1 GB+ corpora including
a second family. **A1 does not gate RFC 0007** — it belongs to
the template-mining/compression pillar, measured under RFC 0006.
RFC 0007 is therefore flipped to `validated` (see its status
note); `accepted` awaits maintainer sign-off per the ladder.

### 9.5 Results — 2026-06-13 (diagnostic, local `unknown` hardware) — A1 / C1 / C2 on HDFS_v1

**Corpus.** LogHub HDFS_v1 (Zenodo record 8196385, md5
`76a24b4d…`) — 11,175,629 lines, 1,577,982,906 raw bytes; fetched at
bench time, never redistributed (`query-bench.yml`). The
**maximally-templated** log corpus (a handful of templates over 11.2 M
lines) — the single best case for the template-mining compression
premise. Run via
`ourios-bench --gates a1,c1,c2 --parquet-zstd-level 19 --allow-unknown-hardware`.
**Local hardware → diagnostic, not
authoritative**; A1's verdict is corpus-structural and
hardware-independent (compressed bytes are deterministic), C1/C2 are
ratios, so the findings hold regardless of the runner.

| gate | result | verdict |
|---|---|---|
| A1 | ourios 8.300× vs zstd-19 16.000× → **delta 0.516×** (raw 1.578 GB → ourios 189.98 MB, zstd-19 98.21 MB) | FAIL — now **diagnostic** (RFC 0011) |
| C1 | **1.000000** — 11,175,578 / 11,175,578 non-lossy rows bit-identical; lossy ratio 4.6e-06 (51 rows) | PASS |
| C2 | end template count **40** at 11.2 M lines (33 at 1 M); ratio 0.825 — sub-linear, **formal gate applies** (≥ 1 M, §3.4.3) | PASS |

**A1 — the decisive finding (→ RFC 0011).** A1 had only ever been
measured on OTel-Demo (best `0.829×`, §9.1/§9.4). HDFS_v1 is the
corpus that should most reward template mining, yet A1 fails *harder*
(`0.516×`): the more templated the corpus, the more completely
monolithic zstd-19 captures its redundancy in one window (16×), while
template mining's extracted params (block IDs, timestamps, IPs) are
high-cardinality columns that don't compress as well and the columnar
layout adds framing. **The best case for template mining is the best
case for the byte codec.** So `≥ 3× over zstd` cannot hold on any
realistic log corpus — A1 is **demoted to diagnostic** and template
mining's compression value is recognised as logical/query-pruning
(B1/B2), not on-disk bytes. See RFC 0011.

**C1 + C2 — the miner pillar's real gates, PASS on a representative
corpus.** At 11.2 M lines C1 is bit-identical (1.0) with a 4.6e-06
lossy ratio, and C2 plateaus at 40 templates with the formal gate
*applying* (not abstaining, unlike the §9.1 sub-1 M runs). Under RFC
0011 these are RFC 0001's `validated` thesis gates — both pass here.
The authoritative `baseline-8vcpu-32gib` representative rerun (for the
actual RFC 0001 `validated` flip) followed on 2026-06-14 (§9.6); as
expected of deterministic verdicts, the numbers are identical.

### 9.6 Results — 2026-06-14 (authoritative, `baseline-8vcpu-32gib`) — C1 / C2 on HDFS_v1

**Corpus.** LogHub HDFS_v1 (Zenodo record 8196385, md5
`76a24b4d…`) — 11,175,629 lines, 1,577,982,906 raw bytes; fetched at
bench time on the baseline host, md5-verified, never redistributed.
**Hardware.** `baseline-8vcpu-32gib` — the §1 baseline (8 dedicated
vCPU, 32 GiB RAM, local SSD), provisioned for this run and torn down
immediately after. These are the **authoritative** C1 / C2 numbers
for RFC 0001's `validated` gates.
**Run.** Dedicated baseline host (no CI run id): one `ourios-bench
--gates c1,c2 --hardware-kind baseline-8vcpu-32gib` run at git
`9a57ace`; results JSON retained by the maintainer
(`2026-06-14T00-36-23.225Z-9a57ace.json`). A1 was deliberately **not**
run — it is diagnostic, not gating (RFC 0011); the §9.5 diagnostic A1
reading stands.

| gate | result | verdict |
|---|---|---|
| C1 | **1.000000** — 11,175,578 / 11,175,578 non-lossy rows reconstruct bit-identically; lossy ratio 4.6e-06 (51 rows) | PASS |
| C2 | end template count **40** at 11.2 M lines (33 at 1 M); ratio 0.825 — sub-linear, **formal gate applies** (≥ 1 M, §3.4.3) | PASS |

**Authoritative confirmation.** The verdicts match §9.5's local
diagnostic run bit-for-bit — expected, since C1 (reconstruction
fidelity) and C2 (template-count convergence) are deterministic
functions of (corpus, miner) with no wall-clock or hardware-sensitive
component (contrast A1's writer-environment sensitivity, §9.4). The
value of this run is the authoritative `hardware_kind` stamp on the
two gates that, under RFC 0011, define RFC 0001's `validated`: **both
PASS on a representative ≥ 1 M-line corpus on §1 baseline hardware.**
