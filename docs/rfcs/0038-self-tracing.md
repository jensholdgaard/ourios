---
rfc: 0038
title: Self-tracing — the OTel traces signal, disciplined to request scope
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-23
supersedes: —
superseded-by: —
---

# RFC 0038 — Self-tracing — the OTel traces signal, disciplined to request scope

> **Status: `drafted` (2026-07-23).** Sections §§1–4 complete; §5 acceptance
> criteria are the contract, not yet green. No implementation has landed.

## 1. Summary

Ourios dogfoods two of the three OpenTelemetry signals about itself — logs
(via the `tracing` → OTLP appender bridge) and metrics — but not **traces**.
The consequence is concrete: its own log records carry no `trace_id` /
`span_id`, so a warning from the MCP handler cannot be correlated to the
request that caused it. This RFC adds the traces signal, fulfilling
`CLAUDE.md` §6.3 ("every RPC is traced"), which `docs/roadmap.md` records as
deliberately deferred at the first milestone. The commitment is **spans on
request-scoped operations only** — one per query, per MCP tool call, per OTLP
`Export` batch, and per compaction sweep — and a hard rule that the per-record
ingest hot path mints **no spans**. Trace correlation on logs follows for
free, because the log-appender bridge stamps the active span's ids onto every
record it emits.

## 2. Motivation

**Why now.** A telemetry backend whose own logs cannot be trace-correlated is
a credibility gap, and the missing signal was noticed in Ourios's own
dogfooded logs (an rmcp error line with empty `trace_id`/`span_id`). §6.3 has
always required it; the deferral was a scope call, not a design decision.

**Why at this layer.** Traces are a process-global concern owned by
`ourios-telemetry`, the single crate that holds the OTel SDK (RFC 0001 §6.8's
export-architecture split: library crates depend on the API only). Adding a
`SdkTracerProvider` + a `tracing-opentelemetry` layer there is the one place
the change belongs.

**Why the discipline is load-bearing.** Ourios's thesis is query performance,
and its ingest path processes records at high throughput. OpenTelemetry's own
guidance is unambiguous that per-item instrumentation on such a path is wrong:
the Collector coding guidelines say to *"avoid outputting logs per a received
or processed data item … for such high-frequency events instead of logging
consider adding an internal metric,"* and the trace-span guidance restricts
spans to operations that are *significant, have duration, and involve
out-of-process calls* — explicitly **not** short in-process work or
point-in-time occurrences. A span (and its context propagation) per log record
would tax exactly the path the project optimises. So the RFC's central act is
drawing the line, defensibly, between request scope (spans) and record scope
(metrics, which already exist).

## 3. Proposed design

### 3.1 The instrumentation boundary

There is **zero** span instrumentation in the tree today; the change is purely
additive. The boundary:

| Gets exactly one span | Signal | Anchor |
|---|---|---|
| A logs query (`POST /v1/query`) | server span, root | `querier.rs` `handle_query` |
| Each MCP tool call (`query_logs`, `list_templates`, `template_drift`) | span, child of rmcp's own `serve_inner` span | `mcp.rs` `#[tool]` fns |
| One OTLP **Export batch** (gRPC or HTTP) | server span at the shared choke point | `receiver/pipeline.rs` `ingest_bound` |
| One compaction **sweep** | internal span | `compactor.rs` sweep tick |

| Never gets a span (metrics only — already present) |
|---|
| The miner per-record `ingest` / `ingest_mined` / `ingest_structured` |
| The encode-pool per-record `emit_concurrent` worker loop |
| The record-sink per-partition `flush_*` / `drain_*` (async, decoupled from the request) |
| Tenant fan-out's per-`ResourceLogs` loop |

