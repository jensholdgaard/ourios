---
rfc: 0039
title: Inbound trace-context propagation — SERVER spans continue the caller's trace
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-25
supersedes: —
superseded-by: —
---

# RFC 0039 — Inbound trace-context propagation

## 1. Summary

Ourios's request-scoped SERVER spans (RFC 0038) are currently created as trace
**roots**: they never read the incoming W3C `traceparent`/`tracestate`, so a
caller's trace stops at the Ourios boundary. This RFC installs a global
`TraceContextPropagator` and, at each ingress, extracts the caller's
`opentelemetry::Context` from the request carrier (HTTP headers / gRPC metadata)
and attaches it as the **current** context around the span-producing future
(`FutureExt::with_context`), so the root span inherits it as parent — not via
`set_parent`, which fails on an already-entered `#[tracing::instrument]` span.
The observable result: the `ingest logs`, `POST /v1/query`, and MCP tool spans join
the caller's distributed trace instead of starting a disconnected one, and a
`parentbased` sampler honours the caller's sampling decision. No new signal, no
schema change — this completes the traces pillar that RFC 0038 established.

## 2. Motivation

The point of a `SERVER`-kind span is to be the server half of a client's
request: linked to the caller's `CLIENT`/`PRODUCER` span through propagated
context, so an operator can follow one trace from the application that emitted a
log, through the OTLP exporter, into Ourios's ingest path — or from a query
client into Ourios's querier. RFC 0038 built the spans but not the propagation,
so today every Ourios span is a root. For a telemetry backend that sits *inside*
someone else's distributed system, that is the most consequential remaining gap
in the traces signal: correlation-within-Ourios works (RFC 0038), but
correlation-across-the-boundary does not.

This is deliberately a small, bounded change at the ingress layer. It touches
the traces pillar (hence an RFC), but it adds no new spans, no new attributes of
consequence, and no on-disk change — it only sets the *parent* of spans that
already exist.

## 3. Proposed design

### 3.1 The one global: a W3C propagator

Install the W3C Trace Context propagator once, in `ourios-telemetry`'s `init()`,
alongside the existing provider installation:

```rust
opentelemetry::global::set_text_map_propagator(
    opentelemetry_sdk::propagation::TraceContextPropagator::new(),
);
```

This is unconditional (cheap, stateless) and independent of whether the traces
pipeline is enabled — extraction is a no-op when no exporter is installed, and
installing the propagator regardless keeps the ingress code uniform. `baggage`
propagation is out of scope (§7).

### 3.2 The ingress map

**Four** ingress categories open a span on the request path — six span-producing
functions in all, since the MCP category is three tool functions. The count is
stated so test coverage (RFC0039.1/.3/.6) omits no site. The carrier — where the
incoming `traceparent` lives — is not always co-located with the span:

| # | Span | Site (file:line) | Carrier & where it is reachable |
|---|---|---|---|
| A | `ingest logs` (gRPC) | span in `ingest_bound` (`pipeline.rs:291`); entry `LogsReceiver::export` (`grpc.rs:152`) | raw `http::HeaderMap` in the tower auth layer `AuthService::call` (`grpc.rs:102`, already reads `request.headers()`) |
| B | `ingest logs` (HTTP) | span in `ingest_bound`; entry `handle_logs` (`http.rs:95`) | axum `HeaderMap` (`http.rs:95`) |
| C | `POST /v1/query` | `handle_query` (`querier.rs:421`) | axum `HeaderMap` (`querier.rs:423`) |
| D | `execute_tool <tool>` (×3) | the three `_traced` fns (`mcp.rs:333/416/488`), each via a thin `#[tool]` delegate | `ctx.extensions.get::<axum::http::request::Parts>()?.headers` (as `mcp_session_id` reads, `mcp.rs:223`) |

The mechanism is uniform (§3.3): extract the caller's `opentelemetry::Context`
and make it the **current** context around the span-producing future, so the
span — a tracing root — inherits it as its OTel parent.

### 3.3 The mechanism: attach the context, do not `set_parent`

