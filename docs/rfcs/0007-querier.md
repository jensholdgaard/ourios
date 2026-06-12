---
rfc: 0007
title: Querier — DataFusion execution frontend for the logs DSL
status: validated
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-06-01
supersedes: —
superseded-by: —
---

# RFC 0007 — Querier: DataFusion execution frontend for the logs DSL

> **Status note.** **`validated`** (2026-06-12, per the maintainer's
> authorization of the same date). The `docs/verification.md` §3
> ladder requires for `validated` that *"every thesis-gate in
> `benchmarks.md` §7 that the RFC's pillars touch passes on
> representative corpora."* This RFC's pillar is the query engine
> (pillar #3 — DataFusion); the gates it touches are **B1 and B2**,
> and both now pass **authoritatively** on the §1 hardware baseline
> (`baseline-8vcpu-32gib`), measured over ~1 GB+ corpora **including
> a second corpus family** — LogHub HDFS_v1, 11.2 M rows
> (`docs/benchmarks.md` §9.4): B1 at **34.2× / 25.4×** against the
> ≥ 10× gate with exact row-count agreement vs the reference
> pipeline; B2's windowed template-exact scan **flat** at 1 row
> group / 4.2–5.9 ms from 735 k to 11.2 M rows while the full-span
> variant grows with the corpus. **A1's authoritative FAIL does not
> gate this RFC**: the ladder scopes validation to the gates the
> RFC's pillars touch, and A1 belongs to the template-mining /
> compression pillar (measured under RFC 0006), where its
> escalation is handled. The prior validated-pending checklist
> resolves as: (1) authoritative `baseline-8vcpu-32gib` rerun —
> **✓ done** (§9.4); (2) denser error band — **still open**, a
> non-blocking quality improvement (the §9.4 B1 bands remain
> 11 / 28 rows); (3) second corpus family for B2 — **✓ done**
> (HDFS_v1 via the query-bench arm). Earlier history: the §5
> acceptance criteria RFC0007.1–.5 went `green` via
> `crates/ourios-querier/tests/{execution,boundary,forward_compat}.rs`
> and the `crates/ourios-querier/src/lib.rs` no-leakage unit test
> (`tests/acceptance.rs` is a pointer to them); the first indicative
> `ci-runner` B1/B2 readings are §9.3. `accepted` follows on
> maintainer sign-off per the `docs/rfcs/README.md` ladder.

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
(RFC 0005 §3.9 reader contract) — already exists, so the querier can be built
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
§3.3 query-consumer-absence rule fixes the Phase 3 B1/B2 pushdown
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
  future RFC 0005 §3.6 storage amendment (§8).
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
  - **Then** the pruned fraction `row_groups_pruned /
    (row_groups_scanned + row_groups_pruned)` (both `QueryResult`
    stats fields) exceeds a floor (e.g. ≥ 80% on the bench corpus)
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
  - **Then** results honour RFC 0005 §3.9 reader-contract defaults without
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

## 7. Alternatives considered

- **Expose DataFusion SQL directly (no logs DSL).** Cheapest to
  build — register the tables, hand users SQL. Rejected: it
  violates hazard §4.6 (no DataFusion/SQL leakage), couples the
  user-facing query contract to an implementation dependency, and
  forfeits the logs-shaped ergonomics RFC 0002 exists to provide.
- **Write a bespoke vectorised execution engine.** Maximum control
  over pushdown. Rejected: it contradicts pillar #3 (`CLAUDE.md`
  §2 — "we do not write a vectorised execution engine") and the
  "off-the-shelf parts plus thin glue" thesis (§1). DataFusion
  already does row-group skipping from Parquet stats.
- **Lucene/Tantivy-style inverted index alongside Parquet.** A
  second index structure for term lookups. Rejected for v1: the
  thesis is that template structure + Parquet statistics *collapse*
  the inverted index into the columnar store (§1) — adding a
  separate index pre-judges that the collapse fails, which is what
  B1/B2 are meant to test. Revisit only if B1/B2 fail.
- **Defer the crate until RFC 0002's DSL branch is decided.**
  Rejected: the execution layer (lowering target, pushdown,
  B1/B2 measurement) is branch-independent (RFC 0002 §5.5), and
  B1/B2 are the project's largest unmeasured risk — building the
  branch-independent half now buys the thesis signal soonest. The
  parser integration landed once RFC 0002 §3 resolved (Branch B); see §8.

## 8. Open questions

- [x] RFC 0002 §3 resolved (Branch B, #143) and the parser integration
      landed (#145–#154; RFC 0002 is `green`). The execution layer here was
      branch-independent throughout, as planned.
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

## 9. References

- `CLAUDE.md` §1 (thesis), §2 pillar #3 (DataFusion), §4.6 (DSL/no
  leakage hazard), §3.5 (schema evolution), §3.7 (multi-tenancy),
  §7 (new crate).
- RFC 0002 — query DSL (the syntax this executes; §5.5 plan target).
- RFC 0005 — Parquet storage (the reader contract this queries).
- RFC 0006 — bench harness (defers B1/B2 here; the corpus series).
- `docs/benchmarks.md` B1/B2 (the thesis-gate definitions).
- `docs/roadmap.md` Phase 3.
