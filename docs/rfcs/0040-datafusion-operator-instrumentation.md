---
rfc: 0040
title: DataFusion → OTel operator instrumentation — the query span as an operator tree
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-25
supersedes: —
superseded-by: —
---

# RFC 0040 — DataFusion → OTel operator instrumentation

## 1. Summary

The `POST /v1/query` span (RFC 0038) is flat: it times the whole query but shows
nothing of *where* the time went. This RFC deepens it into an operator tree by
emitting one OTel child span per `ExecutionPlan` node, reconstructed post-hoc
from the finished physical plan. DataFusion 54 records genuine wall-clock
`StartTimestamp`/`EndTimestamp` on every `BaselineMetrics`-backed operator, so
the spans carry **real** bounds (not synthetic timings), with `output_rows`,
`elapsed_compute`, `output_bytes`, and pruning counts as attributes. The logic
lives in a new, dependency-light crate (`ourios-df-otel`) whose only deps are
`datafusion` and `opentelemetry` — so it lifts cleanly to a standalone
`datafusion-opentelemetry` for `datafusion-contrib`, the dogfood-then-give-back
path RFC 0038 §7 named. This is a new crate (hence an RFC per `CLAUDE.md` §7) and
extends the traces pillar (§5.1).

## 2. Motivation

Ask "why was this query slow?" and today's trace answers only "it took 340 ms."
Every mature database instrumentation — the postgres client span with
`db.query.text` and per-statement timing is the canonical example — lets an
operator see the work *decomposed*. For a query engine, the natural
decomposition is the physical plan: which operator scanned how many row groups,
where pruning helped, which node dominated the wall clock. Ourios already reads
this per-operator data (`scan_stats`/`fold_metrics`) but only rolls it up into
aggregate `QueryStats` **metrics** — the per-node structure is discarded. This
RFC keeps that structure as **spans**, turning the flat query span into the
operator tree an engine's trace should be.

It is also strategic. RFC 0038 §7 committed to building a reusable
`datafusion-opentelemetry` component for `datafusion-contrib` — "built for
Ourios's own query span first and then extracted upstream." This RFC is that
build. Keeping the crate's dependencies to `datafusion` + `opentelemetry` (no
Ourios types) is what makes the extraction a lift, not a rewrite.

## 3. Proposed design

### 3.1 The timing source (the finding that shapes everything)

DataFusion 54 operators built on `BaselineMetrics` record a real
`StartTimestamp` at stream construction and a real `EndTimestamp` on
drain/`Drop` (`datafusion-physical-plan-54/src/metrics/baseline.rs:75,135,175`).
These surface as `MetricValue::StartTimestamp` / `EndTimestamp` in the node's
`MetricsSet`, and `MetricsSet::aggregate_by_name()` reduces per-partition
instances to **earliest start** / **latest end**
(`metrics/value.rs:915`) — exactly the wall-clock interval a span needs. This is
the crux: because the timestamps are genuine, the operator spans are truthful,
not derived. `ElapsedCompute` (CPU-busy time) becomes an *attribute*, never the
span's timeline.

Two residual constraints, both benign for Ourios:

1. **Post-hoc.** Metrics populate only after `collect()` returns. Every Ourios
   query path fully buffers via `datafusion::physical_plan::collect`
   (`lib.rs:73`; never `execute_stream`), so the plan is finished and its
   timestamps final when we read them. Spans are therefore built *after* the
   query, with explicit start/end — not opened live.
2. **Opt-in metrics.** `ExecutionPlan::metrics()` returns `None` for operators
   that do not use `BaselineMetrics` (`execution_plan.rs:492`). A node with no
   timestamps is **skipped** (no span), so the tree shows the operators that
   actually carry timing; children of a skipped node re-parent to the nearest
   timed ancestor (or the query span).

### 3.2 The walk — reuse what already exists

`accumulate_scan_stats` (`lib.rs:703`) already recurses the physical plan tree:
for each node it reads `plan.metrics()` and recurses over `plan.children()`. The
span reconstruction is the same walk with a different fold — for each timed
node emit a span instead of (in addition to) accumulating stats. The retained
`plan: Arc<dyn ExecutionPlan>` is available at exactly the sites `scan_stats` is
called today, before the `Arc` drops: `lib.rs:1355` (count scan), `:1422`
(aggregate), `:1492` (row materialize), `drift.rs:191`. The new crate exposes a
single entry point:

