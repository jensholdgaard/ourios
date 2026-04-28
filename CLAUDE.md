# CLAUDE.md — Ourios

> Status: **draft**. This is a greenfield project. Nothing has been
> implemented yet. These directives exist to make sure the first ten
> thousand lines are written with the same care as the hundred-thousandth.

You are contributing to **Ourios**, a log storage and query backend built
on Apache Parquet, a Drain3-derived template miner, and Apache DataFusion.
The name is classical Greek — *οὔριος*, "the fair wind that fills a ship's
sail." Logs flow forward, under a following wind, with minimal friction.

This document is your project context. Read it at the start of every
session before touching code. The governing loop for all work:
**gather context → act → verify → repeat.**

---

## 1. What Ourios is (and is not)

### Is
A single Rust binary with two roles (`ingester`, `querier`) that accepts
OTLP logs over gRPC/HTTP, extracts log templates online via a Drain-derived
algorithm, writes columnar Parquet files to object storage, and answers
queries via DataFusion with aggressive predicate pushdown.

The thesis: **Parquet + template mining + DataFusion collapses the
inverted index, the compression layer, the storage tier, and the query
engine into one stack of off-the-shelf parts plus thin glue.** Our job is
the glue, plus the honest handling of the places where template mining can
go wrong.

### Is not
- Not a metrics backend. OTLP metrics and traces are out of scope.
- Not a SIEM. We are not in the security-event-correlation business.
- Not a Loki/Mimir/ClickHouse clone. If the answer is "just use $X,"
  we should not be building this.
- Not a managed service. The project ships a binary and a Helm chart.

If a PR expands scope outside these lines, the default action is to reject
it and capture the idea as an RFC for later consideration.

---

## 2. The architectural pillars

Three choices are load-bearing. Changing any of them is an RFC-level
decision, not a PR-level one.

1. **Parquet as the on-disk format.** Columnar layout, per-column
   compression, predicate pushdown via min/max statistics, bloom filters,
   and page indexes. Query performance comes from skipping row groups via
   footer reads, not from scanning.
2. **Drain-derived online template mining.** Log lines collapse to
   `(template_id, params)` at ingest time. This is where the 50–200×
   compression comes from — before any byte-level codec runs. Correctness
   of this layer is the single biggest engineering risk in the project.
3. **DataFusion as the query engine.** We do not write a vectorised
   execution engine. We hand DataFusion logical plans and let it work.

See `docs/architecture/overview.md` for the full rationale.

---

## 3. The non-obvious invariants

These are the rules that, if violated, quietly corrupt user data or
destroy the project's value proposition. They override "keep it simple"
and "move fast." If you are about to break one, stop and open an RFC.

