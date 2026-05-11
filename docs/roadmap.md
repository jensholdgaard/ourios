# Roadmap to MVP

> Living document. Refreshed at phase boundaries (§4) and whenever
> a merged PR materially changes the *current state* in §3.
> Last updated: **2026-05-11** (after PR #15: `sim_seq` +
> `confidence_ratio` landed).

This document answers two questions in one place: *what does
"MVP" mean for Ourios*, and *how far are we from it*. The
artifact is parallel to [`hazards.md`](./hazards.md) and
[`benchmarks.md`](./benchmarks.md): hazards say what we mustn't
break, benchmarks say what success looks like, and this file
says how we get from here to there.

---

## 1. What "MVP" means here

**MVP for Ourios is thesis-proving, not production-ready.**

The thesis (`CLAUDE.md` §2) claims that Parquet + Drain-derived
template mining + DataFusion collapses the inverted index, the
compression layer, the storage tier, and the query engine into
one stack of off-the-shelf parts plus thin glue. That claim is
falsifiable. The MVP is the smallest stack that lets us run the
**thesis-gate benchmarks** in [`benchmarks.md`](./benchmarks.md)
on a real corpus and either confirm the claim or kill it.

Production-shape concerns — gRPC OTLP receiver, WAL durability,
snapshot mechanism, Helm chart, the full §6.8 telemetry surface,
the RFC 0002 query DSL — are deliberately **out of MVP scope**
(§5). Each is a real shipping concern, but none of them changes
the answer to "does the thesis hold." We defer to keep the
critical path as short and honest as possible.

---

## 2. The MVP gate: thesis benchmarks

Five `[THESIS]`-marked goals in
[`benchmarks.md`](./benchmarks.md) define MVP-done. Hitting all
five on a representative corpus means the thesis holds; missing
any of them means a pillar (`CLAUDE.md` §2) is wrong and a PR
won't fix it — an RFC will.

| Gate | What it measures | Why it matters |
|---|---|---|
| **A1** | End-to-end compression ratio vs. zstd-alone over flat text | Pillar 1 (Parquet) + Pillar 2 (template mining) stack, or they don't |
| **B1** | Predicate-pushdown query latency on time/template/tenant filters | Pillar 1 (footer reads + min/max stats skip row groups) actually skips |
| **B2** | Template-exact query latency (`where template_id = X`) | Pillar 2's `template_id` column is a usable index, not a curiosity |
| **C1** | Bit-identical reconstruction rate over the corpus | The hardest invariant (`CLAUDE.md` §3.3) holds in practice, not just in unit tests |
| **C2** | Template-count convergence (Drain finds a small, stable number of templates) | Pillar 2 (template mining) extracts the structure we believed was there |

`A2`, `B3`, `C3`, `C4`, `D*`, `E*` in `benchmarks.md` are
relevant but not MVP-blocking — they're tuning goals, honesty
goals, or post-MVP shipping concerns.

---

## 3. Current state (as of 2026-05-11)

**§5 scenarios green: 6 / 29.** RFC 0001 status: `red`.

What the code does today:

- **`ourios-core`** — `MinerConfig` (defaults + validation, `[§3.1]`/`[§3.2]`-anchored), `TenantId` (newtype, `[§3.7]`-anchored).
- **`ourios-miner`** —
  - `tokenize` (Unicode-whitespace splitting, separators array
    captured but not yet flowing through the pipeline).
  - `mask` with `MaskTag` (UUID/IPv4/NUM rules; HEX/TS/PATH/STR/OVERFLOW are reserved enum variants, no emitter yet).
  - `sim_seq` + `confidence_ratio` + `Token` enum (RFC §3.2 / §6.3 math primitives, no caller yet).
  - `MinerCluster` with `TenantState` (cluster-wide
    `template_id` allocator, exact-match `HashMap` per-tenant
    template store — placeholder for the real Drain tree).
- **No other crates yet.** `ourios-wal`, `ourios-parquet`,
  `ourios-ingester`, `ourios-querier`, `ourios-server`,
  `ourios-bench` are listed in the workspace `Cargo.toml` as
  comments and don't exist on disk.

What's specifically missing for the thesis gates:

| Gate | Blocker(s) |
|---|---|
| **A1** | Records-to-Parquet writer, mined records flowing through a real pipeline (corpus → Parquet) |
| **B1** | DataFusion frontend, Parquet reader, predicate pushdown wiring |
| **B2** | Same as B1 plus `template_id` as a queryable column |
| **C1** | Separators preservation through ingest, `reconstruct()`, `lossy_flag` semantics, body retention |
| **C2** | Drain tree + `descend` + `widen` + best-candidate selection (replacing the exact-match `HashMap` placeholder) |

For `cargo test --all-features`'s outer-loop view: 30 passed /
23 ignored. The 23 ignored stubs map to RFC 0001 §5 scenarios
that the missing pieces above would unblock.

---

## 4. Path to MVP — three phases

Phase scope only; per-PR breakdown lives in the planning that
opens each phase, not in this doc, so the file stays stable as
mid-stream design decisions land.

### Phase 1 — Finish the miner

**Goal:** the miner mines, audits, retains bodies, reconstructs.
By the end of this phase the miner self-contained covers RFC
0001 §6.2 / §6.3 / §6.4 / §6.5 / §6.6 end-to-end and most §5
scenarios are green.

