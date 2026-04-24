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

No numbers in this document are measured yet. They are the thresholds
the thesis asks us to hit; if we miss them on representative corpora,
the thesis is wrong and a pillar changes.

## 0. How to read this document

Every goal below carries two labels.

- **Scope** — `thesis-gate` or `tuning-goal`.
  - A `thesis-gate` failing on representative corpora means a pillar
    (`CLAUDE.md` §2) is wrong. The response is an RFC, not a sprint.
  - A `tuning-goal` failing means the design is sound but the
    implementation needs work. The response is a PR.
- **Bar** — `must-win`, `should-win`, or `stretch`.
  - `must-win` — shipping without it is shipping a broken claim.
  - `should-win` — expected on representative corpora; explained when
    missed.
  - `stretch` — aspirational; missing is not a bug.

A goal with scope `thesis-gate` and bar `must-win` is load-bearing for
the whole project. There are five of those below, marked `[THESIS]`.

## 1. Corpora and methodology

Before any goal is meaningful, the corpora and methodology must be
pinned — otherwise we will argue about numbers instead of about
architecture.

- **Public**: LogPAI corpora (HDFS, BGL, Spark, Apache, OpenSSH,
  Windows) — the same corpora the Drain paper reports on. Lets us
  reproduce published claims as a sanity floor.
- **Self-collected (deferred)**: at least one anonymised corpus per
  target deployment archetype. Proposed set:
  1. Structured Java/Spring service (well-templated, low entropy).
  2. Go service under Kubernetes (heterogeneous, mid entropy).
  3. Heterogeneous k8s aggregate across many services (high entropy,
     mixed formats).
- **Hardware baseline**: commodity cloud VM, 8 vCPU, 32 GiB RAM,
  gp3-class SSD. All `must-win` numbers are quoted against this
  baseline; scaling to larger hardware is a separate question.
- **Reference system**: `zstdcat <file.zst> | grep <pattern>`. The
  "naive alternative" the thesis beats or does not beat. Everything is
  quoted *relative to this*, not in absolute terms.

Goals quoted below assume this setup. When a goal is measured on a
different setup, the measurement is annotated.

## 2. Compression goals (Category A)

The core claim that template mining does useful work *before* byte
codecs run.

### A1 `[THESIS]` — End-to-end compression ratio vs. zstd-alone

- **Scope**: thesis-gate.
- **Bar**: must-win.
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
| A1 | Compression ≥ 3× over zstd-alone | Template mining is not pulling weight on this corpus class |
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

As of **2026-04-24**, no benchmark has been run. All targets are
aspirational. This document will gain a *Results* section as
measurements arrive, organised by goal id (A1, B1, …) with date,
hardware, corpus, and delta against target.

Reviewers: a PR that materially affects the hot path must either
(a) cite the benchmark result and its delta against the relevant
goal, or (b) explain why the hot-path effect is bounded below
measurability. "I did not run the benchmarks" is a PR rejection, per
`CLAUDE.md` §6.6.