### 3.1 No silent template merges
A template merge that crosses semantic boundaries (e.g. merging "user
logged in" with "user logged out" because they share token structure)
corrupts the backend. A user searches for one and gets hits from both.

**Rules:**
- Similarity threshold is configurable, default strict (≥ 0.7).
- Every merge emits an audit event with old + new templates.
- Low-confidence parses (below threshold but above floor) MUST retain the
  original `body` in the Parquet row. No exceptions.
- The miner exposes: `template_count`, `merges_total`, `confidence_p50`,
  `confidence_p01`, `body_retention_ratio`, `parse_failures_total`. These
  are not optional telemetry.

### 3.2 No unbounded cardinality in `params`
Drain assumes parameters are short, variable bits. Reality: a `params`
slot may capture an entire stack trace, request body, or base64 blob.
Unbounded values destroy Parquet's dictionary encoding and bloat files.

**Rules:**
- Per-parameter byte limit (default 256). Overflow spills to a side
  `body` column with the full original line.
- Per-service alert when overflow rate > 1% of lines.
- Never raise the limit above 1 KiB without an RFC.

### 3.3 Bit-identical body reconstruction
Operators will ask "show me what was actually logged." If we render from
`template + params` and drop whitespace, quoting, or separators, we have
lied to the user.

**Rules:**
- The miner must capture inter-token whitespace and separators, or flag
  the line as "lossy reconstruction" and retain the body.
- Reconstruction is covered by property tests: for every mined line in
  the corpus, render-from-template must equal the original line byte for
  byte, or the line must be flagged lossy.

### 3.4 WAL-before-ack
Ingester acknowledges an OTLP batch only after it has been durably
written to the WAL. "Durably" means fsync or equivalent. No in-memory-only
acks, ever.

**Rules:**
- Batched fsync with an explicit latency/durability knob. Default is
  batch up to 100 ms *or* until WAL segment fills, whichever first.
- Crash recovery test is part of CI. An ingester killed mid-batch must
  recover without losing acknowledged data.
- If you add replication later, it is in addition to the WAL, not
  instead of it.

### 3.5 Parquet schema changes require a migration plan
The on-disk format is the contract with the user's existing data. A
breaking schema change means either rewriting their history or breaking
their queries.

**Rules:**
- All schema changes go through the schema RFC process
  (`docs/rfcs/README.md`).
- Every field is either `OPTIONAL` (preferred) or carries an explicit
  migration for historical files.
- Readers MUST handle absent columns (old files) and unknown columns
  (future files) without error.

### 3.6 Object storage is the source of truth
Local disk is cache and WAL. Parquet on S3 is the truth. Never design a
feature that requires local disk to be durable beyond the WAL horizon.

### 3.7 Multi-tenancy is not bolted on
Every code path that touches data takes a tenant ID. Every Parquet file
is partitioned by tenant. Every template tree is scoped per tenant. There
is no "we'll add tenancy in v2" — that PR is larger than v1.

---

## 4. The hazards (read `docs/hazards.md` too)

Before any change to the hot path, re-read `docs/hazards.md`. The short
list:

1. **Template miner correctness** — strict thresholds, confidence scoring,
   body retention on low confidence, audit events on merges.
2. **Parameter cardinality blowup** — length limits, overflow to body
   column, per-service alerting.
3. **WAL durability vs. latency** — batched fsync with explicit knob,
   crash-recovery test in CI.
4. **Small file problem** — background compaction, target row-group size
   128 MB–1 GB, target file size 256 MB–2 GB.
5. **Template schema evolution across deploys** — template versioning,
   explicit alias mechanism, drift detection as a first-class query.
6. **Query DSL vs. DataFusion SQL surface** — the logs DSL is a
   separately-specified layer; do not leak DataFusion specifics through
   to users.
7. **Bit-identical body reconstruction** — property tests on the corpus.

If a PR touches one of these areas, the PR description must explicitly
address how the change preserves the invariant. Reviewers: check for this
and block the PR if it is missing.

---

## 5. Development workflow

### 5.1 RFCs over improvisation
Any change that touches an architectural pillar (§2), an invariant (§3),
or a hazard (§4) requires an RFC before implementation. RFCs live in
`docs/rfcs/`. Process is in `docs/rfcs/README.md`.

Small changes (bug fixes, dependency bumps, internal refactors) do not
need RFCs. If you are unsure, assume RFC.

### 5.2 Phased execution
Multi-file changes are broken into phases of ≤ 5 files each, each phase
verified before the next begins. This is for humans and for Claude equally
— context decay and review fatigue are the same problem.

### 5.3 Plan then build
When the user says "plan this" or "think about this first," produce only
the plan. Write no code until the user says go. When a plan already
exists, execute it — do not improvise. If the plan is wrong, flag it and
wait.

### 5.4 Follow references, not descriptions
When a contributor points at existing code as a reference ("do it like
X"), match the reference's patterns exactly. The working code is a better
spec than any English description of it.

### 5.5 One-word mode
"Yes", "do it", "go", "ship it" are execution triggers. Do not re-summarise
the plan. The context is loaded; the word is just the go signal.

### 5.6 Verification process
The path from invariant or hazard to passing test is described in
`docs/verification.md`. Acceptance criteria live in RFC §5;
`docs/rfcs/README.md` defines the maturity stages an RFC moves
through. The shortest version of the rule: *if a criterion cannot be
turned into a test, the RFC has a gap.*

---

## 6. Code quality

### 6.1 Rust conventions
- Rust **stable**, pinned in `rust-toolchain.toml`. MSRV documented.
- `#![deny(unsafe_code)]` at every crate root unless an RFC justifies
  otherwise (e.g. `ourios-parquet` may need unsafe for zero-copy).
- `clippy::pedantic` warnings are errors in CI. Allow-lists go in the
  crate, not the CI config, and each `allow` takes a `//` comment
  explaining why.
- `rustfmt` on commit. No exceptions. A rustfmt-noise PR is cheaper than
  a style debate.

### 6.2 Testing discipline
- **Unit tests** next to the code, mandatory for anything non-trivial.
- **Property tests** (`proptest`) for anything with an invariant: the
  template miner, the Parquet writer, the query planner. Reconstruction
  is always a property test.
- **Corpus tests** for the template miner. We maintain an anonymised log
  corpus (see `testdata/`) and measure template count, merge rate, and
  reconstruction accuracy on every change to the miner.
- **Crash recovery test** for the WAL. SIGKILL the ingester mid-batch and
  verify no acknowledged data is lost.
- **Benchmarks** (`criterion`) for the hot path: OTLP → WAL, WAL →
  Parquet, Parquet → query result. Regressions block merges.

### 6.3 Observability of ourselves
Ourios is a telemetry backend. It must be excellently observable itself.
Every subsystem exposes Prometheus metrics; every hot path emits
structured logs (via Ourios, when we bootstrap); every RPC is traced.
"We'll add metrics later" is a PR rejection.

### 6.4 Senior-dev review bar
Write code that three experienced Rust engineers would all write the same
way. No robotic comment blocks, no over-narrated logic, no speculative
abstractions. If a fix feels hacky, escalate to a clean solution before
merging.

### 6.5 Comments discipline
Default to no comments. Comment only when the *why* is non-obvious — a
hidden invariant, a specific bug being worked around, a counter-intuitive
performance choice. Do not restate what the code does. Do not reference
the current PR number or author. If the comment would rot within three
months, do not write it.

### 6.6 Forced verification before "done"
A task is not done until you have run, locally:
- `cargo fmt --all --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-features`
- Any benchmarks touching changed hot paths
- `mdbook build` for any change under `docs/` or `book.toml`, and
  visually verify any new SVG, Mermaid, or LaTeX actually renders

"The bytes hit disk" ≠ "the code compiles" ≠ "the code is correct."
State which checks you ran. If a check is not set up yet, say so
explicitly instead of claiming green.

### 6.7 Documentation site
Docs are an mdBook (`book.toml` at repo root, `docs/` is the source
tree). The first-class artefacts are the architecture set
(`docs/architecture/`), RFCs (`docs/rfcs/`), hazards
(`docs/hazards.md`), the benchmarks / thesis-gates doc
(`docs/benchmarks.md`), the glossary (`docs/glossary.md`), and
lectures (`docs/talks/`).

GitHub Pages deployment is **deferred** until the first shipping
milestone (the `ourios-wal` crash-recovery test landing). The
workflow at `.github/workflows/pages.yml` is intentionally
`workflow_dispatch`-only until that gate clears. When Pages is
enabled, the public URL is `https://jensholdgaard.github.io/ourios/`.

Diagram conventions (Mermaid for RFCs, Excalidraw-style SVG for
lectures) are written in `docs/rfcs/README.md`. That file is the
source of truth — do not duplicate it here, do not improvise around
it.

---

## 7. Repository layout (target)

```
ourios/
├── book.toml                 # mdBook config; docs/ is the source tree
├── crates/
│   ├── ourios-core/          # shared types, tenant, IDs, errors
│   ├── ourios-miner/         # Drain-derived template miner
│   ├── ourios-wal/           # write-ahead log
│   ├── ourios-parquet/       # Parquet writer + schema
│   ├── ourios-ingester/      # OTLP receiver + ingest pipeline
│   ├── ourios-querier/       # DataFusion frontend + logs DSL
│   ├── ourios-server/        # binary, both roles
│   └── ourios-bench/         # criterion benchmarks
├── deploy/
│   └── helm/                 # Helm chart
├── docs/
│   ├── SUMMARY.md            # mdBook navigation
│   ├── introduction.md       # mdBook landing page
│   ├── architecture/
│   ├── rfcs/                 # design RFCs; see rfcs/README.md
│   ├── talks/                # lecture-length explanations
│   │   └── img/              # SVG figures (hand-authored / Excalidraw)
│   ├── benchmarks.md         # thesis-gate measurements
│   ├── hazards.md
│   └── glossary.md
├── testdata/
│   └── corpus/               # anonymised log corpora
└── .claude/
    └── skills/               # project-specific Claude Code skills
```

Do not create crates outside this layout without an RFC. A new crate is an
architectural commitment.

---

## 8. Context management (agent-specific)

### 8.1 Re-read before editing
After 10+ messages in a session, re-read any file before modifying it.
Auto-compaction silently rewrites your memory of file contents; editing
against stale context produces broken patches.

### 8.2 Parallel sub-agents for wide searches
For any task that needs to touch or understand > 5 independent files,
spawn parallel sub-agents rather than sequentially processing the files.
Each agent gets its own context budget. Use `Explore` for read-only
research, a fresh agent in a worktree for isolated write tasks.

### 8.3 Use the file system as memory
Do not hold intermediate results in the conversation. Write them to
`scratch/` (gitignored) and grep them back. Benchmark output, query
plans, Parquet footers — save them as files and process with `jq`,
`grep`, `awk`. This is cheaper and more reproducible than chat context.

### 8.4 No semantic search
You have `grep`, not an AST. When renaming or changing any
function/type/trait, search separately for: direct references, trait
impls, string literals, re-exports, macro expansions, and tests. Assume
the first grep missed something.

### 8.5 Cache discipline
The system prompt, tools, and this file are cached as a prefix.
- Do not suggest mid-session model switches.
- Do not edit this file in the same session where its rules will be
  applied downstream — that invalidates the prefix for the rest of the
  session.
- For long sessions, compact into `context-log.md` and fork rather than
  grow.

---

## 9. Collaboration with non-Claude contributors

Ourios is public OSS. Claude-authored code goes through the same review,
CI, and RFC process as any human contribution. Do not self-merge. Do not
label a PR as ready for review until CI is green and the PR description
addresses any invariant listed in §3 or hazard in §4 that the change
touches.

Attribution: Claude-generated commits use a `Co-Authored-By:` trailer.
RFCs authored in collaboration with Claude list the human driver as the
author and Claude as "drafting assistance" in the RFC header.

---

## 10. When in doubt

1. Read `docs/hazards.md`.
2. Read the most recent accepted RFCs in `docs/rfcs/`.
3. If the change is still not obviously aligned, open an RFC before
   writing code.

Prefer asking one clarifying question to writing five hundred lines of
the wrong thing.

---

*Last updated: 2026-04-26. Original draft 2026-04-23; this revision aligns
§7 with the present-day repo and adds §6.7 (Documentation site) and an
mdBook entry to §6.6, codifying decisions made in commits
1c806f5..84eed86. This document is load-bearing; further changes require
a `meta:` RFC and majority maintainer approval.*
