# Ourios

> **οὔριος** · _the fair, following wind that fills a ship's sail._
> Logs flow forward, under fair wind, with minimal friction.

**Ourios is a log storage and query backend** built on three off-the-shelf
parts:

- **Apache Parquet** — columnar storage on object storage, with predicate
  pushdown via row-group statistics, bloom filters, and page indexes.
- **A Drain-derived online template miner** — collapses log lines to
  `(template_id, params)` at ingest time, giving 50–200× compression
  before any byte-level codec runs.
- **Apache DataFusion** — a production-ready vectorised SQL execution
  engine. We hand it logical plans; it does the work.

The thesis: this combination collapses the inverted index, the compression
layer, the storage tier, and the query engine into roughly 15–20k lines of
Rust plus thin glue. The novel work is gluing the parts together honestly
and handling the failure modes of template mining rather than pretending
they don't exist.

---

> [!IMPORTANT]
> **Status: draft.** No code exists yet. This repository currently
> contains the design specification, the invariants we commit to, the
> hazards we refuse to paper over, and the RFC process by which we will
> build the rest. If you are here to run a binary, you are early.

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
   50–200× on the bytes.
2. The most common log query shape is "filter by time + attributes, fetch
   a bounded result." This is exactly what Parquet + DataFusion is
   already good at when the file layout is right.

See `docs/architecture/overview.md` for the long version.

## Two features nobody else has

- **`template:<id>` queries.** Filter by the mined template, not just by
  string match. "Show me every occurrence of the login-error template
  across every service" is a single bloom-filter probe per Parquet file.
- **`templates_drift(<service>)`.** Show templates that appeared or
  disappeared across a deploy boundary. A deploy that changes a log line
  from "user logged in" to "user authenticated" shows up here. This is
  how you catch log-schema drift at the time of the change instead of
  discovering it three weeks later when your alert stops firing.

## Non-goals

- Metrics and traces. OTLP logs only. Traces are linked by `trace_id`
  but not stored.
- Managed-service features. We ship a binary and a Helm chart.
- Being a SIEM. No correlation engine, no detection rules, no alerting
  DSL beyond what Grafana/Prometheus already provide.
- Being a Loki / Mimir / ClickHouse replacement in their full scope. If
  "just use $X" covers your use case, do that.

---

## Architecture, in one picture

```
                  ┌──────────────────────────────────────────────┐
  OTel collector  │  OTLP gRPC / HTTP                            │
  ─────────────►  │       │                                      │
                  │       ▼                                      │
                  │  ┌─────────────┐   ┌──────────┐   ┌────────┐ │
                  │  │ template    │──►│   WAL    │──►│ Arrow  │ │
                  │  │ miner       │   │ (fsync)  │   │ batches│ │
                  │  │ (per-tenant)│   └──────────┘   └────┬───┘ │
                  │  └─────────────┘                        │     │
                  │         ingester                         ▼     │
                  │                                   ┌───────────┐│
                  │                                   │ Parquet   ││
                  │                                   │  writer   ││
                  │                                   └─────┬─────┘│
                  └─────────────────────────────────────────┼──────┘
                                                            ▼
                                                    ┌───────────────┐
                                                    │  S3 / object  │
                                                    │    storage    │
                                                    └───────┬───────┘
                                                            ▲
                  ┌─────────────────────────────────────────┼──────┐
                  │                                   ┌─────┴─────┐│
                  │                                   │  Parquet  ││
                  │                                   │   reader  ││
                  │                                   └─────┬─────┘│
                  │                                         │      │
                  │  ┌─────────────┐   ┌──────────────┐    │      │
   logs DSL    ─► │  │   parser    │──►│  DataFusion  │◄───┘      │
   query       ◄─ │  │ (LogQL-ish) │   │  logical plan│           │
   response       │  └─────────────┘   └──────────────┘           │
                  │                       querier                 │
                  └──────────────────────────────────────────────┘
```

## Repository layout

```
crates/    # Rust workspace (empty for now)
deploy/    # Helm chart (empty for now)
docs/      # Architecture, hazards, RFCs, glossary
testdata/  # Anonymised log corpora for miner evaluation (empty for now)
.claude/   # Claude Code skills specific to this project
```

See `CLAUDE.md` for the full development contract and invariants.

## Documentation

- `docs/architecture/overview.md` — the deep dive. Start here.
- `docs/architecture/storage-layout.md` — Parquet schema and partitioning.
- `docs/architecture/template-miner.md` — Drain-derived design decisions.
- `docs/architecture/ingest-path.md` — OTLP → WAL → Parquet.
- `docs/architecture/query-path.md` — DataFusion + logs DSL.
- `docs/hazards.md` — where projects in this space die, and how we won't.
- `docs/glossary.md` — terms of art (Parquet, Drain, row group, …).
- `docs/rfcs/README.md` — how we make decisions.

## Governance and contributing

- `GOVERNANCE.md` — how decisions get made.
- `CONTRIBUTING.md` — how to contribute (including the RFC process).
- `CODE_OF_CONDUCT.md` — CNCF-aligned.
- `SECURITY.md` — vulnerability reporting.

## Licensing

Apache License 2.0. See `LICENSE`.

## Acknowledgements

- The Parquet project and the Apache Arrow ecosystem for making columnar
  storage on object storage a solved problem.
- Pinjia He et al. for the original Drain paper (ICSE 2017) and IBM for
  Drain3.
- The DataFusion and InfluxData teams for proving that a fast, vectorised,
  pluggable query engine can ship as an open-source library.

---

*Ourios is a draft specification and a direction of travel. It is not,
yet, a working backend. If you would like it to be, see
`docs/rfcs/README.md`.*
