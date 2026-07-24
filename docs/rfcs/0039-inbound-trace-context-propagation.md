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
and attaches it as the span's parent via `OpenTelemetrySpanExt::set_parent`. The
observable result: the `ingest logs`, `POST /v1/query`, and MCP tool spans join
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

Five span sites open on the request path. The carrier — where the incoming
`traceparent` lives — is not always co-located with the span:

| # | Span | Site (file:line) | Carrier & where it is reachable |
|---|---|---|---|
| A | `ingest logs` (gRPC) | span in `ingest_bound` (`pipeline.rs:291`); entry `LogsReceiver::export` (`grpc.rs:152`) | tonic `MetadataMap` via `request.metadata()` (`grpc.rs:159`), or raw `http::HeaderMap` in the tower auth layer `AuthService::call` (`grpc.rs:102`) |
| B | `ingest logs` (HTTP) | span in `ingest_bound`; entry `handle_logs` (`http.rs:95`) | axum `HeaderMap` (`http.rs:95`) |
| C | `POST /v1/query` | `handle_query` (`querier.rs:421`) | axum `HeaderMap` (`querier.rs:423`) — same fn as the span |
| D | `execute_tool <tool>` | the three `_traced` fns (`mcp.rs:333/416/488`) | `ctx.extensions.get::<axum::http::request::Parts>()?.headers` (as `mcp_session_id` already reads, `mcp.rs:223`) |

For **C** and **D** the carrier and the span live in the same function:
extract and `set_parent` at the top of the instrumented fn.

### 3.3 The `tokio::spawn` boundary (ingest)

The ingest span is the hard case. For **A** and **B** the `ingest logs` span is
created inside `ingest_bound`, which runs inside a freshly `tokio::spawn`ed task
(`grpc.rs:167`, `http.rs:146`) — the same spawn boundary RFC 0038.3 is about.
The carrier is only reachable in the handler **before** the spawn; the span is
born **after** it, in a different task. Ambient current-context does not cross
`tokio::spawn`.

Therefore the fix cannot rely on context flow. The handler must:

1. Extract `let cx = propagator.extract(&carrier);` **before** the spawn (where
   the `MetadataMap`/`HeaderMap` is in scope).
2. Move `cx` into the spawned closure and hand it to `ingest_bound` as an
   explicit parameter (`parent: opentelemetry::Context`).
3. Inside `ingest_bound`, after the span is entered, call
   `tracing::Span::current().set_parent(cx)`.

This mirrors how RFC 0038.3 already moves *span* context across the same
boundary via `.instrument(Span::current())`; here it is the extracted *parent*
context that is moved. `ingest_bound`'s signature gains one parameter; the two
call sites (`grpc.rs`, `http.rs`) each extract before spawning.

### 3.4 The extractor shims

`opentelemetry::propagation::Extractor` is a two-method trait (`get`, `keys`).
Two thin adapters are needed:

- **axum `HeaderMap`** — `opentelemetry-http` provides `HeaderExtractor`, but to
  avoid a new dependency a ~10-line local `struct HeaderExtractor<'a>(&'a
  HeaderMap)` is equivalent (the C/D sites and the HTTP ingest site B).
- **tonic `MetadataMap`** — a `struct MetadataExtractor<'a>(&'a MetadataMap)`
  reading ASCII metadata keys (site A, if extracting from gRPC metadata rather
  than the raw HTTP headers at the tower layer).

Both live in `ourios-ingester`'s receiver module (and re-used by
`ourios-server`), or in a small shared helper. The RFC prefers extracting site A
from the **tower auth layer's raw `http::HeaderMap`** (`grpc.rs:102`), so a
single `HeaderExtractor` covers A/B/C/D and no tonic-metadata adapter is needed —
the auth layer already reads `request.headers()` and inserts into extensions, so
the extracted `Context` can ride the same request-extensions channel the auth
binding uses (`grpc.rs:119`), reaching `ingest_bound` without a signature change.
This is the preferred shape; §7 leaves the exact carry-channel (explicit
parameter vs. request extension) to implementation review.

### 3.5 Dependency promotion (the non-obvious cost)

Today the trace-capable OTel crates are `[dev-dependencies]` only in
`ourios-ingester` and `ourios-server` — their production `[dependencies]` carry
`opentelemetry` with the **`metrics`** feature alone. The `otel.kind` string
fields on the instrument macros work without them because the tracing→OTel
bridge lives in `ourios-telemetry`. Propagation needs real types in production
code (`TraceContextPropagator`, `Context`, `OpenTelemetrySpanExt::set_parent`),
so this RFC promotes to `[dependencies]` in both crates:

- `opentelemetry` (add the `trace` feature),
- `opentelemetry_sdk` (`trace` feature, for `TraceContextPropagator`),
- `tracing-opentelemetry` (for `OpenTelemetrySpanExt`).

This is a real compile-surface and build-time cost and is called out here so it
is a conscious choice, not a surprise in the diff.

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

### 3.7 `set_parent`'s `Result`

`tracing-opentelemetry` 0.33's `set_parent` returns `Result<(),
SetParentError>`. A failure means the span had no OTel layer (traces disabled) —
expected and non-fatal. The call ignores the error (`let _ = …`) or matches it
away; it is never a request-affecting error (no `unwrap`/`expect`, per
`CLAUDE.md`).

## 4. Alternatives considered

**Do nothing (status quo — roots).** Correlation within Ourios works; the cost
is that no operator can follow a trace across the Ourios boundary. For a
telemetry backend this is precisely the interesting join, so the gap is not
acceptable long-term.

**Extract at the shared `ingest_bound` span only, via ambient context.** Fails:
the carrier does not reach `ingest_bound` (its signature has no request), and
`tokio::spawn` severs ambient context (§3.3). Extraction must happen in the
handler.

**A tower/tonic middleware layer that extracts and injects context for all
routes.** Cleaner in principle (one layer, no per-handler code), and worth
revisiting — but the ingest span is born *after* the spawn inside `ingest_bound`,
so a middleware that sets the current context still would not reach that span
without the same explicit hand-off. A layer would help sites B/C/D but not the
hard site A/ingest; this RFC does the explicit extraction uniformly and leaves a
middleware refactor as a follow-up once the pattern is proven.

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

- [ ] Carry-channel for the ingest parent context: explicit `ingest_bound`
      parameter vs. a request-extension (the auth layer already inserts into
      `request.extensions_mut()`; the extracted `Context` could ride the same
      channel with no signature change). §3.4 prefers the extension; confirm on
      review.
- [ ] Should site A extract from the tower auth layer's raw `http::HeaderMap`
      (one `HeaderExtractor` for all sites, no tonic-metadata adapter) or from
      tonic's `MetadataMap` in `export`? The former is preferred (§3.4).
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
  [`OpenTelemetrySpanExt::set_parent`](https://docs.rs/tracing-opentelemetry/0.33.0/tracing_opentelemetry/trait.OpenTelemetrySpanExt.html).
- Pinned: `opentelemetry` 0.32.0, `opentelemetry_sdk` 0.32.1,
  `tracing-opentelemetry` 0.33.0.
