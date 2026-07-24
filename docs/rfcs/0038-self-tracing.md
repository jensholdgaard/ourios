---
rfc: 0038
title: Self-tracing — the OTel traces signal, disciplined to request scope
status: green
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-23
supersedes: —
superseded-by: —
---

# RFC 0038 — Self-tracing — the OTel traces signal, disciplined to request scope

> **Status: `green` (2026-07-24).** All seven §5 acceptance criteria are
> implemented and pass: RFC0038.1 (request-scope spans + log correlation,
> #614/#615/#616/#617), RFC0038.2 (ingest O(1), #615), RFC0038.3 (spawn-boundary
> context, #617), RFC0038.4 (traces configured via the universal OTel SDK env
> vars — no bespoke Ourios surface — with the `OTEL_TRACES_EXPORTER=none`
> disable mapping tested), RFC0038.5 (loop guard, #614), RFC0038.6 (flush on
> shutdown, #614), RFC0038.7 (canonical GenAI/MCP span attributes with the
> genai-relocation live-check exemption, §3.6; exemption tracked by #622). §3.4
> was amended to lean on the universal OTel env vars instead of a bespoke
> config-file sampler surface (maintainer decision, 2026-07-24).

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
gets a single **child** span (`commit wal`, `INTERNAL` kind). It has
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
cross-request correlation.

**Lean on the universal OTel SDK env vars — no bespoke Ourios config.** These
env vars are the config contract operators already know; inventing a parallel
Ourios surface for the same thing is drift and a second way to configure one
knob. So Ourios configures traces entirely through the standard SDK vars and
does not couple a unique config to them:

- **Sampler:** Ourios does **not** call `.with_sampler(...)`. The SDK resolves
  the sampler from `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG` (any
  standard sampler name; default `parentbased_always_on`). Invalid values are
  logged and ignored by the SDK per the env-var spec — Ourios does not add its
  own validation or precedence layer.
- **Disable:** the standard per-signal switch `OTEL_TRACES_EXPORTER=none` turns
  the traces pipeline off, restoring today's logs-plus-metrics posture exactly
  (no tracer, no `trace_id` on logs). `init()` reads it directly — Ourios plays
  the "autoconfigure" role Go's `autoexport` / Java's autoconfigure play, since
  the Rust SDK's manual exporter construction reads no exporter-selector var
  (#618). `TelemetryConfig.traces_enabled` (default on) remains a programmatic
  override on top; `OTEL_SDK_DISABLED=true` disables all three signals together.
- **Endpoint / transport:** `OTEL_EXPORTER_OTLP_ENDPOINT` and the other
  `OTEL_EXPORTER_OTLP_*` vars, already read by the SDK exporter.

There is no `telemetry.traces.*` config-file section and no file-vs-env
precedence: the SDK's own env resolution is authoritative.

### 3.5 Span names and attributes

Names are fixed here (low cardinality, ids as attributes not names) and follow
OTel's span-naming guidance: the `{action} {target}` pattern, no static
namespace prefix in the name (the `ourios.*` dotted style is for *metrics*, not
spans; a span's origin is the `service.name` resource attribute, so an
`ourios.` prefix would be exactly the redundant static text the spec says to
drop). The MCP tool spans adopt the GenAI convention's `execute_tool
{tool.name}` form, so Ourios's own agent-facing tool calls interoperate with
GenAI-aware backends. So the §5 contract is complete:

| Operation | Span name | Kind |
|---|---|---|
| Logs query | `POST /v1/query` (HTTP `{method} {route}`) | `SERVER` |
| MCP tool call | `execute_tool query_logs` / `execute_tool list_templates` / `execute_tool template_drift` (GenAI `execute_tool {tool.name}`) | `INTERNAL` (child of rmcp `serve_inner`) |
| OTLP Export batch | `ingest logs` | `SERVER` |
| WAL group-commit | `commit wal` | `INTERNAL` (child of the batch) |
| Compaction sweep | `sweep partitions` | `INTERNAL` |

Required attributes are low-cardinality and set at span start (so they are
available to sampling): the query and MCP spans carry `ourios.tenant` (the
query span also the standard `http.request.method` / `http.route` /
`http.response.status_code`); the ingest-batch span carries the batch's record
count and the number of distinct tenants it fanned out to (counts, not ids);
the sweep span carries the partitions/files swept. Tenant and other identifiers
are **attributes**, never part of the span name, keeping names low-cardinality.

### 3.6 GenAI/MCP semantic-convention attributes on the tool spans

The MCP tool spans are Ourios's agent-observability surface: an agent driving
the `/mcp` tools should see them exactly as it sees any GenAI tool call. Each
`execute_tool {tool}` span therefore carries the canonical OTel attributes —
`gen_ai.operation.name = execute_tool`, `gen_ai.tool.name` (the tool), and
`mcp.method.name = tools/call`, plus `mcp.session.id` recorded from the
forwarded `mcp-session-id` header so an agent's calls within one session
correlate. The span name follows the GenAI `{gen_ai.operation.name}
{gen_ai.tool.name}` form (`execute_tool query_logs` etc.); because
`#[tracing::instrument]` requires a static name literal, the name and the two
attributes are written separately per tool rather than one derived from the
other, so the MCP-span unit test asserts **both** the name and the attribute
values together — a drift between them fails the test.

These four attributes **moved** out of core semantic-conventions to the separate
[`semantic-conventions-genai`](https://github.com/open-telemetry/semantic-conventions-genai)
registry; in our pinned dependency (semconv v1.42.0) they survive only as
**deprecated** "Moved to …" stubs, which `weaver registry live-check` reports as
`violation`s. weaver cannot take a second registry dependency (`not yet
implemented: Multiple dependencies is not supported yet`), and v1.42.0 still
ships the `gen-ai`/`mcp` model besides — so a second dependency would also
collide on group ids. The live-check job therefore gates on a **filtered**
violation count that exempts *only* the genai-relocation deprecation for the
`gen_ai.*`/`mcp.*` namespaces; every other violation (including any other
deprecation on those keys) still fails. Issue #622 tracks collapsing this into a
single genai dependency once upstream deletes its v1.42 copies.

Driving an MCP call through live-check also surfaces `rmcp`'s **own** internal
instrumentation (bare `session_id` / `peer_info` / `notification` fields on
events at `rmcp` source lines) — non-semconv third-party noise, not Ourios
signal. That is muted at the source, alongside the export-stack loop guard, in
`ourios-telemetry`'s `guarded_env_filter` (`rmcp=off`); Ourios's own
`execute_tool` span (target `ourios_server::mcp`) is unaffected.

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
> `commit wal` child span (and no further sub-spans), **And** any log
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

> **Scenario RFC0038.4 — traces configure through the universal OTel SDK env
> vars, and disabling is the standard per-signal switch.**
> **Given** the standard `OTEL_TRACES_SAMPLER` / `OTEL_TRACES_SAMPLER_ARG` and
> `OTEL_TRACES_EXPORTER` env vars (no bespoke Ourios config surface),
> **When** the sampler is left unset; set via env `parentbased_traceidratio` at
> a ratio; and `OTEL_TRACES_EXPORTER=none`,
> **Then** the default samples (root) traces (`parentbased_always_on`, the SDK
> default — Ourios does **not** override the sampler); the env ratio sampler
> exports the configured fraction (the SDK's own resolution, which Ourios does
> not alter); and `OTEL_TRACES_EXPORTER=none` (honored by `init()`) installs
> **no** tracer and stamps **no** `trace_id`/`span_id` on log
> records — the observable, runtime logs-plus-metrics-only behaviour (no
> throughput change). (Sampler resolution and invalid-value handling are the
> SDK's universal, upstream-tested behaviour; Ourios tests only its own mapping
> of `OTEL_TRACES_EXPORTER=none` to the disable path.)

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

> **Scenario RFC0038.7 — MCP tool spans carry the canonical GenAI/MCP
> attributes, and only their relocation is exempted from live-check.**
> **Given** the `/mcp` tool surface with traces enabled,
> **When** an agent invokes a tool over an established MCP session,
> **Then** the `execute_tool {tool}` span carries `gen_ai.operation.name =
> execute_tool`, `gen_ai.tool.name` (the invoked tool), `mcp.method.name =
> tools/call`, and `mcp.session.id` (the caller's session), **And**
> `weaver registry live-check` over the emitted telemetry reports no violation
> other than the sanctioned "moved to semantic-conventions-genai" deprecation
> for the `gen_ai.*`/`mcp.*` namespaces — every other drift still fails the gate
> (§3.6; the exemption's removal is tracked by #622).

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
- **RFC0038.4** — a unit test over Ourios's own mapping: `OTEL_TRACES_EXPORTER`
  → whether the traces pipeline installs (`none` → off; unset / `otlp` / any
  other → on). Sampler resolution (`OTEL_TRACES_SAMPLER`/`_ARG`) is the SDK's
  universal, upstream-tested behaviour that Ourios no longer overrides — there
  is nothing Ourios-specific left to test there.
- **RFC0038.7** — the `ourios-server` MCP-span integration test asserts the
  `gen_ai.*`/`mcp.*` attributes (including the session id) on the emitted span;
  the `live-check` CI job proves emission-time semconv conformance, gating on
  the genai-relocation-filtered violation count so a real drift on any other
  attribute still fails (§3.6).

## 7. Open questions

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