The per-Export-batch span is the correct coarse boundary (OTel's messaging
convention blesses one "Receive/Process" span for a whole batch); it encloses
fan-out + WAL commit + miner hand-off **as a whole**, at zero per-record cost.
Within it, the WAL **group-commit** — the one genuinely I/O-bound,
latency-bearing step (a batched fsync; hazard §3.4 WAL durability-vs-latency) —
gets a single **child** span (`ourios.wal.commit`, `INTERNAL` kind). It has
duration and a meaningful boundary, which OTel's guidance says makes it a span
rather than an event (an event is a point in time and cannot carry the commit
*latency*, which is the whole reason to instrument it). This is the trace's one
sub-span; the per-record loops below it stay bare.
The record-sink flush is genuinely asynchronous — its work outlives the batch
that produced it — so it correctly has **no** span; we do **not** thread batch
context into the buffer to link flushes back (that is the throughput killer to
avoid). Serialize/encode detail, if ever wanted, is a span **event**, not a
span. Per-record observability stays in the metrics the hot path already emits.

Span boundaries coincide with the timing brackets metrics already measure
(`Instant::now()` … `record_ok/err`/`record_sweep`/WAL-commit timing), so a
span is "the causal, parent-child view over the same points metrics already
measure" — minimal new code.

### 3.2 The tracer, in the bootstrap

`ourios-telemetry::init` builds a `SdkTracerProvider` (OTLP `SpanExporter`,
batch processor) alongside the existing meter and logger providers, under the
same "build all fallible steps before installing globals" discipline, and
installs it via `global::set_tracer_provider`. The subscriber registry gains a
`tracing-opentelemetry` `OpenTelemetryLayer` next to the existing appender
bridge and `fmt` layer. Binding `tracing` spans to OTel spans is what makes
the ids exist; the **log-appender bridge then stamps `trace_id`/`span_id` onto
every emitted log record automatically** — no per-call-site change for
correlation.

Two hazards, both flagged for the implementer:

1. **The telemetry-induced-telemetry loop guard must extend to traces.** The
   existing bridge already mutes the exporter's own `tonic`/`hyper`/`h2`/
   `tower`/`opentelemetry*` events; the trace layer needs the **same** filter,
   or the OTLP exporter's transport spans feed back into the exporter.
2. **`TelemetryGuard` must flush the tracer on shutdown** (SIGTERM / `Drop` /
   the subscriber-already-installed teardown branch), so batched spans are not
   lost on exit — the same treatment the logger provider gets.

### 3.3 Span context across the async boundary

The three non-MCP span sites hand work to a detached task — the gRPC/HTTP
receivers `tokio::spawn` the ingest, and the compactor `spawn_blocking`s the
sweep. `tokio::spawn` does **not** propagate span context. Each span is
therefore either opened **inside** the spawned callee (`ingest_bound`, the
sweep body) or the spawned future is `.instrument(Span::current())`-wrapped at
the call site. This RFC prefers opening the span inside the callee (one choke
point, no per-transport duplication). This is the single highest-risk detail
and carries its own acceptance scenario (RFC0038.3).

The MCP tool spans need no such care: rmcp already creates a `serve_inner`
span around dispatch, which — once the trace layer exists — becomes their
parent and starts exporting for free. The querier and OTLP paths have no such
inherited root and get Ourios-created roots.

### 3.4 Configuration and sampling

Sampling is the *second* line of defense (the first is not minting hot-path
spans). The default is **`parentbased_always_on`** — the OTel SDK default, and
the right one here: OTel's guidance says to consider sampling only above
~1000 traces/sec and to *avoid* it at "tens of small traces per second or
lower," which is where Ourios's disciplined span count (per query / MCP call /
Export **batch** / sweep — never per record) sits; and as a self-hosted,
air-gapped binary there is no per-span vendor cost to manage. The one
volume-sensitive span is the per-Export-batch one under heavy ingest, and the
standard `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG` knob (e.g.
`parentbased_traceidratio` at `0.1`) is the operator's lever for exactly that —
Export batches are independent root traces, so ratio-sampling them loses no
cross-request correlation. `TelemetryConfig` gains `traces_enabled: bool`
(default on) and an optional sample ratio; a new `telemetry.*` section in the
RFC 0020 config file
(`traces.enabled`, `traces.sample_ratio`, `otlp.endpoint`) exposes it in the
file front-end — the **first** telemetry config section (telemetry is env-only
today). Traces can be disabled wholesale (`traces.enabled: false`), which
restores today's logs-plus-metrics posture exactly.