`OpenTelemetrySpanExt::set_parent` must be called *before* the span is entered.
On an already-entered span — which every `#[tracing::instrument]` span is, for
its whole body — it returns `SetParentError::AlreadyStarted` and the parent is
**silently not set**. So propagation cannot `set_parent` from inside an
instrumented fn. Instead it makes the extracted context **current** *before* the
span is built; `tracing-opentelemetry` then parents a root span to
`Context::current()`. The idiom is
`opentelemetry::trace::FutureExt::with_context(future, cx)` — run the
span-producing future under the extracted context. One contract, every site:

- **Query (C) and HTTP ingest (B):** a tower `PropagationLayer` on the axum
  router extracts `cx` from the request `HeaderMap` and runs the downstream as
  `next.run(req).with_context(cx)`. `handle_query`'s root span inherits `cx`; the
  handler is unchanged.
- **gRPC ingest (A):** the same extraction in the existing tower auth layer
  (`AuthService::call`, `grpc.rs:102`), stashing `cx` in the request extensions
  beside the auth binding.
- **The `tokio::spawn` boundary (A/B):** the `ingest logs` span is born inside
  `ingest_bound`, *after* the spawn (`grpc.rs:167`, `http.rs:146`), which a
  layer's `with_context` does not cross. So the handler reads `cx` (from the
  extension for A, extracts directly for B), moves it into the spawned closure,
  and runs `ingest_bound(...).with_context(cx).await`. The span, first polled
  under `cx`, inherits it — **no `ingest_bound` signature change, no
  `set_parent`.**
- **MCP (D):** the un-instrumented `#[tool]` delegate extracts `cx` from `ctx`'s
  forwarded headers and runs `self.<tool>_traced(...).with_context(cx).await`;
  the `_traced` span inherits `cx` across rmcp's own dispatch spawn.

This is the same discipline RFC 0038.3 uses to carry work across `tokio::spawn`,
applied here to the parent context — and it is one uniform contract, resolving
the earlier draft's split between an explicit parameter and a request extension.

### 3.4 The extractor shim

`opentelemetry::propagation::Extractor` is a two-method trait (`get`, `keys`).
One adapter suffices: `struct HeaderExtractor<'a>(&'a http::HeaderMap)`, since
`http::HeaderMap` is the carrier for **every** site — the gRPC path extracts from
the raw HTTP headers at the tower auth layer (`grpc.rs:102`), so no tonic
`MetadataMap` adapter is needed. Extraction goes through the propagator installed
in §3.1: `global::get_text_map_propagator(|p| p.extract(&HeaderExtractor(headers)))`.
`opentelemetry-http` ships an equivalent `HeaderExtractor`; the ~10-line local
one avoids a dependency (revisit if a metadata extractor is ever needed).

### 3.5 Dependency promotion (the one production-surface change)

The ingress code needs `opentelemetry` types in production
(`Context`, `propagation::Extractor`, `trace::FutureExt::with_context`,
`global::get_text_map_propagator`), but `opentelemetry` is a production
dependency of `ourios-ingester`/`ourios-server` today only with the
**`metrics`** feature. This RFC adds the **`trace`** feature to that existing
dependency in both crates. The propagator *install*
(`opentelemetry_sdk::propagation::TraceContextPropagator`, §3.1) stays in
`ourios-telemetry`, which already depends on `opentelemetry_sdk`; and because the
parenting is via the current-context bridge the `tracing-opentelemetry` layer
already provides (not a `set_parent` call), **`tracing-opentelemetry` is not
needed in the ingress crates at all**. So the whole production-surface cost is
one added feature flag on a crate already depended on — smaller than a
`set_parent` design would have required.

### 3.6 Sampling interplay

With a parent context attached, the SDK's default `parentbased_always_on`
sampler (RFC 0038 §3.4, resolved from `OTEL_TRACES_SAMPLER`) honours the
caller's sampled flag: a caller who sampled the trace propagates `sampled=1` and
Ourios records/exports its spans within that trace; a caller who did not
propagates `sampled=0` and Ourios's spans are dropped, keeping the trace
consistent end-to-end. This is desirable and is the reason to prefer a
`parentbased` sampler as the default — it is what makes propagation meaningful.
A request with no incoming context falls back to the root sampling rule
unchanged (backward-compatible).

