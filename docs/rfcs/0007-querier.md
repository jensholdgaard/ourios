---
rfc: 0007
title: Querier — DataFusion execution frontend for the logs DSL
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-06-01
supersedes: —
superseded-by: —
---

# RFC 0007 — Querier: DataFusion execution frontend for the logs DSL

> **Status note.** This is a first draft. It pins the *execution*
> layer — how a compiled query runs against the RFC 0005 Parquet
> contract with predicate pushdown, and how the B1/B2 thesis gates
> are measured — which is independent of RFC 0002's unresolved
> Branch A/B *syntax* decision (both branches compile to the same
> DataFusion `LogicalPlan` target, RFC 0002 §5.5). It deliberately
> does **not** re-decide the DSL surface. Cannot flip past
> `specified` until RFC 0002 §3 lands and the §5 acceptance
> scenarios below have executable tests.

## 1. Summary

Introduces the `ourios-querier` crate (pillar #3 — DataFusion as
the query engine). It takes a parsed logs-DSL query (RFC 0002),
lowers it to a DataFusion `LogicalPlan` over the RFC 0005 Parquet
data + audit files on object storage, executes it with **aggressive
predicate pushdown** (row-group skipping via min/max statistics,
bloom filters, page indexes), and returns results without ever
exposing DataFusion or SQL to the caller (hazard `CLAUDE.md` §4.6).
It is the home of the **B1** (predicate-pushdown) and **B2**
(template-exact query latency) thesis gates that RFC 0006 §1
deferred. The crate is the read path; it depends on neither the WAL
(RFC 0008) nor the receiver (RFC 0003) — it reads what the writer
already produced.

## 2. Motivation

### 2.1 The thesis's load-bearing half is unmeasured

`CLAUDE.md` §1 stakes Ourios on collapsing the inverted index, the
compression layer, the storage tier, and the query engine into
"one stack of off-the-shelf parts plus thin glue." The compression
and storage claims (A1/C1/C2) are now measured against a real OTLP
corpus (RFC 0006, the `corpus/otel-demo-v*` series). The *query*
claim — that template structure + Parquet statistics let us answer
queries by **skipping** data rather than scanning it — has no code
and no measurement. B1/B2 are blank. Until they aren't, "viable log
backend" is unproven on its central premise.

### 2.2 Why at this layer, and why now

`docs/roadmap.md` Phase 3 names `ourios-querier` alongside
`ourios-bench` (shipped). RFC 0006 §1 explicitly routes B1/B2 here.
The dependency it needs — `ourios-parquet`'s reader contract
(RFC 0005 §3.0/§3.7) — already exists, so the querier can be built
and benchmarked in parallel with the WAL/receiver ingest path. It
is the highest-information work available: it converts the project's
biggest open question into a measurement.

### 2.3 Why an RFC and not just a crate

A new crate is an architectural commitment (`CLAUDE.md` §7), it
realises pillar #3 (§5.1), and it owns hazard §4.6 (no DataFusion
leakage to users). The DSL→plan→execution boundary and the B1/B2
acceptance criteria need pinning before code so the bench gates are
testable contracts rather than retrofitted numbers.

## 3. Background — what the querier is and is not

### 3.1 Is
A library crate exposing a `Querier` that accepts an RFC 0002 query
AST, compiles it to a DataFusion `LogicalPlan`, registers the
RFC 0005 Parquet files as a partitioned `ListingTable` (or a custom
`TableProvider` when partition pruning needs it), executes via
DataFusion's physical planner, and streams typed result rows back.

### 3.2 Is not
- Not the DSL parser/surface — that is RFC 0002. The querier
  consumes the AST RFC 0002 produces.
- Not a SQL endpoint. DataFusion's SQL frontend, `LogicalPlan`
  types, and `arrow`/`datafusion` errors never cross the public
  API (hazard §4.6). The public surface speaks logs-DSL and Ourios
  result/error types.
- Not the storage format. It reads the RFC 0005 contract; it does
  not define it.

## 4. Proposed design

### 4.1 Crate shape
`crates/ourios-querier/`, `#![deny(unsafe_code)]`, workspace lints.
Public surface (sketch — names provisional):

```rust
pub struct Querier { /* object-store handle, session ctx, config */ }
pub struct QueryRequest { tenant: TenantId, query: ParsedQuery, /* time bounds, limit */ }
pub struct QueryResult { /* typed rows + stats: rows, row_groups_scanned, row_groups_pruned, bytes_read */ }
pub enum QueryError { /* no datafusion/arrow types leaked */ }
impl Querier {
    pub async fn run(&self, req: QueryRequest) -> Result<QueryResult, QueryError>;
}
```

### 4.2 DSL → LogicalPlan lowering
RFC 0002 §5.5 fixes the compilation target as a DataFusion
`LogicalPlan` for both syntax branches. The querier owns that
lowering: predicates → `Expr` filters; template references →
`template_id` equality/`IN`; time bounds + tenant → partition-key
filters (Hive partitioning per RFC 0005). The lowering is the only
place DataFusion types appear; they are an implementation detail
behind `run`.

### 4.3 Predicate pushdown (the thesis mechanism)
Pushdown is scoped to exactly the columns RFC 0005 indexes. Its
§3.4 query-consumer-absence rule fixes the Phase 3 B1/B2 pushdown
keys as `template_id`, `tenant_id`, and `time_unix_nano`, and
§3.6 deliberately gives `params` list values **no** page index and
**no** bloom filter (per-row entropy too high). The querier
therefore relies on:

