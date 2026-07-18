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
each marked `[THESIS]`. A1 keeps the `[THESIS]` tag (a thesis-relevant
measurement) but RFC 0011 (`accepted`) set its **scope to `diagnostic`**:
it is recorded, not gating (see its section below and the §7 table).

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
- **Why recorded** (diagnostic, not a bar): `CLAUDE.md` §2 pillar #2
  describes a **logical** 50–200× reduction (lines → `(template_id,
  params)`) whose payoff is query pruning (B1/B2), *not* on-disk bytes vs
  a byte codec. A1 tracks the on-disk ratio as the columnar queryability
  premium + a codec-regression guard; RFC 0011 (`accepted`) demoted it
  from a gate to this diagnostic.

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
- **Grain (amended for #444, 2026-07-10)**: because the metric is
  defined *per stable service*, the gate is evaluated **per
  `service.name`** on a multi-service corpus, not on the whole corpus.
  A corpus passes iff every service with ≥ 1 M lines converges; a
  single-service (or plain-text `<unknown>`) corpus is gated on that
  one service's exact-millionth-line ratio, reproducing the
  pre-amendment verdict for historical converged corpora. The
  whole-corpus ratio is retained as a diagnostic. See RFC 0006 §3.4.3.
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
are the querier-pillar ones — B1/B2). **A1 fails authoritatively** and
carries a hardware-sensitivity caveat (§9.4). *(A1 was subsequently
reclassified a recorded **diagnostic**, not a gate — RFC 0011,
`accepted` 2026-06-14. The A1 readings throughout §9 are diagnostic; A1
gates nothing, and the "open gate" / "must-win" framing in the dated
entries below is superseded.)*

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

### 9.7 Results — 2026-06-15 (authoritative, `baseline-8vcpu-32gib`) — D2 / D3 / B2-post (RFC 0009 compaction)

**Hardware.** `baseline-8vcpu-32gib` — the §1 baseline (8 dedicated
vCPU, 32 GiB RAM, local SSD), provisioned for this run and torn down
immediately after. These are the **authoritative** D2 / D3 / B2-post
numbers for RFC 0009's `validated` measure (RFC0009.7).
**Run.** Dedicated baseline host (no CI run id): the `ourios-bench`
`compaction` bench at git `4d52288`. Two invocations — the band-scale
one-shot (`OURIOS_COMPACTION_BASELINE=1`, `FILES=32`, `ROWS=4800`,
`BODY_BYTES=4096`) for D2/D3, then the `b2-post-compaction` criterion
group. Synthetic (no corpus): D2/D3 drive one partition of 32 small
files (~485 MiB) through `compact_partition`; B2-post queries
32-files-vs-1-file with the result set held constant.

| measure | result | verdict |
|---|---|---|
| **D2** compaction throughput | 32 files (485.2 MiB) → 1 in 2.91 s = **166.8 MiB/s**; 153,600 rows conserved | keeps up — single-partition / single-threaded, ≫ any per-partition seal rate, so the backlog drains |
| **D3** small-file size band | output **456.7 MiB** — **IN** the 256 MiB–2 GiB band; **0%** of live files < 128 MiB (target < 5%) | PASS |
| **B2-post** query latency | template query: uncompacted **12.78 ms** (32 row groups, 33.5 MiB read, 32 files) → compacted **2.10 ms** (1 row group, 1.05 MiB, 1 file) = **6.1×** | PASS |