### 3.7 Traces disabled

`with_context` merely attaches an `opentelemetry::Context` for the duration of a
future; it has no fallible surface and no `Result` to handle (contrast the
`set_parent` design, which returned `SetParentError` — one reason to prefer the
attach idiom). When traces are disabled the span carries no OTel layer, the
attached context is inert, and nothing is exported — a no-op, not an error. No
`unwrap`/`expect` is introduced (`CLAUDE.md` §6.1).

## 4. Alternatives considered

**Do nothing (status quo — roots).** Correlation within Ourios works; the cost
is that no operator can follow a trace across the Ourios boundary. For a
telemetry backend this is precisely the interesting join, so the gap is not
acceptable long-term.

**Extract at the shared `ingest_bound` span only, via ambient context.** Fails:
the carrier does not reach `ingest_bound` (its signature has no request), and
`tokio::spawn` severs ambient context (§3.3). Extraction must happen in the
handler.

**A single `set_parent` call inside each instrumented fn.** The obvious first
design, and what an earlier draft proposed — but it does not work:
`OpenTelemetrySpanExt::set_parent` returns `AlreadyStarted` on an entered span,
and every `#[tracing::instrument]` span is entered for its body, so the parent is
silently dropped (§3.3). The attach-the-context idiom (`with_context`) is the
correct primitive and is what §3.3 adopts.