- **Partition pruning**: `tenant_id` and time partition keys filter
  whole directories before any file is opened.
- **Row-group skipping**: min/max statistics on `template_id`,
  `time_unix_nano`, and severity let DataFusion drop row groups
  whose stats can't satisfy the predicate.
- **Bloom filter / page index** on `template_id` (RFC 0005 §3.6
  writer policy) for high-selectivity template-exact equality (B2).
- **Param predicates are *not* row-group-prunable** under the
  current RFC 0005 format — they apply as post-scan DataFusion
  filters over the rows the above pruning leaves. They benefit
  from template/time pruning narrowing the scan, but a param value
  alone skips no row groups; param-level pruning would need a
  future RFC 0005 §3.6 storage amendment (§7).
- The querier configures the DataFusion session so the above are
  enabled, and surfaces `row_groups_pruned` / `bytes_read` in
  `QueryResult` stats so B1 can assert pruning actually happened.

### 4.4 No-leakage boundary (hazard §4.6)
A boundary test asserts the public API's types are Ourios-owned:
no `datafusion::*` / `arrow::*` / SQL strings in signatures or
error `Display`. DataFusion is a `pub(crate)` dependency.

## 5. Acceptance criteria

> `Given/When/Then`, ids greppable from tests. These realise the
> RFC 0006 B1/B2 gates as querier-level contracts.

- **RFC0007.1 — B1 predicate pushdown prunes row groups `[thesis]`**
  - **Given** a corpus partitioned across many row groups where a
    target `template_id` lives in a known minority of them
  - **When** a template-exact query runs
  - **Then** `QueryResult.row_groups_pruned / total` exceeds a
    floor (e.g. ≥ 80% on the bench corpus)
  - **And** `bytes_read` is sub-linear in corpus size for fixed
    result size.

- **RFC0007.2 — B2 template-exact latency scales with result, not corpus `[thesis]`**
  - **Given** the same query against corpora of increasing size
    with the result-set size held ~constant
  - **When** each is executed
  - **Then** median latency is bounded by result size, not corpus
    size (the inverted-index-collapse claim, `docs/benchmarks.md`
    B2) — measured by `criterion` across the `corpus/otel-demo-v*`
    series.

- **RFC0007.3 — no DataFusion/SQL leakage `[§4.6]`**
  - **Given** the public API
  - **When** a query errors or returns
  - **Then** no `datafusion`/`arrow`/SQL type appears in any public
    signature or error message (compile-/string-level boundary test).

- **RFC0007.4 — forward-compatible reads `[§3.5]`**
  - **Given** Parquet files with unknown columns (future schema) or
    missing optional columns (old schema)
  - **When** queried
  - **Then** results honour RFC 0005 §3.7 reader defaults without
    error.

- **RFC0007.5 — tenant isolation `[§3.7]`**
  - **Given** multi-tenant data
  - **When** a query for tenant T runs
  - **Then** no row from another tenant can appear, enforced at the
    partition-prune layer (a query without a tenant is a usage
    error, not a cross-tenant scan).

## 6. Testing strategy

Mapped to `CLAUDE.md` §6.2:
- **Unit** — DSL→`LogicalPlan` lowering (RFC0007.1/.4/.5 plan shape),
  colocated.
- **Boundary test** — RFC0007.3 no-leakage (trybuild/string assertion).
- **Integration** — run queries over fixture Parquet from the
  `ourios-parquet` writer; assert `row_groups_pruned`/`bytes_read`
  (RFC0007.1) and tenant isolation (RFC0007.5).
- **Bench (`criterion`)** — RFC0007.2 latency-vs-corpus-size across
  `corpus/otel-demo-v*`; wired into `ourios-bench` as the B1/B2
  gates, closing the RFC 0006 §1 deferral.
- **Property (`proptest`)** — lowering total over the RFC 0002 AST
  (no panic; tenant + time bounds always present in the plan).

## 7. Open questions

- [ ] Blocked on RFC 0002 §3 (Branch A vs B) before the parser
      integration is final — but the execution layer here is
      branch-independent.
- [ ] `ListingTable` vs a custom `TableProvider` — does partition
      pruning over object storage need the custom provider, or does
      the listing table's pruning suffice?
- [ ] Param-predicate pushdown is **out of scope** under the
      current format (RFC 0005 §3.6 gives `params` no index/bloom).
      If param predicates ever need row-group pruning, that's a
      **future RFC 0005 §3.6 storage-format amendment** (add index/
      bloom to selected param columns — selectivity vs file-size
      cost), not a querier-side policy decision.
- [ ] Streaming vs materialised results in `QueryResult` (large
      result sets); pagination surface.
- [ ] Object-store caching / footer-cache policy for repeated
      queries — affects B2 measurement methodology.
- [ ] Async runtime + concurrency model for the querier role of the
      server binary (RFC 0003 sibling).

## 8. References

- `CLAUDE.md` §1 (thesis), §2 pillar #3 (DataFusion), §4.6 (DSL/no
  leakage hazard), §3.5 (schema evolution), §3.7 (multi-tenancy),
  §7 (new crate).
- RFC 0002 — query DSL (the syntax this executes; §5.5 plan target).
- RFC 0005 — Parquet storage (the reader contract this queries).
- RFC 0006 — bench harness (defers B1/B2 here; the corpus series).
- `docs/benchmarks.md` B1/B2 (the thesis-gate definitions).
- `docs/roadmap.md` Phase 3.
