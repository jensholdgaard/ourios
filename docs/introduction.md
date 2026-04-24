# Ourios

> *οὔριος* — the fair wind that fills a ship's sail.

Ourios is a log storage and query backend built on Apache Parquet, a
Drain-derived online template miner, and Apache DataFusion. It is a
work-in-progress; this book is where the design lives.

## How this book is organised

- **Architecture** — the pillars, hazards, and shared vocabulary. The
  load-bearing reading for anyone new to the project.
- **Benchmarks** — the measurements that would falsify the thesis,
  stated as goals before any code exists to measure against.
- **RFCs** — design decisions in progress. Each RFC is a contract
  between the people working on Ourios about how a given subsystem
  will be built; once accepted and implemented, it graduates into an
  architecture document.
- **Talks** — lecture-length explanations of ideas from the RFCs, for
  when you want the background rather than the specification.

## Project status

Greenfield. No code has been written yet; the design artefacts in
this book are what exists today. Contributions, RFC discussion, and
push-back on the invariants are all welcome — see `CLAUDE.md` in the
repository root for the governing conventions.