**Capabilities to land:**

- Drain tree (root → length-N nodes → prefix nodes → leaves)
  with `descend`.
- Best-candidate selection in `MinerCluster::ingest` via
  `sim_seq` (replaces the exact-match `HashMap` placeholder).
- `widen` step + `template_widened` audit emission +
  type-expansion + `template_type_expanded` audit + degenerate-
  template guard.
- Three-zone confidence branching (clean / lossy / parse-failure)
  + body retention in the lossy zone.
- Separators preservation through the ingest pipeline +
  `reconstruct()` + `lossy_flag` semantics per §6.6.
- Per-parameter byte-limit check + `OVERFLOW` marker + forced
  body retention.

**Unblocks:** thesis gates **C1** (reconstruction) and **C2**
(template-count convergence). RFC 0001 §5 scenarios H1.\*,
H2.\*, H5.\*, H7.\*, §3.3.1, RFC0001.\* should mostly flip in
this phase.

### Phase 2 — Records to Parquet

**Goal:** mined records become Parquet files. By the end of this
phase a corpus run produces on-disk Parquet that any
DataFusion-aware reader can open.

**Capabilities to land:**

- New crate `ourios-parquet`.
- Record schema matching RFC 0001 §6.1 (`tenant_id`,
  `template_id`, `template_version`, `params`, `separators`,
  `body?`, `confidence`, `lossy_flag`).
- Writer: record batch → Parquet file (with row-group sizing
  from `hazards.md` H4 — target 128 MB–1 GB row groups).
- Reader: Parquet file → record batch (for verification + the
  Phase 3 DataFusion path).
- Audit-event Parquet stream (the contract called out in RFC
  0001 §9 *"Cross-RFC contracts pending"*).

**Unblocks:** thesis gate **A1** (compression ratio). The Parquet
column codec earns its share of the 50–200× headline only once
records actually land on disk in this format.

**Out of MVP scope, parked here:** background compaction
(small-file problem, `hazards.md` H4) — corpus runs are bounded,
a single Parquet file per phase is acceptable; production
compaction is a post-MVP PR.

### Phase 3 — DataFusion + bench

**Goal:** the thesis-gate benchmarks run.

**Capabilities to land:**

- New crate `ourios-querier` — register the Phase 2 Parquet
  files with DataFusion and accept raw SQL. **No DSL** — RFC
  0002's surface is a post-MVP concern; the bench can use SQL
  directly.
- New crate `ourios-bench` — corpus runner that ingests the
  corpus, writes Parquet, runs the A1/B1/B2/C1/C2 measurements,
  and reports numbers that go into `benchmarks.md` §9 (Status).
- `testdata/corpus/` — anonymised real-log corpus committed to
  the repo (or a download script if size demands).

**Unblocks:** thesis gates **B1** (predicate-pushdown latency)
and **B2** (template-exact latency). At the end of this phase,
`benchmarks.md` §7 (the thesis-gate summary) has measured
numbers for every `[THESIS]` row, and either the thesis holds
or it doesn't.

---

## 5. Deliberately out of MVP

Each item is a real production concern. The reason it's deferred
is *"answering 'does the thesis hold?' doesn't require it,"* not
*"we don't think it matters."*

| Capability | Why deferred for MVP | When it lands |
|---|---|---|
| **Write-ahead log** (`ourios-wal`) | Corpus replay is bounded and reproducible; durability is irrelevant for thesis-proving | First post-MVP shipping PR series — required before any non-corpus traffic |
| **OTLP receiver** (gRPC + HTTP) | Bench input is the corpus, not gRPC | Same — paired with WAL since both gate ingest |
| **Snapshot mechanism** (RFC 0001 §6.9) | Corpus runs from cold start; replay budget moot | After WAL — snapshots are an optimisation on top of WAL replay |
| **Full §6.8 telemetry surface** | One or two metrics suffice for the bench; the §3.1.2 mandatory set is a production observability concern | After Phase 1 finishes — the metrics depend on the miner's hot path being final |
| **Query DSL** (RFC 0002) | Raw SQL through DataFusion serves the bench; DSL is operator UX | Post-MVP — RFC 0002 already drafted but not specified |
| **Multi-tenancy at runtime** (rate limits, eviction, lifecycle) | Bench uses one tenant; the type is in place but no orchestration around it | Post-MVP, tied to operator-console RFC (see RFC 0001 §9 *"Multi-tenancy and operational lifecycle"*) |
| **`ourios-server` binary + Helm chart** | Bench is a binary in `ourios-bench`; full deployment shape is shipping concern | Post-MVP, sequencing TBD |

---

## 6. Update cadence

This file refreshes:

- After every merged PR that materially changes §3 (current
  state) — the merging PR's author (or their drafting
  assistant) updates the table and the §5 scenario count.
- At phase boundaries (§4) — when Phase 1 finishes, §3's
  current state and §4's "blockers" tables are reconciled, and
  the next-phase opening planning PR is summarised here.
- When a thesis-gate result lands in `benchmarks.md` §9 — this
  doc gets a one-line note in §3 acknowledging the result.

The doc is intentionally *not* refreshed on every spec edit —
RFC patches and `hazards.md` edits don't change the road map
unless they change what MVP requires. If you find yourself
updating §3 every PR, the doc has become an activity log; the
fix is to be more selective, not to stop updating.