```rust
// ourios-df-otel
pub fn record_plan_spans<T: opentelemetry::trace::Tracer>(
    plan: &dyn ExecutionPlan,
    parent: &opentelemetry::Context,
    tracer: &T,
);
```

The tracer is a generic `T: Tracer`, not `&dyn Tracer`: the `Tracer` trait has an
associated `Span` type and is not object-safe as a bare trait object. Callers pass
the global tracer (`opentelemetry::global::tracer("ourios-df-otel")`, a
`BoxedTracer`) or any concrete tracer.

It walks `plan`, and for each node with `StartTimestamp`+`EndTimestamp` builds a
child span (parent = its plan-parent's span, root = `parent`) named by
`ExecutionPlan::name()` (e.g. `DataSourceExec`, `FilterExec`, `AggregateExec` —
low-cardinality, the operator kind).

### 3.3 Span emission — the raw OTel span builder (not `#[instrument]`)

Backdated spans cannot come from `#[tracing::instrument]` (it starts "now"). The
crate uses the OTel SDK span builder directly. `with_start_time` and
`end_with_timestamp` take `std::time::SystemTime`, so the node's
`DateTime<Utc>` timestamps convert via `SystemTime::from`; `end_with_timestamp`
takes `&mut self`:

```rust
let start: SystemTime = node_start.into();     // DateTime<Utc> -> SystemTime
let end:   SystemTime = node_end.into();
let mut span = tracer
    .span_builder(node.name().to_string())
    .with_kind(SpanKind::Internal)
    .with_start_time(start)
    .with_attributes(node_attributes(&metrics))
    .start_with_context(tracer, parent_cx);    // parent_cx = this node's parent span's context
// … recurse into children, passing this span's context as their parent …
span.end_with_timestamp(end);                  // &mut self; real EndTimestamp
```

**Attributes are normative** — the span contract is deterministic (types, units,
and the no-match representation are fixed):

| Attribute | Type / unit | Source (`MetricValue`) |
|---|---|---|
| `…output_rows` | int, rows | `OutputRows` |
| `…elapsed_compute` | int, **nanoseconds** | `ElapsedCompute` (`Time::value()` is ns) |
| `…output_bytes` | int, bytes | `OutputBytes` |
| `…row_groups_pruned` | int, count | scan `PruningMetrics::pruned()` |
| `…row_groups_matched` | int, count | scan `PruningMetrics::matched()` |

Pruning is emitted as the two **counts**, never a ratio — a ratio is undefined
when `matched == 0` (a fully-pruned or non-scanning node); a consumer derives the
ratio if it wants one. An attribute whose metric a node does not report is
**omitted**, not zero-filled, so presence is meaningful. Names follow an existing
OTel convention where one applies, else an `ourios.query.operator.*` /
`datafusion.*` namespace — **the exact names clear the OTel MCP + weaver
registry** (`CLAUDE.md` alignment rule) before landing; §7 tracks it.

### 3.4 Parenting into the query span

The operator spans must nest under `POST /v1/query`. That span is a *tracing*
span (in `ourios-server`); the plan executes in *`ourios-querier`*. The parent
`opentelemetry::Context` is obtained inside the querier via
`tracing::Span::current().context()` (`OpenTelemetrySpanExt`) — the query span is
current throughout `run_query`, including the post-`collect` reconstruction. This
adds `tracing-opentelemetry` + `opentelemetry`(trace) as `ourios-querier`
dependencies (parallel to RFC 0039's promotion, and called out likewise). The
querier then calls `ourios_df_otel::record_plan_spans(&plan, &cx, &tracer)` at
the `scan_stats` sites.

No DataFusion type crosses any Ourios public boundary (H6): `record_plan_spans`
is an internal side-effect on the retained plan; the query *response* and *error*
surfaces are unchanged.

### 3.5 The new crate

`crates/ourios-df-otel/` — deps `datafusion` (the pinned 54) and `opentelemetry`
(trace) **only**, no `ourios-*` deps. `#![deny(unsafe_code)]`. This isolation is
deliberate: it is what lets the crate lift to a standalone
`datafusion-opentelemetry` for `datafusion-contrib` with no un-picking. The
Ourios-specific wiring (getting the parent context, the call sites) stays in
`ourios-querier`; the crate is pure "`ExecutionPlan` tree + parent context →
spans."

### 3.6 Cost discipline (RFC 0038's boundary, honoured)

The reconstruction is **O(plan nodes)** — a handful per query — and runs **once
per query**, after execution. It is not per-record and not per-batch (RFC
0038.2's invariant). It is gated on the query span being **recording and
sampled**: before walking, check
`parent.span().span_context().is_sampled()` (the parent `Context`'s active span's
`SpanContext`), which is `false` both when traces are disabled (no OTel layer →
an invalid, unsampled `SpanContext`) and when the sampler dropped this trace. An
unsampled query skips the walk entirely, so the cost is zero on the sampled-out
path (the default) and bounded-tiny on the sampled path. A `criterion` guard
confirms no query-latency regression on the sampled-out path.

## 4. Alternatives considered

**(b) True live spans by wrapping `ExecutionPlan`/`RecordBatchStream`.** Insert a
wrapping operator via a `PhysicalOptimizerRule`
(`SessionStateBuilder::with_physical_optimizer_rule`) that opens a span in
`execute()` and ends it when the stream drains. This captures true intra-operator
concurrency/overlap that the post-hoc min/max bounds flatten. But it adds a
per-poll wrapper to the hot execution path, complicates the `collect`-based flow,
and re-derives timing DataFusion already records — all for concurrency detail
few will read. Deferred: it is the natural *next* increment of the extractable
crate, not the first cut. Post-hoc (a) already yields real bounds.

**(c) One query span, plan as an attribute/event.** Attach
`displayable(plan).indent()` plus rolled-up metrics as attributes on the existing
query span. Cheapest, and a fine fallback when traces are off — but it is a
string blob, not a navigable operator tree, and defeats the "where did time go"
goal (no per-operator timeline). Rejected as the primary design; the plan-text
*may* still ride the query span as a supplementary attribute (§7).

**Do nothing (flat query span).** The query span still gives end-to-end latency
and the aggregate pruning metrics. But the per-operator structure — already
computed and thrown away — stays invisible, and the `datafusion-contrib`
give-back never happens.

**A module inside `ourios-querier` instead of a crate.** Simpler in the tree, but
couples the logic to Ourios and forfeits the extraction. The whole value is a
`datafusion`+`opentelemetry`-only component; a crate is what encodes that.

**Adopt an existing `datafusion-contrib` OTel crate if one now exists.** None is
referenced in-repo, and RFC 0038 treated this as greenfield — but the ecosystem
moves. §7 makes "check `datafusion-contrib` for a current crate" a gate before
building, to adopt-or-align rather than duplicate.

## 5. Acceptance criteria

> **Scenario RFC0040.1 — a query emits an operator span tree under its query
> span.**
> **Given** traces enabled, the query span sampled, and a logs query that scans
> at least one Parquet file,
> **When** the query executes,
> **Then** at least one child span is emitted whose parent (transitively) is the
> `POST /v1/query` span, one per timed `ExecutionPlan` node, each named by the
> operator kind (`DataSourceExec`, `FilterExec`, …), forming the plan tree.

> **Scenario RFC0040.2 — operator spans carry real wall-clock bounds.**
> **Given** the same,
> **When** the tree is reconstructed,
> **Then** each operator span's start/end equals the node's aggregated
> `StartTimestamp`/`EndTimestamp` (earliest-start / latest-end across
> partitions) — genuine wall-clock, within the parent query span's interval, not
> derived from `ElapsedCompute`.

> **Scenario RFC0040.3 — the metric attributes are present and correct.**
> **Given** an operator reporting `output_rows`, `elapsed_compute`,
> `output_bytes`, and (for the scan) pruning counts,
> **Then** its span carries those as attributes, equal to the values
> `fold_metrics`/`aggregate_by_name` reads for the same node — the span and the
> `QueryStats` metric never disagree about the same operator.

> **Scenario RFC0040.4 — nodes without metrics are skipped, not faked.**
> **Given** an `ExecutionPlan` node whose `metrics()` is `None` (no
> `BaselineMetrics`),
> **Then** no span is emitted for it, and its children re-parent to the nearest
> timed ancestor (or the query span) — the tree never invents a timeline.

> **Scenario RFC0040.5 — O(plan), once per query; never per-record.**
> **Given** a query returning N records,
> **When** it executes,
> **Then** the number of operator spans is bounded by the plan node count and is
> **independent of N** (RFC 0038.2's invariant), and the reconstruction runs once
> after `collect`, not per batch or per row.

> **Scenario RFC0040.6 — zero cost when unsampled / traces off.**
> **Given** traces disabled, or the query span not sampled,
> **When** a query executes,
> **Then** the plan walk does not run, no operator span is emitted, and the
> query-latency benchmark shows no regression attributable to this feature (the
> default, sampled-out path).

## 6. Testing strategy

Mapped to `CLAUDE.md` §6.2:

- **RFC0040.1 / .2 / .3 / .4** — integration tests in `ourios-querier` (or a
  `ourios-df-otel` test) over the scoped-`InMemorySpanExporter` harness: run a
  real query against a small fixture Parquet set with the query span current,
  then assert the exported spans' names, parent linkage, start/end (against the
  plan's own `aggregate_by_name` timestamps, read independently in the test so
  the assertion is not self-referential), and attributes. A synthetic plan with
  a `metrics()`-`None` node covers .4.
- **RFC0040.5** — a span-count assertion parameterised over N (records) asserting
  operator-span count is constant in N (the RFC 0038.2 shape), and a check that
  the walk is invoked once per query (a counter/mock).
- **RFC0040.6** — a `criterion` guard on the `Parquet → query result` hot-path
  benchmark confirming no regression on the traces-off / unsampled path; a unit
  test that the walk is skipped when the parent context is not sampled.
- Attribute-name conformance rides the existing `weaver registry live-check`
  gate once the `ourios.query.operator.*` / `datafusion.*` names are registered
  (§3.3, §7).
- `ourios-df-otel` unit tests over hand-built `MetricsSet`s: the
  `MetricValue → attribute` mapping, and the timestamp reduction.

## 7. Open questions

- [ ] **Attribute names.** `output_rows`/`elapsed_compute`/`output_bytes`/pruning
      — which map to existing OTel semconv (there is a nascent `db.*` /
      query-engine convention to check via the OTel MCP), which become an
      `ourios.query.operator.*` registry namespace, and which a neutral
      `datafusion.*` set for the extractable crate. Must clear the OTel MCP +
      weaver registry before implementation (`CLAUDE.md` OTel-alignment rule).
- [ ] **Crate name / extraction.** `ourios-df-otel` in-repo, targeting
      `datafusion-opentelemetry` upstream — confirm no such crate already exists
      in `datafusion-contrib` (adopt/align if so). Keep the public surface
      (`record_plan_spans`) Ourios-free from day one.
- [ ] **Querier OTel deps.** §3.4 adds `tracing-opentelemetry` +
      `opentelemetry`(trace) to `ourios-querier`. Acceptable (mirrors RFC 0039),
      or should the parent context be threaded from `ourios-server` to keep the
      querier trace-dep-free? Trade-off: threading a `Context` param vs. a dep.
- [ ] **The "show the query" attribute.** Separately from the operator tree,
      should the query span carry the DSL statement (scrubbed, H6) and/or the
      `displayable(plan)` text as a supplementary attribute — the direct
      `db.query.text` analogue? It interacts with the `skip_all` PII decision
      (RFC 0038 §3.5) and deserves its own note; possibly a small follow-up
      rather than part of this RFC.
- [ ] **Live spans (option b).** Left as the next increment of the extractable
      crate if intra-operator concurrency detail is ever needed.

## 8. References

- RFC 0038 (self-tracing) §3.1 (the `POST /v1/query` span this nests under), §7
  (the `datafusion-opentelemetry` future-work commitment), RFC0038.2 (the
  O(1)-in-records span-count invariant this RFC honours).
- RFC 0021 (DataFusion/arrow upgrade) — the pinned DataFusion 54 whose
  `BaselineMetrics` timestamps make option (a) truthful.
- RFC 0039 (inbound propagation) — the sibling traces-completeness RFC; the same
  dep-promotion pattern.
- `CLAUDE.md` §7 (new crate = architectural commitment → RFC), §3 (H6: no
  DataFusion type crosses the query boundary), §6.3 (observability of
  ourselves), OTel-alignment rule (signal names via the OTel MCP + weaver).
- DataFusion — `ExecutionPlan::{name, children, metrics}`
  (`datafusion-physical-plan-54`), `MetricsSet::aggregate_by_name`,
  `MetricValue::{StartTimestamp, EndTimestamp, OutputRows, ElapsedCompute,
  OutputBytes, PruningMetrics}`, `BaselineMetrics`.
- OpenTelemetry — span builder `with_start_time` / `end_with_timestamp` (backdated
  spans); pinned `opentelemetry` 0.32 / `tracing-opentelemetry` 0.33.