**Per-handler extraction with no shared layer.** Workable but repetitive: each
handler would extract and wrap. The `PropagationLayer` (§3.3) centralises the
request-local sites (B/C); only the spawn-crossed span (A/B's `ingest_bound`) and
MCP (D, behind rmcp's dispatch) need the explicit `with_context` hand-off, which
no layer can do for them anyway.

**Adopt `opentelemetry-http`'s `HeaderExtractor` as a dependency.** Reasonable,
but it is one more crate for a ~10-line shim; the RFC inlines the extractor. If
a tonic-metadata extractor is later needed, revisit.

## 5. Acceptance criteria

> **Scenario RFC0039.1 — a SERVER span continues an incoming trace.**
> **Given** the traces pipeline enabled and the global `TraceContextPropagator`
> installed,
> **When** a `POST /v1/query` request and an OTLP `Export` (both gRPC and HTTP)
> each arrive carrying a valid W3C `traceparent` for trace `T` span `S`,
> **Then** the resulting `POST /v1/query` and `ingest logs` spans each have
> `trace_id == T` and parent span id `== S` (they are children of the caller's
> span, not roots).

> **Scenario RFC0039.2 — no incoming context is a fresh root, unchanged.**
> **Given** the same setup,
> **When** a request arrives with **no** `traceparent`,
> **Then** the span is a fresh root with a newly minted `trace_id` and no
> parent — identical to pre-RFC behaviour, and no error is raised.

> **Scenario RFC0039.3 — the extracted context survives the ingest spawn.**
> **Given** the gRPC and HTTP OTLP receivers, whose `ingest logs` span is created
> inside a `tokio::spawn`ed `ingest_bound`,
> **When** a batch arrives carrying `traceparent` for trace `T`,
> **Then** the `ingest logs` span (and its `commit wal` child) resolve to
> `trace_id == T` — proving the parent context was extracted before the spawn and
> applied to the post-spawn span (the RFC 0038.3 boundary, for the parent
> context this time).

> **Scenario RFC0039.4 — the caller's sampling decision is honoured.**
> **Given** the default `parentbased` sampler,
> **When** a request carries `traceparent` with the sampled flag **unset**
> (`-00`), and separately with it **set** (`-01`),
> **Then** the unset case produces **no** exported span (the trace was not
> sampled upstream), and the set case exports the span within trace `T` — the
> parent decision governs, end to end.

> **Scenario RFC0039.5 — a malformed carrier is treated as absent.**
> **Given** the propagator,
> **When** a request carries a syntactically invalid `traceparent`,
> **Then** extraction yields an empty context, the span becomes a fresh root
> (as RFC0039.2), and no panic or request error occurs.

> **Scenario RFC0039.6 — the MCP tool span joins the caller's trace.**
> **Given** an MCP `tools/call` over `/mcp` carrying `traceparent` for trace `T`,
> **When** the tool executes,
> **Then** the `execute_tool <tool>` span resolves to `trace_id == T`, parented
> to the caller — so an agent driving Ourios's tools sees the tool execution
> inside its own trace.

## 6. Testing strategy

Mapped to `CLAUDE.md` §6.2:

- **RFC0039.1 / .2 / .5 / .6** — integration tests in `ourios-server` /
  `ourios-ingester` using the RFC 0038 scoped-`InMemorySpanExporter` harness:
  drive `handle_query`, `handle_logs`, and (global-tracer binary, per RFC0038.1
  MCP arm) an MCP `tools/call`, each with an injected `traceparent` header, then
  assert `SpanData.span_context.trace_id()` / `.parent_span_id()`. The
  no-context and malformed-context cases assert a fresh, valid root and no error.
- **RFC0039.3** — extends the RFC0038.3 spawn-boundary harness
  (`rfc0038_3_spawn_boundary.rs`, global tracer): call `LogsReceiver::export`
  directly with a `traceparent` in the request metadata/headers and assert the
  `ingest logs` + `commit wal` spans carry the injected `trace_id`.
- **RFC0039.4** — a sampler test: with `OTEL_TRACES_SAMPLER=parentbased_always_on`
  (default), inject `-00` vs `-01` traceparents and assert exported-span presence.
  The parent-based resolution itself is upstream SDK behaviour; the test covers
  Ourios's wiring (that the extracted context reaches the sampler).
- The extractor shims get a unit test (round-trip a `traceparent` through a
  `HeaderMap` and back to a `SpanContext`).

## 7. Open questions

- [ ] Confirm `FutureExt::with_context` correctly re-attaches the extracted
      context inside the spawned `ingest_bound` task (§3.3) — the spawn-boundary
      test (RFC0039.3) is the check. (The carry-channel and site-A/metadata-vs-
      header questions the earlier draft left open are now settled by §3.3/§3.4:
      one `HeaderExtractor` over the raw `http::HeaderMap`, `cx` in the request
      extension across the spawn, no `ingest_bound` signature change.)
- [ ] MCP tool spans are `otel.kind = "internal"` and lack an enclosing Ourios
      SERVER span for `/mcp` (rmcp's `serve_inner` is muted by the `rmcp=off`
      loop-guard, RFC0038.7). An INTERNAL span continuing a *remote* parent is
      valid but slightly unusual — is a dedicated `/mcp` SERVER span warranted
      instead? Deferred; RFC0039.6 parents the INTERNAL span directly for now.
- [ ] `tracestate` and `baggage`: `tracestate` rides along with `TraceContext`
      automatically; `baggage` propagation is explicitly out of scope here.
- [ ] Response-side **injection** (Ourios as a client to object storage / a
      downstream) is a separate concern — not in this RFC (inbound only).

## 8. References

- RFC 0038 (self-tracing) — the spans this RFC gives parents to; §3.3 (the
  `tokio::spawn` boundary), §3.4 (the sampler), RFC0038.3 (spawn-boundary test
  harness), RFC0038.7 (`rmcp=off` loop-guard).
- `CLAUDE.md` §6.3 (observability of ourselves), §2 (the traces pillar via
  RFC 0038), §6.1 (no `unwrap`/`expect` in non-test code).
- W3C Trace Context — <https://www.w3.org/TR/trace-context/>.
- OpenTelemetry — [context propagation](https://opentelemetry.io/docs/specs/otel/context/api-propagators/);
  [`FutureExt::with_context`](https://docs.rs/opentelemetry/0.32.0/opentelemetry/trace/trait.FutureExt.html)
  (the attach idiom this RFC uses; note `OpenTelemetrySpanExt::set_parent`
  returns `AlreadyStarted` on an entered span, which is why it is *not* used).
- Pinned: `opentelemetry` 0.32.0, `opentelemetry_sdk` 0.32.1,
  `tracing-opentelemetry` 0.33.0.