## 4. Alternatives considered

**Correlation-only (a tracer that generates ids but exports no spans).** The
appender bridge needs only an active OTel span context to stamp ids, so we
could install the tracer + layer but attach no span exporter — cheaper, and it
fixes the reported symptom. Rejected as a half-step: once the tracer and layer
exist, the exporter is a few lines more and delivers the actual traces signal
§6.3 asks for; shipping ids that point at spans nobody can see is worse
ergonomics than either extreme.

**Full auto-instrumentation (span everything, sample hard).** Wrap every
function / the per-record path in spans and lean on a low sample ratio to
control cost. Rejected: sampling reduces *export* volume but not span
*creation* + context-propagation cost on the hot path, and it muddies traces
with per-record noise that OTel's own guidance says to model as metrics. The
metrics already exist; duplicating them as spans is pure cost.

**Do nothing / keep traces deferred.** Rejected: it leaves §6.3 unmet and the
self-logs uncorrelatable, and the deferral's original rationale (first-
milestone scope) has expired.

**tracing's `trace_id` via a non-OTel mechanism (e.g. a request-id field).**
Rejected: it would not interoperate with the OTel traces signal a user's
Collector expects, and Ourios's whole posture is OTel-native.

## 5. Acceptance criteria

> **Scenario RFC0038.1 — request-scoped operations open exactly one span, and
> their logs carry the trace context.**
> **Given** a server with traces enabled and an always-on sampler,
> **When** a logs query, an MCP `query_logs` call, a single OTLP `Export`
> batch, and a compaction sweep each execute,
> **Then** each produces the expected span(s): one server span for the query,
> one child-of-`serve_inner` span for the MCP call, one internal span for the
> sweep, and — for the Export — one server batch span with a single
> `ourios.wal.commit` child span (and no further sub-spans), **And** any log
> record emitted within that operation carries the operation's
> `trace_id`/`span_id` (the correlation the reported gap was about).

> **Scenario RFC0038.2 — the ingest hot path mints no per-record spans.**
> **Given** traces enabled and an always-on sampler,
> **When** one `Export` batch of N records is ingested,
> **Then** the number of spans produced by the ingest path is bounded by the
> batch/commit structure and is **independent of N** (O(1) in the record
> count, not O(N)) — the miner, encode-pool, and record-sink inner loops
> create none — **And** the ingest-throughput benchmark shows no regression
> attributable to tracing beyond the per-batch span (a documented ceiling).

> **Scenario RFC0038.3 — span context survives the spawn boundary.**
> **Given** the receiver's `tokio::spawn`ed ingest and the compactor's
> `spawn_blocking`ed sweep,
> **When** each runs,
> **Then** the batch/sweep span is present and correctly parented (not
> orphaned), so records/log lines produced under it resolve to the batch's
> trace — verified by asserting the emitted log's `trace_id` equals the span's
> (the `tokio::spawn` context-loss trap is closed).

> **Scenario RFC0038.4 — sampling is configurable and defaults sane.**
> **Given** the standard `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG`
> knobs (and the config-file equivalent),
> **When** the sampler is left unset, set to `parentbased_traceidratio` at a
> ratio, and disabled (`traces.enabled: false`),
> **Then** the default samples (root) traces, the ratio sampler exports the
> configured fraction deterministically, and disabling restores the exact
> logs-plus-metrics-only behaviour (no tracer installed, no `trace_id` on
> logs, no throughput or dependency change).

> **Scenario RFC0038.5 — no telemetry-induced-telemetry loop.**
> **Given** the OTLP span exporter's own transport stack (`tonic`/`hyper`/…)
> emits spans/events,
> **When** traces are enabled,
> **Then** those exporter-internal spans are muted by the same loop-guard
> filter that mutes them for the logs bridge — exporting a span does not
> generate more spans about the export.

