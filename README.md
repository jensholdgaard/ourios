# Ourios

[![CI](https://github.com/jensholdgaard/ourios/actions/workflows/ci.yml/badge.svg)](https://github.com/jensholdgaard/ourios/actions/workflows/ci.yml)
[![coverage](https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Fjensholdgaard%2Fourios%2Fbadges%2Fcoverage.json)](https://github.com/jensholdgaard/ourios/actions/workflows/ci.yml)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/jensholdgaard/ourios/badge)](https://scorecard.dev/viewer/?uri=github.com/jensholdgaard/ourios)
[![OpenSSF Best Practices](https://www.bestpractices.dev/projects/13499/badge)](https://www.bestpractices.dev/projects/13499)
[![Release](https://img.shields.io/github/v/release/jensholdgaard/ourios?filter=v*)](https://github.com/jensholdgaard/ourios/releases/latest)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust: stable](https://img.shields.io/badge/rust-stable%20%C2%B7%20MSRV%201.88-orange.svg)](rust-toolchain.toml)
[![Docs: mdBook](https://img.shields.io/badge/docs-mdBook-brightgreen.svg)](https://jensholdgaard.github.io/ourios/)
[![Artifact Hub](https://img.shields.io/endpoint?url=https%3A%2F%2Fartifacthub.io%2Fbadge%2Frepository%2Fourios)](https://artifacthub.io/packages/search?repo=ourios)

> **οὔριος** · _the fair, following wind that fills a ship's sail._
> Logs flow forward, under fair wind, with minimal friction.

**Ourios is a log storage and query backend** built on three off-the-shelf
parts:

- **Apache Parquet** — columnar storage on S3-compatible object storage,
  with predicate pushdown via row-group statistics, bloom filters, and
  page indexes.
- **A Drain-derived online template miner** — collapses log lines to
  `(template_id, params)` at ingest time: a *logical* 50–200× reduction
  that drives query pruning (not a bytes-vs-codec claim; see
  [RFC 0011](docs/rfcs/0011-a1-rescope.md)).
- **Apache DataFusion** — a production-ready vectorised SQL execution
  engine. We hand it logical plans; it does the work.

The thesis: this combination collapses the inverted index, the
compression layer, the storage tier, and the query engine into one stack
of off-the-shelf parts plus thin glue. (Compression here is Parquet's
byte codec plus the miner's *logical* template reduction — the
query-pruning win noted above, not bytes that beat a codec.) The novel
work is gluing the parts together honestly and handling the failure
modes of template mining rather than pretending they don't exist.

---

> [!NOTE]
> **Status: pre-release, under active RFC-driven development.** The full
> path — OTLP in; WAL, miner, Parquet on object storage; logs-DSL /
> DataFusion queries out — is implemented and tested behind RFC
> acceptance gates, and the performance thesis is measured (including
> against Grafana Loki) in [`docs/benchmarks.md`](docs/benchmarks.md).
> Signed pre-release binaries, container images, and a Helm chart exist
> (`v0.2.x`), but interfaces and the on-disk schema can still move:
> treat everything as pre-1.0.

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
   Mine the template once, key every occurrence by a small, stable
   `template_id`, and selective queries prune to the row groups that
   can contain their answer.
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
- Managed-service features. We ship a binary, container images, and a
  Helm chart.
- Being a SIEM. No correlation engine, no detection rules, no alerting
  DSL beyond what Grafana/Prometheus already provide.
- Being a Loki / Mimir / ClickHouse replacement in their full scope. If
  "just use $X" covers your use case, do that.

---

## Architecture, in one picture

```
                  ┌──────────────────────────────────────────────────┐
  OTel SDK /      │  OTLP gRPC (:4317) / HTTP (:4318)                │
  Collector ────► │  TLS/mTLS · bearer/OIDC · per-tenant binding     │
                  │       │                                          │
                  │       ▼                                          │
                  │  ┌───────────┐   ┌─────────────┐   ┌───────────┐ │
                  │  │   WAL     │──►│ template    │──►│  Parquet  │ │
                  │  │ (fsync,   │   │ miner       │   │  writer   │ │
                  │  │ then ack) │   │ (per-tenant)│   └─────┬─────┘ │
                  │  └───────────┘   └─────────────┘         │       │
                  │                    receiver              │       │
                  └──────────────────────────────────────────┼───────┘
                                                             ▼
                ┌──────────────┐              ┌──────────────────────┐
                │  compactor   │ ◄──────────► │ S3-compatible object │
                │ (background  │              │ store — per-tenant,  │
                │  merge)      │              │ hour-partitioned     │
                └──────────────┘              │ Parquet + blooms +   │
                                              │ audit stream         │
                                              └──────────┬───────────┘
                                                         │
                  ┌──────────────────────────────────────┼──────────┐
   logs DSL /     │  ┌──────────────┐  ┌──────────────┐  │          │
   JSON (:4319) ► │  │  DSL parser  │─►│  DataFusion  │◄─┘          │
   + MCP (/mcp) ◄ │  │ (own grammar)│  │  logical plan│             │
                  │  └──────────────┘  └──────────────┘             │
                  │                      querier                    │
                  └─────────────────────────────────────────────────┘
```

One binary, three roles — **receiver**, **querier**, and the background
**compactor**. Two orderings are load-bearing: a batch is fsync'd to the
WAL **before** it is acknowledged and before the miner touches it
(WAL-before-ack, [`CLAUDE.md`](CLAUDE.md) §3.4), and object storage is
the source of truth — local disk is only WAL and cache (§3.6). All
storage seams run against S3-compatible object storage
([RFC 0013](docs/rfcs/0013-object-storage.md),
[RFC 0019](docs/rfcs/0019-storage-backend-selection.md)); the local
filesystem remains the zero-dependency development backend, and the WAL
stays on local disk by design.

## What is implemented

Everything below is built RFC-first: each RFC pins `Given / When / Then`
acceptance scenarios, which land as failing (red-gate) tests before the
code that turns them green. Statuses live in each RFC's frontmatter —
33 RFCs and counting ([`docs/rfcs/`](docs/rfcs/README.md)).

- **Ingest** — OTLP gRPC + HTTP receiver with the full
  compliance-scenario set ([RFC 0003](docs/rfcs/0003-otlp-receiver.md),
  [RFC 0018](docs/rfcs/0018-otlp-log-spec-compliance.md)); WAL with
  fsync-before-ack and a SIGKILL crash-recovery test in CI
  ([RFC 0008](docs/rfcs/0008-wal.md)); the record-sink write path
  ([RFC 0014](docs/rfcs/0014-ingest-write-path.md)).
- **Template miner** ([RFC 0001](docs/rfcs/0001-template-miner.md)) —
  per-tenant Drain-derived trees, three-zone confidence matching,
  byte-identical reconstruction (lossy lines flagged and retained
  verbatim), audited merges and aliases, bounded memory at corpus scale
  ([RFC 0023](docs/rfcs/0023-bounded-template-memory.md)), and
  `ourios.miner.*` telemetry defined through an OTel weaver registry.
- **Storage** — per-tenant, hour-partitioned Parquet on S3-compatible
  object storage ([RFC 0005](docs/rfcs/0005-parquet-storage.md),
  [RFC 0013](docs/rfcs/0013-object-storage.md),
  [RFC 0019](docs/rfcs/0019-storage-backend-selection.md)); bloom
  filters on `template_id` / `trace_id` / `span_id`; promoted queryable
  attribute columns
  ([RFC 0022](docs/rfcs/0022-queryable-attribute-columns.md));
  background compaction ([RFC 0009](docs/rfcs/0009-compaction.md)); an
  audit-event stream.
- **Query** — a pipe-composable logs DSL with its own grammar (no
  DataFusion or SQL leaking through):
  `service == "api" and severity >= error | count by template_id |
  sort count desc | limit 10`, `resolves_to(42)`, half-open
  `range(...)`, plus a structured JSON surface
  ([RFC 0002](docs/rfcs/0002-query-dsl.md),
  [RFC 0007](docs/rfcs/0007-querier.md)); template-registry body
  rendering
  ([RFC 0017](docs/rfcs/0017-template-registry-query-rendering.md));
  the `drift from <t1> to <t2>` audit query
  ([RFC 0010](docs/rfcs/0010-audit-stream-queries.md)); the HTTP query
  endpoint ([RFC 0016](docs/rfcs/0016-query-serving-endpoint.md)); a
  cached template-map artifact is in flight
  ([RFC 0033](docs/rfcs/0033-cached-template-map.md)).
- **Security & tenancy** — enforced multi-tenant binding on every
  surface ([RFC 0026](docs/rfcs/0026-authentication-tenant-binding.md)),
  static bearer tokens and OIDC
  ([RFC 0029](docs/rfcs/0029-oidc-bearer-layer.md)), and TLS/mTLS with
  hot certificate reload on the listeners
  ([RFC 0030](docs/rfcs/0030-tls-mtls-listeners.md)).
- **Operations** — YAML config file with `${env:VAR}` substitution or
  pure `OURIOS_*` environment variables
  ([RFC 0020](docs/rfcs/0020-configuration-file.md)); signed releases
  with offline-verifiable provenance (cargo-dist); container images
  including `-static` / `-scratch` variants; a Helm chart validated by
  a deploy test; dogfooded telemetry (Ourios ships its own logs to an
  OTLP endpoint and exposes OTel-defined metrics); nightly fuzzing
  ([RFC 0015](docs/rfcs/0015-fuzzing-harness.md)); SHA-pinned CI and a
  cargo-deny supply-chain gate.

## Quick start

Grab a [signed release](https://github.com/jensholdgaard/ourios/releases/latest)
(or `cargo build --release -p ourios-server`) and run one process with
the receiver and querier roles on localhost:

```sh
mkdir -p /tmp/ourios/data /tmp/ourios/wal

OURIOS_BUCKET_ROOT=/tmp/ourios/data \
OURIOS_WAL_ROOT=/tmp/ourios/wal \
OURIOS_RECEIVER_ENABLED=1 \
OURIOS_RECEIVER_GRPC_ADDR=127.0.0.1:4317 \
OURIOS_RECEIVER_HTTP_ADDR=127.0.0.1:4318 \
OURIOS_QUERIER_ENABLED=1 \
OURIOS_QUERIER_HTTP_ADDR=127.0.0.1:4319 \
./ourios-server
```

Point any OpenTelemetry SDK or Collector at the OTLP ports (Ourios
speaks OTLP and nothing else; the tenant derives from `service.name`),
then query with the logs DSL:

```sh
curl -s http://localhost:4319/v1/query \
  -H 'X-Ourios-Tenant: checkout' \
  -H 'Content-Type: text/plain' \
  -d 'severity >= error | count by template_id | sort count desc | limit 10'
```

The [quickstart guide](docs/guides/quickstart.md) walks this end to end,
[configuration](docs/guides/configuration.md) covers the
`--config ourios.yaml` file form, and
[Kubernetes (Helm)](docs/guides/kubernetes.md) is the production
topology on object storage. Read
[authentication](docs/guides/authentication.md) before any listener
leaves localhost.

## Agents: the MCP surface

The querier serves the Model Context Protocol at `/mcp`
([RFC 0027](docs/rfcs/0027-mcp-query-surface.md)): the `query_logs`,
`list_templates`, and `template_drift` tools, plus the
`ourios://dsl-grammar` resource and the `ourios://query-schema`
cost-model resource
([RFC 0032](docs/rfcs/0032-query-schema-cost-model-resource.md)) that
tells an agent which query shapes prune and which scan before it spends
a query. Same auth, same tenant binding as the HTTP API.

## How it measures

[`docs/benchmarks.md`](docs/benchmarks.md) is the scoreboard: §7 defines
the thesis gates, §9 records every run with its run IDs, hardware class,
and caveats. Read the caveats before quoting anything. Where things
stand:

- **Pruning gates (B1/B2): pass on the hardware baseline** (8 dedicated
  vCPU / 32 GiB). Predicate-needle queries clear the ≥ 10× gate against
  a `zstdcat | grep` reference with exactly matching row counts, and
  windowed template-exact queries stay flat as the corpus doubles —
  result-bound, not corpus-bound — at 16 GiB scale.
- **Reconstruction (C1): 1.000000** — every non-lossy row reconstructs
  bit-identically; lossy rows are flagged and retained.
- **On-disk compression vs. zstd (A1): a recorded diagnostic, not a
  gate** ([RFC 0011](docs/rfcs/0011-a1-rescope.md)) — a whole-stream
  codec captures the same redundancy; the miner's reduction pays in
  pruning, and the doc says so plainly.
- **Against Grafana Loki**
  ([RFC 0031](docs/rfcs/0031-comparative-evaluation-loki.md); shared CI
  runner, so indicative — both systems ingest the identical OTLP stream
  and every counted pair is machine-checked for result-set
  equivalence): the two classes the thesis stakes itself on hardest
  pass their frozen 10× storage-bytes must-wins — template-exact lookup
  at ~77× and trace correlation at ~22× fewer bytes fetched — and on
  the program's latency channel the needle classes answer in ~75 ms
  against 23–24 s for Loki (308–323×). Time-window browses are an honest,
  documented loss on the storage-bytes channel (Loki's label + time
  index is genuinely better at "give me k recent rows"), while the
  RFC's latency floor for them passes as written. The frozen gates now
  assert on every comparative-bench dispatch, as a regression gate.

## Repository layout

```
crates/      # Rust workspace: ourios-{core,config,miner,wal,parquet,
             #   ingester,querier,server,telemetry,semconv,testgen,bench}
deploy/helm/ # the Helm chart
docs/        # mdBook source: guides, architecture, hazards, RFCs,
             #   benchmarks, verification, roadmap, glossary, talks
fuzz/        # cargo-fuzz harness (its own nightly-only workspace)
semconv/     # OTel semantic-convention registry (weaver source of truth)
templates/   # weaver codegen templates for crates/ourios-semconv
testdata/    # anonymised seed corpora; the multi-GB bench corpora are
             #   captured by CI workflows and published as corpus/* tags
```

## Documentation

The docs are an mdBook, published at
**<https://jensholdgaard.github.io/ourios/>** (source under `docs/`):

- [Guides](docs/guides/quickstart.md) — quickstart, configuration,
  Docker, Kubernetes, authentication.
- [`docs/rfcs/`](docs/rfcs/README.md) — how we make decisions, and the
  RFCs that specify everything implemented so far.
- [`docs/hazards.md`](docs/hazards.md) — where projects in this space
  die, and how we won't.
- [`docs/benchmarks.md`](docs/benchmarks.md) — the thesis gates and the
  measurements against them.
- [`docs/verification.md`](docs/verification.md) — how an RFC criterion
  becomes a red-gate test becomes a green one.
- [`docs/glossary.md`](docs/glossary.md) — terms of art (Parquet, Drain,
  row group, …).
- [`docs/talks/`](docs/talks/0001-template-miner.md) — lecture-length
  explanations, starting with the template miner.

## Development process

Every change that touches a pillar, invariant, or hazard goes through an
RFC with pinned acceptance scenarios; RFCs climb a maturity ladder
(`drafted → specified → red → green → validated → accepted`) defined in
[`docs/rfcs/README.md`](docs/rfcs/README.md), and the scenarios land as
failing tests before the implementation.

Ourios is also an experiment in how software gets built: development is
**intentionally fully AI-assisted** (Claude), with a human maintainer
owning direction, review, and every merge. AI-authored commits carry
`Co-Authored-By` trailers and clear the same CI, review, and RFC bar as
any human contribution — see [`CLAUDE.md`](CLAUDE.md) §9 and the
"tests are specifications" discipline in §6.2 for how the known failure
modes are handled.

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
