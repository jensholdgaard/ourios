# Ourios

> *οὔριος* — the fair wind that fills a ship's sail.

Ourios is a log storage and query backend built on Apache Parquet, a
Drain-derived online template miner, and Apache DataFusion. The thesis:
**Parquet + template mining + DataFusion collapses the inverted index,
the compression layer, the storage tier, and the query engine into one
stack of off-the-shelf parts plus thin glue.** Log lines collapse to
`(template_id, params)` at ingest — a *logical* 50–200× reduction whose
payoff is query pruning (a selective query reads a handful of row
groups instead of scanning the corpus), not on-disk bytes that beat a
byte codec. Our job is the glue, plus the honest handling of the places
where template mining can go wrong.

The repository [README](https://github.com/jensholdgaard/ourios#readme)
is the short front door; this book is the design record and the depth
behind it.

## Project status

Pre-release, under active RFC-driven development. The full path — OTLP
in; WAL, miner, Parquet on object storage; logs-DSL / DataFusion
queries out — is implemented and tested behind RFC acceptance gates,
and the performance thesis is measured (including against Grafana
Loki) in [Benchmarks](./benchmarks.md). Signed pre-release binaries,
container images, and a Helm chart exist, but interfaces and the
on-disk schema can still move: treat everything as pre-1.0.

## How this book is organised

- **[Getting started](./guides/quickstart.md)** — run the single
  binary, [configure it](./guides/configuration.md), deploy it with
  [Docker](./guides/docker.md) or [Helm](./guides/kubernetes.md), and
  [lock it down](./guides/authentication.md) before a listener leaves
  localhost.
- **Architecture** — the load-bearing reading:
  [OTLP's log data model vs. the miner's view of
  it](./architecture/otlp-log-format.md),
  [hazards](./hazards.md) (where projects in this space die, and how
  we won't), [verification](./verification.md) (how an RFC criterion
  becomes a red-gate test becomes a green one), and the
  [glossary](./glossary.md).
- **[Benchmarks](./benchmarks.md)** — the thesis gates, stated so they
  could falsify the project, and every measurement against them with
  run IDs and caveats; plus the [roadmap](./roadmap.md).
- **[RFCs](./rfcs/index.html)** — every subsystem is specified before it
  is built. Each RFC pins `Given / When / Then` acceptance scenarios
  and climbs a maturity ladder
  (`drafted → specified → red → green → validated → accepted`); the
  frontmatter status tells you how much to trust it.
- **[Talks](./talks/0001-template-miner.md)** — lecture-length
  explanations of the ideas behind the RFCs, for when you want the
  background rather than the specification.

Contributions, RFC discussion, and push-back on the invariants are all
welcome — `CONTRIBUTING.md` and `CLAUDE.md` in the repository root
carry the governing conventions, including the project's intentionally
fully AI-assisted development process under human maintainer review.