**Reading.** D3 is the headline: a band-scale compaction lands its
output squarely in the H4 256 MiB–2 GiB target with zero sub-128 MiB
files — the small-file problem, eliminated. D2 shows consolidation
runs at ~167 MiB/s on one partition/thread, far above any plausible
per-partition seal rate, so a backlog drains (the "keeps up"
property). B2-post quantifies the query payoff that motivated RFC 0009
(the PR #92 B2 finding that per-file footer/metadata reads dominate):
collapsing 32 files → 1 cuts the footer reads ~6× on this query. The
structural reductions (32 → 1 files / row groups, rows conserved) are
hardware-independent and also pinned in `ourios-parquet`'s
`rfc0009_1_*` / `compaction_conserves_every_row` tests; these
wall-clock figures are the baseline-hardware stamp for RFC 0009's
`validated`. The full sustained-ingest soak (D2's "backlog returns to
zero in a one-hour window at D1's rate") and D1 itself remain unrun —
the throughput here is the RFC0009.7 D2 measure, not that soak.

### 9.8 Results — 2026-06-18 (authoritative, `baseline-8vcpu-32gib`) — ingest write-path + recovery (criterion) and real-corpus A1 / C1 / C2 + B1 / B2

**Hardware.** `baseline-8vcpu-32gib` — the §1 baseline (8 dedicated
vCPU, 32 GiB RAM, local SSD), provisioned for this run and torn down
immediately after. Two such hosts (one per invocation set), both at
git `d3f2cae`.
**Run.** (a) the self-contained `ourios-bench` criterion benches
`ingest_write_path` (RFC 0014) and `recovery` (RFC0008.3) — synthetic,
no corpus — at full criterion settings; (b) the `ourios-bench` binary
`--gates a1,c1,c2` against two real corpora, plus the `b1`/`b2`
criterion benches (`--warm-up-time 1 --measurement-time 3`, matching
`query-bench.yml`) over those corpora. Corpora: **LogHub HDFS_v1**
(Zenodo record 8196385, md5 `76a24b4d…` — 11,175,629 lines /
1,577,982,906 raw bytes (1.47 GiB) of **real Hadoop production logs**,
above §8's ≥ 1 GiB canonical minimum)
and the frozen **OTel-Demo v1** (`corpus/otel-demo-v1`, 38,782 lines /
31.5 MiB). HDFS is fetched in-job and never redistributed (§1).

**(a) Ingest write path + recovery — supportive wall-clock (criterion).**

| bench | median | throughput |
|---|---|---|
| `wal_append/batch` — OTLP→WAL append + fsync (the WAL-before-ack unit) | **372 µs** | 10.5 MiB/s |
| `sink_write/1000` — WAL→Parquet emit + flush (RFC 0014) | 2.64 ms | 379 K rec/s |
| `sink_write/10000` | **12.24 ms** | **817 K rec/s** |
| `recovery/{1,4,16}` — WAL replay over N segments (RFC0008.3) | 169 µs → 507 µs → **1.87 ms** | ~O(N), no amplification |

Single-threaded micro-benches on synthetic records — *supportive*
wall-clock (the structural sides are pinned by `ourios-ingester`'s
RFC 0014 / `ourios-wal`'s RFC0008.3 tests), not gates. Dedicated
hardware ran ~20–30% faster with much lower variance than the
indicative `ci-runner` figures.

**(b) Thesis gates A1 / C1 / C2 on real corpora.**

| corpus | A1 (ourios vs zstd-19 → delta) | C1 reconstruction | C2 convergence |
|---|---|---|---|
| **HDFS_v1** (11.18 M lines, 1.47 GiB) | 6.21× vs 16.0× → 0.386 — **FAIL** (diagnostic) | **1.000000** (11,175,578 / 11,175,578 non-lossy rows; lossy ratio 4.6e-06, 51 rows) — **PASS** | ratio **0.825**, 40 templates — **PASS** |
| OTel-Demo v1 (38.8 K lines) | 14.6× vs 33.3× → 0.438 — **FAIL** (diagnostic) | 1.000000 (lossy ratio 0.0097) — **PASS** | **ABSTAIN** (< 1 M lines), 282 templates |

C1 reconstructs **every** non-lossy row bit-for-bit across 11 M real
production lines — the §3.3 invariant holds on real data at scale. C2
converges on HDFS (40 templates over 11 M lines; ratio 0.825 ≥ the
threshold) — the template-mining thesis on a real corpus. A1 fails as
expected: it is a recorded **diagnostic, not a gate** (RFC 0011) —
template mining's value is query pruning (B1/B2), not on-disk bytes
beating a whole-stream codec.

**(c) Query gates B1 / B2 on real corpora.**

| bench | result | timing | pruning |
|---|---|---|---|
| `b1/synthetic` | 2000 rows | ourios 2.93 ms vs zstd-grep ref 118 µs | pruned 1/2 row groups, read 7.8 KB |
| `b2/synthetic/{2k,20k,100k}` | result held constant | 2.13 / 4.67 / 11.32 ms | sub-linear in corpus size |
| `b2/real-corpus/HDFS` (template 1, ubiquitous) | 1.72 M rows | 30.8 ms | 14/14 row groups (no prune — template is everywhere) |
| **`b2/real-corpus/HDFS` windowed 1 h** | 28,207 rows | **6.1 ms** | **13/14 row groups pruned by the time window** (~5× faster) |

The windowed HDFS arm is the headline: a time-bounded query on the
real 11 M-line corpus prunes **13 of 14 row groups** via Parquet
min/max statistics — the predicate-pushdown thesis (pillar #1) on real
production data, ~5× faster than the unwindowed scan. (B1's real-corpus
arm skipped: OTel-Demo v1 has no error-band `severity_text` rows for
the selectivity probe.) B1/B2's structural pruning is the gate (pinned
in `ourios-querier`); these are the baseline-hardware wall-clock stamp.

**Not committed by the bench tooling** — this is the curated narrative;
the managed `BENCH-RESULTS` region above is for `--update-benchmarks-md`
runs. The b1/b2 criterion timings use the reduced
`--warm-up-time 1 --measurement-time 3` (matching `query-bench.yml`);
the structural pruning/template numbers are exact and
criterion-setting-independent.

### 9.9 Results — 2026-07-03 (indicative, `ci-runner`) — B1 / B2 post-RFC 0022 (promoted attribute columns)

**Purpose.** The RFC 0022 §5 RFC0022.5 note: the promoted-attribute
write path (per-key `resource.<k>` / `attr.<k>` columns + the two-arm
predicate compile) must leave B1/B2 unchanged. This is the indicative
re-run after RFC 0022 went `green` (#345–#348); the pruning *counters*
are pinned structurally in `crates/ourios-querier/tests/rfc0022_attr_columns.rs`,
this entry is the wall-clock stamp.
**Corpus.** `corpus/otel-demo-v4` (107,332 records → 735,377 mined
rows / 5 files) and `corpus/otel-demo-v5` (163,929 records,
~1.04 GB raw → 1,367,532 mined rows / 6 files). The LogHub HDFS_v1
arm did not run (`fetch_hdfs` off — memory-bound on the hosted
runner).
**Hardware.** `ci-runner` — indicative, not the §1 baseline.
**Run.** `query-bench.yml` 28686650566 at git `6e3301b` (the RFC 0022
`green` merge).

**B1 — predicate pushdown vs `zstdcat | grep` (target: ≥ 10× at
1 GiB).** Query: severity `ERROR`, full corpus span.

| corpus | rows | RGs scanned | ourios bytes | reference bytes (zstd) | ourios | reference | speedup |
|---|---|---|---|---|---|---|---|
| v5 | 11 | 3/6 | 324,773 | 1,403,025 | 8.10 ms | 282.06 ms | 34.8× |

Row count agrees exactly with the reference pipeline. v4 is skipped as
in §9.3 (its capture has no error-band rows).

**B1 verdict: PASS (indicative), no regression** — 34.8× against
§9.3's 40.0× on the same corpus, comfortably inside hosted-runner
noise and 3.5× above the bar. Same caveats as §9.3: ultra-thin error
band, corpus just under the §8 minimum, not the §1 baseline.

**B2 — template-exact latency ∝ result, not corpus.**

| bench | result | timing | pruning |
|---|---|---|---|
| `b2/real-corpus/corpus/v4` (template 45) | 89,382 rows | 8.71 ms | 5/5 row groups (full span) |
| `b2/real-corpus/corpus/v5` (template 8) | 168,487 rows | 12.37 ms | 6/6 row groups (full span) |
| **`b2/real-corpus/corpus-window-1h/v4`** | 12,854 rows | **5.46 ms** | **1/5 — 4 row groups pruned by the time window** |
| **`b2/real-corpus/corpus-window-1h/v5`** | 17,632 rows | **6.71 ms** | **1/6 — 5 row groups pruned by the time window** |
| `b2/synthetic/{2k,20k,100k}` | result held constant | 2.17 / 4.77 / 13.61 ms | sub-linear in corpus size |

**B2 verdict: PASS (supportive, indicative), no regression** — the
windowed latencies sit in the same flat few-ms band as §9.3/§9.8
while the full-span variants grow with corpus size, and everything is
orders of magnitude under the 200 ms bar. The formal target speaks
above ~10 GiB, which remains unmeasured on this runner class.

**Assessment.** The promoted-column machinery (extra column chunks
per row group on the write side; the two-arm `OR` compile on the
read side) shows no measurable drag on either gate. The RFC 0022
`green → validated` step still requires the authoritative
`baseline-8vcpu-32gib` rerun per the standing bench policy
(maintainer opt-in); this entry is its indicative precursor,
curated by hand as in §9.3 — the workflow never writes §9.

### 9.10 Results — 2026-07-04 (authoritative attempt, `baseline-8vcpu-32gib`) — B1/B2 at 16 GiB: run blocked, miner finding

**Purpose.** The first run in the §8 10–100 GiB band: B2's formal
target speaks above ~10 GiB and had never been measured there.
**Corpus.** LogHub HDFS_v2 (bench-time fetch, never redistributed):
31 files, 17,240,888,465 bytes ≈ 16.1 GiB raw, ~71 M lines of Hadoop
daemon logs — the first corpus in our set whose *shape* (stack
traces, multi-format node logs) differs qualitatively from HDFS_v1's
block events.
**Hardware.** `baseline-8vcpu-32gib`, provisioned for the run and
torn down after.
**Outcome: the run did not complete — it produced a product finding
instead.** The B2 store build was OOM-killed at **31.5 GiB RSS**: the
miner mints templates without bound on this corpus shape (template
ids ≥ 56,199 by the 1.8 GiB subset mark, busiest template covering
0.67 % of 8.37 M rows; memory ~linear at ≈2× corpus bytes). Two
bench-side pathologies were found and fixed en route — the eager
corpus load (#350, now streaming: 1.3 GiB flat over hours) and a
quadratic harness snapshot capture (#351, ~400× store-build speedup;
gdb stacks exonerate the miner's CPU path). **RFC 0023 (bounded
template memory) is the response; its RFC0023.7 criterion is this
exact run completing.**

What did land before the kill (recorded as diagnostic):

| bench | result | timing | pruning |
|---|---|---|---|
| `b2/synthetic/{2k,20k,100k}` | result held constant | 2.72 / 6.33 / 19.7 ms | sub-linear in corpus size |
| `b2/real-corpus` (1.1 GiB subset) | windowed 1 h → 1 row | 6.31 ms | 5/6 row groups pruned |
| `b2/real-corpus` (1.8 GiB subset) | template 56199 → 55,751 rows full-span; windowed 1 h | windowed 6.31 ms | 10/11 row groups pruned by the window |

B2's *shape* — flat windowed latency, window-driven pruning — holds
wherever memory allows; the fragmentation itself (56 k templates,
busiest at 0.67 %) also means pillar #2's logical reduction fails on
this corpus shape, which is the same finding from the pruning side.
B1 did not reach its arms (stopped before the reference build once
the OOM trajectory was clear). No gate verdict is claimed from this
entry; the §8-band verdict waits on RFC 0023 + the rerun.

### 9.11 Results — 2026-07-04 (authoritative, `baseline-8vcpu-32gib`) — B1 / B2 at 16 GiB + RFC0023.7

**Purpose.** The §8 10–100 GiB band's first completed measurement (the
§9.10 attempt OOM'd), doubling as **RFC0023.7** (bounded mining must
complete this exact corpus under 8 GiB peak RSS) and the first B1/B2
readings at ≥ 10 GiB — where B2's formal target speaks.
**Corpus.** LogHub HDFS_v2 (bench-time fetch): 31 files,
17,240,888,465 bytes ≈ 16.1 GiB, 71,116,785 mined rows → 21 files /
80 row groups. B2 ran under the §3.3 Fixed severity baseline; B1
under the opt-in `OURIOS_CORPUS_SEVERITY=log4j` extraction (#350),
stated per the methodology rule.
**Hardware.** `baseline-8vcpu-32gib`, provisioned for the run, torn
down after. Git `19e0886` (RFC 0023 bounds + telemetry merged).

**RFC0023.7 — bounded mining at scale: PASS.** Peak RSS
**1.73 GiB** across both benches' store builds (5 s sampler), vs the
§9.10 OOM at 31.5 GiB on identical input — an 18× reduction, under
the 8 GiB bar with 4.6× headroom. Both benches completed (B2 phase
35 min; B1 including its zstd-19 reference build ~2.8 h).

**B1 — predicate pushdown vs `zstdcat | grep` (target: ≥ 10× at
1 GiB, widening to ≥ 100× at 100 GiB).** Query: severity `ERROR`,
full 16 GiB span.

| corpus | rows | RGs scanned | ourios bytes | reference bytes (zstd) | ourios | reference | speedup |
|---|---|---|---|---|---|---|---|
| HDFS_v2 | 24,030 | 54/80 | 19,284,044 | 548,344,798 | 116.76 ms | 13.545 s | **116×** |

Row count agrees **exactly** with the reference pipeline.

**B1 verdict: PASS (authoritative)** — the ≥ 100× mark projected for
100 GiB is crossed at 16 GiB. With §9.8's ~35–40× at ~1 GiB, the
measured trajectory confirms the widening the target predicted: the
reference's cost grows with corpus bytes while Ourios's grows with
the matching row groups.

**B2 — template-exact latency ∝ result, not corpus (formal target:
≥ 10 GiB, ≤ 200 ms for 10 k rows).**

| bench | result | timing | pruning |
|---|---|---|---|
| **`b2/real-corpus` windowed 1 h** | 78 rows | **5.60 ms** | **79/80 row groups pruned by the time window** (21 partitions) |
| `b2/real-corpus` full span | 56,234,257 rows | 124.70 ms | 80/80 scanned (count over the dominant class) |
| `b2/synthetic/{2k,20k,100k}` | result held constant | 1.92 / 3.70 / 10.3 ms | sub-linear in corpus size |

**B2 verdict: PASS (authoritative, first ≥ 10 GiB reading)** — the
windowed query answers in the same few-ms band as the ~1 GiB corpora
(§9.3/§9.8/§9.9): latency tracks the result, not the 71 M-row corpus.

**The fragmentation datum (§9.10's open question, quantified).** The
"busiest template" is id 0 — `NO_TEMPLATE`: under the default 20 k
ceiling, ~79 % of HDFS_v2's rows took the §6.3 parse-failure path
(bodies retained bit-faithfully; observable via
`ourios.miner.parse_failure.reason`, RFC0023.6). Template mining
contributes little *on this corpus shape* — and the B1/B2 numbers
above show the floor it degrades to (first-class-column + time
pruning over Parquet statistics) still clears every gate. Follow-up
noted: the B2 bench's busiest-template picker should exclude
`NO_TEMPLATE` so the full-span arm measures a true template-exact
query on such corpora.

**Assessment.** RFC 0023's §5 is fully discharged (this entry is the
`.7` record); the RFC flips `red → green` alongside this entry. The
§8-band thesis verdict on real, hostile-shaped production logs:
pruning compounds with scale (B1), result-bound latency holds (B2),
and the mining-fragmentation failure mode is now bounded, observable,
and priced.

### 9.12 Results — 2026-07-09 (indicative, local M-series) — otel-demo v8 capture: C1 / C2

_The run is dated 2026-07-09; its C2 verdict was re-scored under the
per-service gate on 2026-07-10 (#444 / RFC 0006 §3.4.3), so the
resolution dates below post-date the heading._

**Corpus.** `corpus/otel-demo-v8` (published GitHub release): a
**48-hour** OTel-Demo 2.2.0 capture at 150 locust users with the
`adFailure` + `paymentFailure` feature flags active — 690,355 OTLP
LogsData batches / 4,948,596 log records / 2.96 GB uncompressed, the
largest and most hostile real capture to date (deliberately injected
failure modes, multi-service, long-horizon). Calibration manifest at
`testdata/calibration/otel-demo-v8.json` (RFC 0024 §3.1).

**C1 — bit-identical reconstruction: PASS, perfect.** The corpus
holds **4,948,596** records (the calibration manifest's count); 17 of
them (all kafka, 0.0003 %) took the §3.3 lossy-flag path with their
bodies retained, and C1 = 1.000000 over the remaining 4,948,579 rows
— the honesty contract holds at 4.9 M rows through failure-mode
churn.

**C2 — template-count convergence (bar: ratio ≥ 0.5 at 1 M lines,
evaluated per service since #444): PASS.** Under the per-service gate
(RFC 0006 §3.4.3, amended 2026-07-10) the corpus passes: the only
service that clears the 1 M-line evaluation floor is **cart**, which
converges at ratio **1.000** with two templates. Every other service
abstains for want of volume; the whole-corpus ratio (**0.199**, end
template count **14,631**, sample cadence 4,833) is retained below as
a diagnostic — it is a category error to grade a multi-service corpus
as one Drain stream (§3.4.3 rationale). The per-service decomposition
(splitting on `service.name` and re-running the gates per service)
localises the whole-corpus fragmentation completely:

| service | lines | end templates | C2 |
|---|---|---|---|
| cart | 2,756,331 | 2 | ratio 1.000 **PASS** |
| recommendation | 971,490 | 17 | abstain (< 1 M) |
| currency | 597,259 | 1 | abstain (< 1 M) |
| ad | 486,726 | 3 | abstain (< 1 M) |
| **kafka** | **136,790** | **14,608** | abstain (< 1 M) |

The gate folds over the gated services (those ≥ 1 M lines): cart is
the sole such service and it passes, so the corpus passes. cart clears
the formal gate at 2.76 M lines with **two** templates; the smaller
services abstain below the 1 M-line floor, so they are not graded —
though their *observed* counts (1–17 templates over 0.5–1.0 M lines)
sit at the same near-flat convergence. The kafka broker, also
abstaining, is the outlier: it mints 14,608 templates on 2.8 % of the
lines. Mechanism
(measured): kafka's cleaner logs emit **3-token lines whose third
token is a unique offset-bearing path**
(`Deleted log /tmp/kafka-logs/…/00000000000000000429.log.deleted.`,
11,651 distinct) — one varying token in a 3-token line is similarity
2/3 ≈ 0.67, below the strict 0.7 threshold (§3.1 no-silent-merges),
so each line mints a template; the 4-token siblings of the same
family (0.75) merge fine. The failure-flag confound turned out to be
a red herring. **#444** settled how to handle the fragmentation
(2026-07-10, maintainer-approved): of the three options — tokenizer
masking, length-aware thresholding,
and accept-and-scope-C2-per-service — **option 3 shipped** (the
per-service gate, RFC 0006 §3.4.3, PR #451); masking is parked as
a future strategic RFC (no commitment; a Collector `transform` or
`redaction` processor can polish high-cardinality infra tokens
upstream) and length-aware thresholding was rejected. The safety story held
throughout (bounded memory per RFC 0023, per-service C1 perfect).

The per-service decomposition is now the **first-class bench gate**
(`ourios-bench --gates c2` prints it whenever any service bucket exists
— distinct `service.name` values plus any `<unknown>`/`<other>`, so a
single-service or plain-text corpus shows its one gated row too);
template creation is a globally-monotonic
event attributed to the minting service, so per-service creations
partition the whole-corpus count exactly (2 + 17 + 1 + 3 + 14,608 =
14,631) in `O(services)` memory — no per-service id set. As of #444
(option 3) this decomposition **is** the gate: C2 is evaluated per
service and folds over the services that clear the 1 M-line floor,
with the whole-corpus ratio kept as a diagnostic (RFC 0006 §3.4.3).

**What the fragmentation actually costs — B2 pricing (indicative,
local M-series).** Running the B2 windowed query on the fragmented
(kafka) vs. converged (cart) service isolates the impact:

| service | templates | 1 h-window query | row groups pruned |
|---|---|---|---|
| cart | 2 | 3.66 ms | 48 / 49 |
| kafka | 14,608 | 3.40 ms | 48 / 49 |

The deployed **time/column pruning floor is identical** whether a
service has 2 templates or 14,608 — a 1 h window prunes 48 of 49 row
groups either way (reconfirming the RFC 0023 graceful-degradation
result on a fresh corpus). Fragmentation does **not** cost query
*latency* or pruning. What it costs is template-exact query
*precision*: probing cart's dominant template (id 1 in this run — a
run-specific identifier, not a canonical one) recovers 1.78 M / 2.76 M
rows (one template is most of the corpus) but only 11,523 /
136,790 on kafka, because kafka's dominant event is scattered across
~11,651 ids — a single `template_id` probe recovers only that one id's
slice (11,523 rows), not the full dominant event. So the
fragmentation is a **query-capability / thesis-value** tradeoff, not a
performance one; the pruning path degrades to the first-class-column
floor unharmed. #444 **accepted** that tradeoff on hostile infra logs:
the per-service gate makes C2 acceptance honest without masking, and
any future masking is deferred to an upstream Collector processor or a
dedicated RFC.

### 9.13 Results — 2026-07-12 (indicative, `ci-runner`) — RFC 0031 comparative program vs Grafana Loki (runs #8–#18)

**Purpose.** The first recorded numbers for the RFC 0031 comparative
program — Ourios against Grafana Loki, the incumbent `CLAUDE.md` §1
defines the project against. These are the §7 **calibration inputs**
the RFC's open questions ask for, not gate verdicts: the `L`-gate
margins are the RFC's *proposed* values (`M_L1..M_L4 = 10`,
`F_L6 = 3`, wired as `ComparativeMargins::default()`), the §5
gate scenarios (RFC0031.2–.11) are still red stubs, and the harness
**reports** each pair under its provisional margin rather than
asserting it. Every "PASS"/"fail" below is provisional pending the
§7 freeze — a maintainer step; the open inputs are enumerated in
point (4) of the closing **Assessment**.

**Corpus.** `corpus/otel-demo-v8` (the §9.12 capture): 4,948,596
log records, 2.96 GB uncompressed — the RFC 0031 §3.3 headline
corpus (real OTLP, failure flags active, kafka fragmentation and
all). Both systems ingest the identical OTLP stream; an OTLP
`partialSuccess` in any push response fails the run, so neither
side can silently drop lines.
**Reference system.** `grafana/loki:3.5.3`, digest-pinned
(`sha256:3165cecce301ce5b9b6e3530284b080934a05cd5cafac3d3d82edcb887b45ecd`),
single-binary mode, fed over its native
OTLP endpoint. Flag deviations from stock are documented below —
all ingest-replay accommodations, all in Loki's favour, per the
§3.7 anti-strawman commitment.
**Hardware.** `ci-runner` — **indicative, not the §1 baseline**;
the authoritative `baseline-8vcpu-32gib` run remains a maintainer
opt-in per RFC 0031 §3.2. Bytes-read, the primary channel, is
CPU-insensitive by construction, but nothing here is quoted as
authoritative.
**Runs.** `comparative-bench.yml` dispatch runs (curated by hand as
ever — no workflow writes §9), each with one harness delta under
test. Counted runs are equivalence-gated passes over the full
corpus; the two diagnostic failures (#11/#13) are listed with
exactly what they carry:

| run | workflow run id | delta under test |
|---|---|---|
| #8 | 29171354194 | honest-metric baseline (§3.6 amendment wired) |
| #9 | 29174022848 | + single-pass count/materialize scan (#485) |
| #10 | 29174342843 | + late materialization (#486) |
| #11 | 29186113326 | L3 diagnostic: Loki 0-rows, pre-salvage panic — no counted numbers |
| #12 | 29188179299 | + L3 trace pair (#487/#488) |
| #13 | 29189430335 | L3 diagnostic recurrence (on the #489 branch): L3 timed out; the salvaged report's other pairs are counted where tabulated |
| #14 | 29190408893 | + `trace_id`/`span_id` blooms (#489; pre-merge on the PR branch, since merged) |
| #15 | 29192897795 | + L1 template pair (#492; pre-merge, since merged) |
| #16 | 29199815903 | + selective-resource diagnostic, first picker (produced a vacuous duplicate of the L6 `k=100` pair — the fix is what #493 merged; the run's L1/L3 pairs measured and passed, so it counts toward the streaks) |
| #17 | 29203804795 | + selective-resource diagnostic pair, fixed picker (#493; pre-merge, since merged) |
| #18 | 29210202343 | + latency_p50 channel (#495; pre-merge, since merged) — bytes unchanged from #17; adds the §3.6 latency numbers below |

In **every** counted run, RFC0031.1 result-set equivalence held on
every pair: the two systems' answers, keyed
`(timestamp_unix_nanos, body_bytes)`, were multiset-identical at
4.9 M-record scale. Runs #11/#13 were L3-flicker diagnostics (an
ingester-visibility artifact, fixed in #490 — see the deviations
list); their table rows above note exactly what each carries.
Every dispatched run
appears in the table, and the per-class tables below carry a row
for every run in each quoted streak (L1: #15/#16/#17; L3:
#14/#15/#16/#17), so the streaks audit from this entry alone.

**The metric (§3.6 as amended 2026-07-12).** The Ourios figure is
the **total** bytes fetched from object storage per query: count
scan + row materialization + template-registry derivation. Loki is
reported on **two channels**: **storage-side** (query-stats
`compressedBytes + headChunkBytes` — the conservative
apples-to-apples counterpart of Ourios's fetched compressed-Parquet
bytes; the harness evaluates gates primarily on this) and
**`totalBytesProcessed`** (decompressed engine-side work, which
overstates Loki's storage reads by the chunk compression ratio;
reported as context). Which channel the frozen §7 gates ride is an
open maintainer decision.

**Program history — the biased ruler, retired.** Runs #5–#7
predate the §3.6 measurement-fidelity amendment and measured the
Ourios side as the **count scan alone** (e.g. run #7's severity
figure of 609,498 B and its "146.9×"-style ratios), silently
excluding the row-materialization and registry IO while Loki's
counterpart figure includes delivering results. Those runs are
program history only and are **not citable**; every number below is
on the honest total.

**L1 — template-exact lookup (must-win, the flagship class):
provisional PASS, widest margins.** Pair: `template_id == 4323`
(2 rows) vs the LogQL line-filter needle `"Updated
connection-accept-rate max connection creation rate to"` over every
stream — the picker proves the two select identical row sets before
the pair counts. Loki has no template concept, so its honest
equivalent is a substring scan of the whole corpus; Ourios rides the
writer's existing bloom filter on `template_id`.

| run | ourios bytes | loki storage-side | loki processed | storage | processed |
|---|---|---|---|---|---|
| #15 | 1,358,683 | 104,825,428 | 2,468,065,726 | **77.2×** | **1,816.5×** |
| #16 | 1,358,683 | 105,191,956 | 2,469,772,352 | 77.4× | 1,817.8× |
| #17 | 1,358,683 | 105,579,510 | 2,474,713,321 | 77.7× | 1,821.4× |

Above the provisional `M_L1 = 10` on **both** channels, in every
run since the pair landed (third consecutive pass at #17). The
Loki side is structural: no template id → nothing to prune with.

**L3 — trace correlation (must-win, OTLP-native): provisional PASS
after blooms.** Pair: every log line for one `trace_id` (9 rows).
`trace_id` is high-cardinality by construction, so it cannot be a
Loki label (§3.3's machine-checked disallowlist); Loki's honest
query is a structured-metadata filter over **all** streams.

| run | ourios config | ourios bytes | loki storage-side | loki processed | storage | processed |
|---|---|---|---|---|---|---|
| #12 | no bloom — `trace_id` column scanned corpus-wide | 72,935,984 | 102,835,803 | 2,419,117,783 | 1.41× | 33.2× |
| #14 | + `trace_id`/`span_id` blooms (#489) | 4,812,668 | 105,353,837 | 2,476,749,585 | **21.9×** | **514.6×** |
| #15 | reproduction | 4,812,668 | 102,133,866 | 2,404,486,169 | 21.2× | 499.6× |
| #16 | reproduction | 4,812,668 | 104,656,570 | 2,456,853,969 | 21.7× | 510.5× |
| #17 | reproduction | 4,812,668 | 105,251,547 | 2,465,855,695 | 21.9× | 512.4× |

Run #12 is the honest before-picture: without blooms Ourios itself
had to fetch the `trace_id` column corpus-wide, and the storage-side
ratio (1.41×) was nowhere near the margin. The blooms (implemented
in #489; the RFC 0005 §3.6 amendment recording them, with this as
its measured evidence, is #491) collapse
the fetch 15×, and the pair has now passed the provisional margin
on both channels three runs in a row. As with L1, Loki's side is
structural: a trace cannot be pre-narrowed to a label stream, so it
scans and decompresses everything in the window.

**L2 — severity predicate (must-win family): parity-plus
storage-side, ~33× processed — not a provisional 10× pass.** Pair:
lowest-volume single-`severity_text` band on the highest-volume
service, full corpus span, 1 row. The run series doubles as the
read-path optimisation ledger (component split: count scan +
materialize + registry):

| run | lever | ourios bytes (count + mat + reg) | loki storage | loki processed | storage | processed |
|---|---|---|---|---|---|---|
| #8 | baseline | 4,270,091 (609,498 + 3,146,731 + 513,862) | 2,880,784 | 89,184,711 | 0.67× | 20.9× |
| #9 | single-pass scan (#485) | 3,660,593 (0 + 3,146,731 + 513,862) | 3,158,323 | 98,114,703 | 0.86× | 26.8× |
| #10 | late materialization (#486) | 2,549,129 (0 + 2,035,267 + 513,862) | 2,751,834 | 85,261,718 | 1.08× | 33.4× |
| #12 | reproduction (no L2 delta) | 2,549,129 | 2,779,800 | 86,255,901 | 1.09× | 33.8× |
| #13 | reproduction | 2,549,129 | 3,349,897 | 98,253,343 | 1.31× | 38.5× |
| #14 | reproduction | 2,549,129 | 3,224,893 | 100,044,070 | 1.27× | 39.2× |
| #15 | reproduction | 2,549,129 | 2,688,942 | 83,216,895 | 1.05× | 32.6× |
| #16 | reproduction | 2,549,129 | 2,673,545 | 82,919,233 | 1.05× | 32.5× |
| #17 | reproduction | 2,549,129 | 3,224,528 | 100,198,466 | 1.26× | 39.3× |

(Run #8's Loki side: 2,880,784 storage / 89,184,711 processed.)
Across the later reproductions the storage-side ratio sits at
**1.05–1.31×** and processed at ~33–39×, the spread being entirely
Loki-side wobble (below). Reading: on the honest metric Ourios
went from *losing* the storage channel (0.67×) to parity-plus via
two read-path fixes, and wins decisively on engine work — but this
is **not** a 10× storage-side pass, and no amount of wobble makes
it one. The remaining named levers: the constant **513,862 B**
template-registry derivation, 20–29 % of every small-answer query's
total (the RFC 0033 cached-template-map candidate), and write-side
page/row-group sizing.

**Time-window browses (L6 floor family): published loss on the
storage channel.** Pairs: all lines of the highest-volume service
in a clean k-row window (the promoted-column bloom's worst case),
plus run #17's diagnostic — the same shape scoped to the
lowest-volume service ("ad", ~34 s window), where the
`service.name` bloom could in principle skip. Floor gate as
reported here: a **bytes-read floor analog** (Ourios ≤ 3× Loki,
i.e. ratio ≥ 0.33) — the harness applies the §7 `F_L6` factor to
this entry's bytes channels. Note the §5 gate as written
(RFC0031.7) defines the L6 floor on **latency p50** — measured in
run #18 (see the latency section below), where the gate as written
passes on all three window pairs; the bytes framing here remains
the conservative reporting channel pending the §7 freeze.

| run | pair | ourios bytes | loki storage-side | loki processed | storage ratio | processed ratio |
|---|---|---|---|---|---|---|
| #8 | k=100 | 5,094,790 | 16,250 | 63,595 | 0.003 fail | 0.012 fail |
| #8 | k=2000 | 9,736,285 | 72,524 | 1,809,523 | 0.007 fail | 0.186 fail |
| #10 | k=100 | 2,257,867 | 16,250 | 63,595 | 0.007 fail | 0.028 fail |
| #10 | k=2000 | 4,528,429 | 72,524 | 1,809,523 | 0.016 fail | **0.40 pass** |
| #17 | "ad" k=100 (diagnostic) | 1,757,489 | 31,616 | 687,043 | 0.018 fail | **0.39 pass** |

This is the honest loss the RFC's L6 disposition anticipated, and
it is published as §5 RFC0031.11 demands: on a browse-k-rows
query Loki reads only the tiny chunk slice its label stream + time
index point at, while Ourios pays fixed per-query costs (the
registry constant plus row-group-granularity materialization) that
dwarf a k-row answer. The #486 late-materialization fix halved the
loss and lifted k=2000 past the processed floor; storage-side stays
0.007–0.018 vs the 0.33 floor on current code. Run #17's
diagnostic sharpens the *why*: scoping to a low-volume service
improves Ourios only ~22 % and flips the processed floor to pass,
but there is **no bloom collapse** — v8's hour partitions each hold
roughly one row group containing **all** services, so the promoted
`service.name` bloom has nothing to skip. The tier-changing lever
is write-side layout (service clustering / row-group sizing —
hazard #4 territory, an RFC-level change), not query-side tuning.

**Latency (§3.6 channel, run #18 — the program's first).** Median
of 7 warm repetitions per pair per system, measured only on
correctness-verified pairs; Ourios timed in-process, Loki over
localhost HTTP (negligible at these magnitudes; stated because
latency is corroborating, not sole-gating):

| pair | ourios p50 | loki p50 | ratio (>1 = Ourios faster) |
|---|---|---|---|
| severity (1 row) | 82.0 ms | 875.0 ms | 10.7× |
| L3 trace (9 rows) | 74.6 ms | 24,101.9 ms | 323× |
| L1 template (2 rows) | 75.7 ms | 23,321.5 ms | 308× |
| window k=100 | 40.2 ms | 13.8 ms | 0.34 |
| window k=2000 | 85.9 ms | 294.8 ms | 3.43 |
| selective-resource k=100 | 38.8 ms | 51.2 ms | 1.32 |

Two findings this channel settles. First, the young-engine latency
risk the RFC hedged against ("a latency loss + bytes-read win =
sound architecture, young implementation") did **not** materialize:
Ourios answers every pair in 39–86 ms — a flat, fixed-cost-shaped
profile — while Loki spans 13.8 ms to 24.1 s, and on the needle
classes the wall-clock gap is interactive-vs-batch (75 ms vs 23–24
seconds). Second, **scenario RFC0031.7 evaluated as written — on
latency — PASSES on all three window pairs** (0.34, 3.43, 1.32,
all ≥ 1/3 at `F_L6 = 3`), and Ourios is outright *faster* on two of
the three; the storage-channel loss published above is real as a
bytes statement, but the RFC's own L6 gate holds the floor. Which
channel the frozen L6 gate uses is part of the §7 decision.

**Determinism note.** For repeated measurements of the same build
and configuration, Ourios's bytes are **byte-identical** (the store
build is deterministic) — differences between runs are exactly the
harness/optimisation deltas the table names, which is what lets the
run series read as an optimisation ledger. Loki's storage-side
figure wobbles run to run (severity pair: 2.67–3.35 MB) with chunk
boundaries and flush timing; ratios quoted against Loki carry that
band.

**Documented Loki flag deviations (all in Loki's favour, per
§3.7).** The committed harness starts Loki with, and comments,
exactly these deviations from stock:

- `-validation.reject-old-samples=false` — the frozen corpus is
  weeks old; stock Loki would reject the replay outright.
- `-querier.query-ingesters-within=0` — stock Loki (default 3 h)
  skips ingesters for queries over weeks-old ranges, making rows
  still in unflushed low-volume chunks **invisible** (the run
  #11/#13 L3 flicker; diagnosed via `ingester.totalReached: 0`,
  fixed in #490). Disabling the cutoff means ingesters are always
  consulted — without it Loki's answer to an old-range query is
  silently incomplete.
- Raised ingestion + per-stream rate limits
  (`-distributor.ingestion-rate-limit-mb=512`,
  `-distributor.ingestion-burst-size-mb=1024`,
  `-ingester.per-stream-rate-limit=512MB`,
  `-ingester.per-stream-rate-limit-burst=1GB`) — replay is
  far faster than the capture's real-time rate.
- Raised internal gRPC message caps
  (`-server.grpc-max-recv-msg-size-bytes=16777216`,
  `-server.grpc-max-send-msg-size-bytes=16777216`) — runs
  #2–#4 failed on the same ~5.27 MB internal message regardless of
  our outer batch size: a single kafka-service LogsData line's
  content alone inflates past Loki's stock 4 MiB internal cap.
  Raising it (standard operator tuning) lets Loki accept the data
  at all, preserving the identical-ingest precondition the
  equivalence check requires.

**Assessment.** (1) The two classes the thesis stakes itself on
hardest — L1 template lookup and L3 trace correlation — pass their
provisional must-win margins on **both** channels, reproduced
across three consecutive runs, and in both cases Loki's cost is
structural rather than tuning: no template concept, and no way to
index a trace id. (2) L2 is parity-plus on storage and a ~33×
processed win, honestly short of a 10× storage claim, with two
named levers still on the table. (3) The window browses are a
published storage-channel loss whose mechanism is understood
(fixed per-query costs vs v8's one-row-group-per-hour layout);
the lever is write-side and RFC-sized. (4) Nothing here is frozen:
the §7 inputs — the primary metric channel (storage-side vs
processed), the must-win margins and floor factors, and whether
the time-window pairs reclassify from gated floor to diagnostic —
are **open maintainer decisions**, and this entry is the
calibration evidence for them, not their resolution.

### 9.14 Results — 2026-07-13 (indicative, `ci-runner`) — comparative run #20: frozen gates on `main`, RFC 0033 acquisition

First dispatch on `main` after the §7 partial freeze **and** after the
RFC 0033 cached template map merged (#511–#513). Job: run #20
(29255000054), exit 0.

**Frozen gates.** All asserting gates pass on `main` — `M_L1`/`M_L3`
storage margins and the `F_L6` latency floors held; equivalence held
on every pair. The dispatch is functioning as the regression gate the
freeze intended (run #19 proved it on the branch; this run proves it
on `main`).

**RFC 0033 acquisition (the run's purpose).** Every pair reports:

```
template-map acquisition (RFC 0033): cold (audit fold, 513862 B; no artifact published)
```

- The registry component is **byte-identical to run #8's baseline**
  (513,862 B constant per body-rendering query): the cache regressed
  nothing, exactly as the advisory design promised.
- But the write-through **never published** on this corpus, so no
  pair ever ran warm and the RFC0033.6 corpus gate
  (`warm/cold ≤ 1/10`) could not be measured.
- The explanation consistent with the run's outputs is §3.2's size
  abstention: the artifact is *uncompressed JSON* carrying every
  `(template_id, version)` canonical template string, while the
  513,862 B it must undercut is *zstd-compressed Parquet* of the
  same strings (plus their event history). On v8's template set the
  JSON evidently meets or exceeds the fold, and the guard refuses a
  publish that would make warm acquisition cost more bytes than the
  fold it replaces. (A publish IO failure would leave the same
  "no artifact" label; the §3.7 publish-outcome telemetry
  distinguishes the two in a served process, but the bench harness
  does not export metrics — the amendment run should print the
  outcome explicitly.)

**Consequences recorded.**

1. RFC 0033 status reverted `green → red` (this PR): RFC0033.6's
   corpus arm is undischarged. The local-shape arm (55.8× on the
   64-event fixture) stands.
2. `M_L2` stays frozen-deferred — §7's unfreeze condition (the
   RFC 0033 warm measurement on the headline corpus) was not met.
3. The lever is an artifact encoding amendment (`format_version` 2,
   compressed body). The same template strings zstd-compress into
   the 513,862 B audit Parquet *with* full event history alongside,
   so a compressed artifact is expected to land well below the fold
   size — to be measured, not assumed. Abstention semantics stay:
   publish only when the artifact beats the fold.

### 9.15 Results — 2026-07-14 (indicative, `ci-runner`) — comparative run #21: the v2 compressed artifact publishes and runs warm

Dispatched from the RFC 0033 v2 implementation branch (PR #522, the
measure-before-merge step). Run 29343438434.

**The RFC 0033 answer.** The zstd artifact **published** on the
corpus (no abstention — the run #20 ambiguity is resolved by the new
per-pair outcome labels), and **every measured pair ran warm**:

```
template-map acquisition (RFC 0033): warm (one artifact GET, 187904 B compressed)
```

- warm = **187,904 B** (the compressed artifact, GET cost) vs
  cold = **513,862 B** (the audit fold, byte-identical to run #8) —
  **warm/cold ≈ 1/2.73**, a ~326 KB cut off every body-rendering
  query's honest total.
- The original RFC0033.6 ratio gate (`≤ 1/10`) does **not** pass on
  this corpus: the artifact is O(live template state), the fold is
  O(audit history), and otel-demo-v8 is young — the amended gate
  (`≤ 1/2`, dated 2026-07-14 in the RFC) asserts the real margin and
  ages upward. See the §5.6 amendment for the full argument.

**The test failure is not an Ourios finding.** The run exited 1 on
one pair: `loki returned 0 of 9 expected rows for [trace
correlation, L3] before timeout` — the Loki-side low-volume-chunk
race (the run #12-era flicker), resurfacing on the shared runner
despite the #490 flag fixes. All other pairs measured; the report
and every RFC 0033 number printed before the panic. A rerun for a
clean L3 pair is queued as run #22.

### 9.16 Results — 2026-07-14 (indicative, `ci-runner`) — runs #22 and #23: the v2 artifact asserting, M_L2 unfrozen

Two dispatches after the RFC 0033 v2 merge (#522):

- **Run #22** (29352282162, from `main`): exit 0 — the clean-record
  run. All then-frozen gates passed, the L3 pair measured cleanly
  (run #21's Loki-side flake did not recur), and every pair ran warm
  on the compressed artifact.
- **Run #23** (29353634499, from the `M_L2`-unfreeze branch): exit 0 —
  the first run with the full assertion set live. L2 processed
  (PRIMARY, frozen 10) **43.97×**; L2 storage-side floor (frozen
  11/10) **1.49×**; L1 storage 108.3× and L3 storage 24.9× against
  their frozen 10s; latency floors held; and the RFC 0033 §5.6
  acquisition gate asserted **warm = 187,905 B compressed on every
  pair** against the 513,862 B fold (ratio ≈ 1/2.73, gate ≤ 1/2).

With #528 merged, §7's measurable gates (M_L1, M_L2, M_L3, F_L6) are
all enforcing on every comparative dispatch; M_L4/F_L7 stay deferred
until measured. RFC 0033's §5 is fully discharged: the corpus arm
passed as measured (#21), and passed again as an asserting gate
(#23) — the status flips red → green with this record.

### 9.17 Results — 2026-07-17→18 (indicative, `ci-runner`) — L4 frequency aggregation measured (PR #536 arc)

The last unmeasured must-win class. The L4 workstream's own dispatch
sequence (~23 real `comparative-bench` runs across the arc — a
numbering distinct from §9.16's) fixed three genuine harness bugs
early (`LogQL` escaping, a control-flow ordering bug, a missing picker
row ceiling), then spent the balance of the runs on a persistent
completeness shortfall that no harness-side fix closed: **Loki never
returned 100% of any L4 candidate's expected rows on this corpus.**
Every mechanism checkable from the harness side was ruled out
directly — exact `(timestamp, body)` ingester dedup (corpus analysis
found zero collisions), push-path drops (`partial_success` asserted
clean on every push), Loki's own `warn`/`error` logs (silent), and its
`loki_discarded_samples_total` accounting (zero, of any kind). The
residual matches open upstream
[grafana/loki#10658](https://github.com/grafana/loki/issues/10658)
(wide-time-range queries silently missing a small percentage of lines,
no maintainer-identified root cause). RFC 0031 §7 records the
resulting amendment: `L4_COMPLETENESS_MARGIN = 0.90`, checked per
`group_key` with phantom-cell and per-key-overcount hard-fails — the
full five-iteration comparator design trail lives there.

The measured pair (picker floors `L4_MAX_ROWS = 100_000`,
`L4_MIN_AVG_INTERVAL_SECONDS = 100` — lower-frequency candidates
measure more completely; mechanism uncharacterized, NOT dedup):
`template_id=60` ("Periodic task \<type\> generated"), `param(0)`,
`bucket(12h)`, 1,197 expected rows, group cardinality 4.

| run (workflow id) | completeness | storage-side (loki/ourios) | processed (loki/ourios) |
|---|---|---|---|
| #18-era first clean pass | equivalence held | 3.73× | 87.1× |
| 29598833238 (2026-07-17) | 1164/1197 = 97.2% | 3.72× | 86.8× |
| 29608796312 (2026-07-17) | 1141/1197 = 95.3% | — (run failed on the unrelated L3 flicker; L4 itself passed) | — |
| 29614831613 (2026-07-17) | 1149/1197 = 96.0% | 3.69× | 86.6× |

Ourios's side is constant at 47,995,205 B total (the honest §3.6
metric). Four consecutive equivalence-verified measurements in a
3.69–3.73× / 86.6–87.1× band: the shape mirrors L2 pre-freeze — a
strong processed-channel win with storage closer to parity. **`M_L4`
stays §7-deferred** (both channels reported, nothing asserted); the
proposed freeze shape on #498 is the L2 precedent (processed must-win
at 10×, storage informational).

Follow-on hardening, so the 2 h dispatch confirms rather than
discovers (#538/#499, closed via #539–#542): a mutation-tested
property suite over the margin comparator, per-pair completeness
recorded as a machine-readable artifact on every dispatch, a backdated
wide-time-range arm in the per-PR `loki-interop` job running the
dispatch's exact Loki flags (one shared constant — config drift
between the 1-minute test and the 2 h run is now unrepresentable), and
a dispatch class filter for targeted re-runs.
