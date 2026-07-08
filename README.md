# Ourios

[![CI](https://github.com/jensholdgaard/ourios/actions/workflows/ci.yml/badge.svg)](https://github.com/jensholdgaard/ourios/actions/workflows/ci.yml)
[![coverage](https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Fjensholdgaard%2Fourios%2Fbadges%2Fcoverage.json)](https://github.com/jensholdgaard/ourios/actions/workflows/ci.yml)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/jensholdgaard/ourios/badge)](https://scorecard.dev/viewer/?uri=github.com/jensholdgaard/ourios)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust: stable](https://img.shields.io/badge/rust-stable%20%C2%B7%20MSRV%201.88-orange.svg)](rust-toolchain.toml)
[![Docs: mdBook](https://img.shields.io/badge/docs-mdBook-brightgreen.svg)](docs/SUMMARY.md)
[![Artifact Hub](https://img.shields.io/endpoint?url=https%3A%2F%2Fartifacthub.io%2Fbadge%2Frepository%2Fourios)](https://artifacthub.io/packages/search?repo=ourios)
<!-- OpenSSF Best Practices: requires maintainer registration at
     https://www.bestpractices.dev — add the badge here once the
     project is enrolled. Codecov likewise needs the maintainer to
     install the app; the coverage badge above is self-hosted. -->


> **οὔριος** · _the fair, following wind that fills a ship's sail._
> Logs flow forward, under fair wind, with minimal friction.

**Ourios is a log storage and query backend** built on three off-the-shelf
parts:

- **Apache Parquet** — columnar storage, with predicate pushdown via
  row-group statistics, bloom filters, and page indexes.
- **A Drain-derived online template miner** — collapses log lines to
  `(template_id, params)` at ingest time: a *logical* 50–200× reduction
  that drives query pruning (not a bytes-vs-codec claim; see
  [RFC 0011](docs/rfcs/0011-a1-rescope.md)).
- **Apache DataFusion** — a production-ready vectorised SQL execution
  engine. We hand it logical plans; it does the work.

The thesis: this combination collapses the inverted index, the compression
layer, the storage tier, and the query engine into roughly 15–20k lines of
Rust plus thin glue. (Compression here is Parquet's byte codec plus the
miner's *logical* template reduction — the query-pruning win noted above,
not bytes that beat a codec.) The novel work is gluing the parts together
honestly and handling the failure modes of template mining rather than
pretending they don't exist.

---

> [!NOTE]
> **Status: pre-release, under active RFC-driven development.** The
> ingest path (OTLP → WAL → miner → Parquet) and the query path
> (logs DSL → DataFusion) are implemented and tested behind RFC
> acceptance gates; the performance thesis is being measured in
> [`docs/benchmarks.md`](docs/benchmarks.md). There is no packaged
> release yet — if you are here to download a binary, you are still
> early, but the code is real.

---

## Why another logs backend?

Existing backends optimise for different things:

- **Loki** optimises for cheap ingest; queries grep bytes and scale by
  throwing containers at them.
- **ClickHouse** optimises for general analytical SQL; logs are one
  workload among many.
- **Elasticsearch** optimises for full-text search with inverted indexes;
  storage cost and operational burden are the price.

Ourios optimises for **logs specifically** by exploiting two facts about
logs that other backends ignore:

1. Log lines are generated from a small number of `printf` templates.
   Store the template once, store the parameters per occurrence, save
   on the bytes.
2. The most common log query shape is "filter by time + attributes, fetch
   a bounded result." This is exactly what Parquet + DataFusion is
   already good at when the file layout is right.

## Two features nobody else has

- **Template-exact queries.** `template_id == 42` filters by the mined
  template, not by string match — "every occurrence of the login-error
  template, across every service" prunes to the row groups whose bloom
  filters can contain it. `resolves_to(42)` expands an operator-asserted
  alias set, so a template that was renamed across a deploy still answers
  as one query.
- **Drift as a first-class query.** `drift from -7d to now` reads the
  miner's audit stream and reports the templates that appeared, widened,
  or changed in the window. A deploy that changes a log line from "user
  logged in" to "user authenticated" shows up here — at the time of the
  change, not three weeks later when your alert stops firing.

## Non-goals

- Metrics and traces. OTLP logs only. Traces are linked by `trace_id`
  but not stored.
- Managed-service features. We ship a binary and (eventually) a Helm
  chart.
- Being a SIEM. No correlation engine, no detection rules, no alerting
  DSL beyond what Grafana/Prometheus already provide.
- Being a Loki / Mimir / ClickHouse replacement in their full scope. If
  "just use $X" covers your use case, do that.

---

## Architecture, in one picture

```
                  ┌──────────────────────────────────────────────────┐
  OTel collector  │  OTLP gRPC (:4317) / HTTP (:4318)                │
  ─────────────►  │       │                                          │
                  │       ▼                                          │
                  │  ┌───────────┐   ┌─────────────┐   ┌───────────┐ │
                  │  │   WAL     │──►│ template    │──►│  Parquet  │ │
                  │  │ (fsync,   │   │ miner       │   │  writer   │ │
                  │  │ then ack) │   │ (per-tenant)│   └─────┬─────┘ │
                  │  └───────────┘   └─────────────┘         │       │
                  │                    ingester              │       │
                  └──────────────────────────────────────────┼───────┘
                                                             ▼
                                                  ┌────────────────────┐
                                                  │   Parquet store    │
                                                  │ (per-tenant, hour- │
                                                  │   partitioned)     │
                                                  └─────────┬──────────┘
                                                            ▲
                  ┌─────────────────────────────────────────┼───────┐
                  │                                  ┌──────┴─────┐ │
                  │                                  │  Parquet   │ │
                  │                                  │   reader   │ │
                  │                                  └──────┬─────┘ │
                  │  ┌──────────────┐  ┌──────────────┐     │       │
   logs DSL    ─► │  │  DSL parser  │─►│  DataFusion  │◄────┘       │
   query       ◄─ │  │ (own grammar)│  │  logical plan│             │
   response       │  └──────────────┘  └──────────────┘             │
                  │                      querier                    │
                  └─────────────────────────────────────────────────┘
```

The order on the ingest side is load-bearing: a batch is fsync'd to the
WAL **before** it is acknowledged and before the miner touches it
(WAL-before-ack, `CLAUDE.md` §3.4). The durability contract for the
Parquet store is object storage as the source of truth (`CLAUDE.md`
§3.6, [RFC 0005](docs/rfcs/0005-parquet-storage.md)); the current
implementation writes that store to a local filesystem, an
implementation detail on the way to the contract, not a change to it.

## What is implemented

Everything below is built RFC-first: each RFC pins `Given / When / Then`
acceptance scenarios, which land as failing (red-gate) tests before the
code that turns them green. Statuses live in each RFC's frontmatter
([`docs/rfcs/`](docs/rfcs/README.md)).

- **Template miner** ([RFC 0001](docs/rfcs/0001-template-miner.md)) —
  per-tenant Drain-derived trees keyed by `(severity_number,
  scope_name)`, three-zone confidence matching, byte-identical
  reconstruction (lossy lines flagged and retained verbatim), the
  Ourios canonical body encoding for structured bodies, snapshot +
  WAL-replay recovery, an operator-driven audited alias map, and
  `ourios.miner.*` telemetry defined through an OTel weaver registry.
- **WAL** ([RFC 0008](docs/rfcs/0008-wal.md)) — append / sync / replay /
  segment rotation, with a SIGKILL crash-recovery test in CI.
  Checkpointing and truncation are still open.
- **OTLP receiver** ([RFC 0003](docs/rfcs/0003-otlp-receiver.md)) —
  gRPC + HTTP, the full compliance-scenario set green, tenant
  derivation per `ResourceLogs`.
- **Parquet storage** ([RFC 0005](docs/rfcs/0005-parquet-storage.md)) —
  per-tenant hour-partitioned files, canonical-JSON attribute and body
  columns, effective-timestamp windowing, an audit-event stream, and
  background compaction ([RFC 0009](docs/rfcs/0009-compaction.md)).
- **Querier + logs DSL** ([RFC 0002](docs/rfcs/0002-query-dsl.md),
  [RFC 0007](docs/rfcs/0007-querier.md)) — a pipe-composable DSL with
  its own grammar (no DataFusion or SQL leaking through):
  `severity >= error`, `template_id == 42`, `resolves_to(42)`,
  half-open `range(...)`, `| count by template_id | sort count desc |
  limit 10`, plus a structured JSON query surface and the
  `drift from <t1> to <t2>` audit query
  ([RFC 0010](docs/rfcs/0010-audit-stream-queries.md)).
- **Server binary** — one binary; the OTLP-receiver and compaction
  roles are served today, the served querier role is pending.

## How it measures

[`docs/benchmarks.md`](docs/benchmarks.md) §9 is the live scoreboard —
every claim below is recorded there with its run IDs and caveats. The
numbers so far are **indicative** (`ci-runner` hardware, not the
benchmark-baseline machine; an authoritative `baseline-8vcpu-32gib`
rerun is in progress):

- **Query pushdown (B1):** severity-needle queries over a ~1 GB corpus
  run 30–40× faster than a `zstdcat | grep` reference pipeline, with
  exactly matching row counts.
- **Windowed template queries (B2):** latency stays flat (~3–4 ms) as
  the corpus doubles — result-bound, not corpus-bound.
- **Reconstruction (C1):** 1.000000 — every one of 1.2M+ non-lossy rows
  reconstructs bit-identically; lossy rows are flagged and retained.
- **Compression (A1):** honest miss so far — ourios is ~0.82× of
  monolithic zstd-19 at ~1 GB, against a ≥ 3.0× gate. The gap is the
  structural price of a queryable columnar layout; the thesis currently
  rests on B1/B2, and §9 says so plainly.

## Repository layout

```
crates/      # Rust workspace: ourios-{core,miner,wal,parquet,ingester,
             #   querier,server,bench,semconv,telemetry}
docs/        # mdBook source: architecture, hazards, RFCs, benchmarks,
             #   roadmap, glossary, talks
semconv/     # OTel semantic-convention registry (weaver source of truth)
templates/   # weaver codegen templates for crates/ourios-semconv
testdata/    # anonymised seed corpora; the ~1 GB bench corpora are
             #   captured by CI workflows and published as corpus/* tags
```

See [`CLAUDE.md`](CLAUDE.md) for the full development contract and
invariants.

## Documentation

The docs are an mdBook (`book.toml`; source under `docs/`):

- [`docs/rfcs/`](docs/rfcs/README.md) — how we make decisions, and the
  ten RFCs that specify everything implemented so far.
- [`docs/hazards.md`](docs/hazards.md) — where projects in this space
  die, and how we won't.
- [`docs/benchmarks.md`](docs/benchmarks.md) — the thesis gates and the
  measurements against them.
- [`docs/verification.md`](docs/verification.md) — how an RFC criterion
  becomes a red-gate test becomes a green one.
- [`docs/roadmap.md`](docs/roadmap.md) — the path to an MVP.
- [`docs/architecture/otlp-log-format.md`](docs/architecture/otlp-log-format.md)
  — OTLP's log data model vs. the miner's view of it.
- [`docs/glossary.md`](docs/glossary.md) — terms of art (Parquet, Drain,
  row group, …).
- [`docs/talks/`](docs/talks/0001-template-miner.md) — lecture-length
  explanations, starting with the template miner.

## Governance and contributing

- [`CONTRIBUTING.md`](CONTRIBUTING.md) — how to contribute (including
  the RFC process).
- [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md) — CNCF-aligned.
- [`SECURITY.md`](SECURITY.md) — vulnerability reporting.

## Licensing

Apache License 2.0. See [`LICENSE`](LICENSE).

## Acknowledgements

- The Parquet project and the Apache Arrow ecosystem for making columnar
  storage a solved problem.
- Pinjia He et al. for the original Drain paper (ICSE 2017) and IBM for
  Drain3.
- The DataFusion and InfluxData teams for proving that a fast, vectorised,
  pluggable query engine can ship as an open-source library.