> **Scenario RFC0038.6 — spans flush on shutdown.**
> **Given** a batch span processor with buffered spans,
> **When** the server shuts down (SIGTERM / `TelemetryGuard::shutdown` /
> `Drop`),
> **Then** the tracer provider is flushed alongside the logger and meter
> providers, and no acknowledged-window span is dropped on a clean exit.

## 6. Testing strategy

Mapped to `CLAUDE.md` §6.2:

- **RFC0038.1 / .3 / .5 / .6** — integration tests in `ourios-server` /
  `ourios-ingester` using an in-memory span exporter (SDK test exporter):
  drive a query, an MCP tool call, an `Export`, and a sweep; assert span
  count/name/parentage and that a co-emitted log's `trace_id` matches. A
  dedicated case asserts the spawn-boundary parentage (.3) and the loop-guard
  muting (.5), and a shutdown case asserts the flush (.6).
- **RFC0038.2** — a span-count assertion parameterised over batch size N
  (spans are O(1) in N), plus a `criterion` guard on the ingest
  (`OTLP → WAL`, `WAL → Parquet`) hot-path benchmarks confirming no
  per-record tracing cost — a regression there blocks merge (§6.2 benchmarks).
- **RFC0038.4** — unit/integration over the sampler configuration surface:
  default, ratio (deterministic fraction over a fixed set of trace-ids), and
  the `traces.enabled: false` disable path (asserting no tracer and no
  `trace_id` on logs).

## 7. Open questions

- [ ] Span/attribute **naming**: adopt HTTP server semconv for the query/MCP
      spans (`{method} {route}`) and a bespoke `ourios.ingest.batch` /
      `ourios.compaction.sweep` for the internal ones? Confirm attribute set
      (tenant, template counts) against semconv cardinality guidance.
- [ ] Should the per-Export-batch span live on `ingest_bound` (single choke
      point, preferred) or on each transport handler (`export`/`handle_logs`)?
      §3.3 prefers the former; confirm no transport-specific attributes are
      lost.
- [ ] `tracing-opentelemetry` version alignment with the pinned
      `opentelemetry` `0.x`; confirm no version-skew with the appender/exporter
      crates before adding the dependency.

**Future work (out of scope here).** A reusable **DataFusion → OTel
instrumentation** — per-operator / per-`ExecutionPlan`-node sub-spans, bridging
DataFusion's existing per-operator `MetricsSet` into the trace — would deepen
the query span into an operator tree. It is a community-shaped component (a
standalone `datafusion-opentelemetry` crate, most naturally offered to
`datafusion-contrib` and announced to the OTel Rust ecosystem), best built for
Ourios's own query span first and then extracted upstream — the same
dogfood-then-give-back path as Ourios's `opentelemetry-rust` contributions.
This RFC's query span (§3.1) is exactly the parent such operator sub-spans
would attach to, so nothing here blocks it and the boundary discipline (query
scope, not ingest) already covers it.

## 8. References

- `CLAUDE.md` §6.3 (Observability of ourselves — "every RPC is traced"); §6.2
  (testing discipline, benchmarks block regressions).
- `docs/roadmap.md` (traces "deliberately deferred").
- RFC 0001 §6.8 (export architecture: API-only library crates, SDK in
  `ourios-telemetry`).
- RFC 0020 (configuration file — where the new `telemetry.*` section lands).
- OpenTelemetry — [defining spans](https://opentelemetry.io/docs/specs/semconv/how-to-write-conventions/#defining-spans)
  (significant, has duration, out-of-process; not for short in-process work);
  [Collector coding guidelines](https://github.com/open-telemetry/opentelemetry-collector/blob/main/docs/coding-guidelines.md)
  (no per-item logging/spans — use a metric); messaging spans (one
  Receive/Process span per batch; links over nested per-item spans);
  [sampling](https://opentelemetry.io/docs/concepts/sampling/) and the
  `OTEL_TRACES_SAMPLER` environment surface.
