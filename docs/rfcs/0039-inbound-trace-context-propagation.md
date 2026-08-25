---
rfc: 0039
title: Inbound trace-context propagation — SERVER spans continue the caller's trace
status: accepted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-25
supersedes: —
superseded-by: —
---

# RFC 0039 — Inbound trace-context propagation

> **Status: `accepted` (2026-08-25, maintainer sign-off).** Terminal —
> the completed-backlog batch flip. Where a thesis-gate applies it
> stands passing in `docs/benchmarks.md` §7; elsewhere `validated` is
> vacuous for this surface (RFC 0008 precedent).
>
> **Status: `green`** — all six §5 scenarios have asserting tests, landed over
> four slices: the query arm and the propagator install (#627), the ingest spawn
> boundary on both OTLP transports (#628), the `/mcp` SERVER span with the tool
> spans nesting locally beneath it (#629), and the sampling regime (this slice).
> Two §3 design decisions were withdrawn during implementation and amended in
> place rather than silently followed — gRPC extraction moved out of the tower
> auth layer into the receiving handler (§3.3/§3.4), and the MCP arm gained a
> span, which §2's original "no new spans" promise had ruled out.

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
the traces pillar (hence an RFC), and for the ingest and query arms it adds no
new spans, no new attributes of consequence, and no on-disk change — it only
sets the *parent* of spans that already exist.

> **Amendment (slice 3).** The paragraph above originally promised "no new
> spans" for *every* arm. That does not survive contact with the MCP arm: the
> only spans `/mcp` had were the `execute_tool <tool>` spans, which are
> `INTERNAL` — a kind the [tracing API][spankind] defines as an operation "as
> opposed to an operations \[sic] with **remote parents** or children" (the
> grammar slip is upstream's; quoted verbatim). Parenting them straight to
> the caller, as §7 originally deferred, would contradict their own kind, and MCP
> `tools/call` is JSON-RPC over HTTP, where both conventions require the inbound
> server span's kind to be `SERVER`. So the MCP arm adds **one** span — a
> `SERVER` span for the inbound `/mcp` request — with the tool spans nesting
> locally beneath it. No on-disk change; §3.3 bullet D and §3.5 carry the
> details, and this closes §7's second open question.

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

**Four** ingress categories open a span on the request path — seven
span-producing functions in all, since the MCP category is three tool functions
plus the `/mcp` server span slice 3 adds (§3.3 bullet D). The count is stated so
test coverage (RFC0039.1/.3/.6) omits no site. The carrier — where the incoming
`traceparent` lives — is not always co-located with the span:

Sites are named by file and function, deliberately without line numbers — those
rot on every touch of the surrounding code and have already been corrected twice
in review.

| # | Span | Site | Carrier & where it is reachable |
|---|---|---|---|
| A | `ingest logs` (gRPC) | span in `IngestPipeline::ingest_bound` (`pipeline.rs`); entry `LogsReceiver::export` (`grpc.rs`) | tonic `MetadataMap`, on the request in `export` itself (§3.4) |
| B | `ingest logs` (HTTP) | span in `ingest_bound`; entry `handle_logs` (`http.rs`) | axum `HeaderMap`, a `handle_logs` extractor |
| C | `POST /v1/query` | `handle_query_traced`, behind the `handle_query` wrapper (`querier.rs`) | axum `HeaderMap`, a `handle_query` extractor |
| D1 | `{method} /mcp` (`SERVER`) | `mcp_server_span_traced`, behind the `mcp_server_span` layer (`mcp.rs`) | axum `HeaderMap`, in the layer — the remote carrier for the whole MCP category |
| D2 | `execute_tool <tool>` (×3, `INTERNAL`) | the three `_traced` fns (`mcp.rs`), each via a thin `#[tool]` delegate | *not* the remote carrier: `McpTraceContext` in `parts.extensions`, the D1 span's own context (§3.3 bullet D) |

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

- **Query (C):** an un-instrumented `handle_query` wrapper extracts `cx` from the
  request `HeaderMap` and awaits the instrumented `handle_query_traced` under it
  — `handle_query_traced(..).with_context(cx).await`. No tower layer: an earlier
  revision of this bullet proposed a shared `PropagationLayer`, but neither ingest
  arm can use one (their span is born past a `tokio::spawn`, below), which would
  leave the query arm as its sole beneficiary — see §4.
- **gRPC ingest (A):** extraction happens in `LogsService::export` itself, from
  the request's tonic `MetadataMap` (§3.4) — *not* in the tower auth layer, as
  an earlier revision of this section proposed. Extraction belongs with the
  handler that receives the call, which is both what OpenTelemetry prescribes
  for a service receiving upstream calls and what makes it testable: the
  RFC0039.3 harness drives `export` directly (no tower stack), so a
  layer-extracted context would leave the extraction itself uncovered. See the
  amendment note below.
- **The `tokio::spawn` boundary (A/B):** the `ingest logs` span is born inside
  `ingest_bound`, *after* the spawn in `export` / `handle_logs`, which
  ambient context does not cross. So the handler extracts `cx` from its own
  carrier before the spawn, moves it into the spawned closure, and attaches it
  to the **whole** spawned block — `async move { ingest_bound(...).await
  }.with_context(cx)` — rather than to `ingest_bound`'s future alone. Wrapping
  the block is deliberate: it holds whether `#[tracing::instrument]` mints its
  span at call time or on first poll, so the span cannot be created outside
  `cx`. **No `ingest_bound` signature change, no `set_parent`.**

> **Amendment (slice 2).** §3.3's gRPC bullet and §3.4 originally routed
> extraction through the tower auth layer into a request extension. That was
> withdrawn during implementation for two reasons: it contradicted §6, whose
> RFC0039.3 test calls `export` directly and so would have exercised only the
> spawn hand-off and never the extraction; and it put a propagation concern
> inside a layer named for authentication. The OTel guidance is explicit that a
> service receiving upstream calls extracts in the receiving handler ("the one
> context on the wire becomes the parent of the new span the library creates"),
> and the OTel Demo's own C++ gRPC service does exactly this with a
> `GrpcServerCarrier` over `client_metadata()`. The cost of the correction is
> the ~15-line `MetadataExtractor` that §3.4 had hoped to avoid.
- **MCP (D):** two spans, because one cannot honestly do both jobs. A
  `mcp_server_span` layer, outermost on the `/mcp` router so it also covers an
  auth rejection, extracts the caller's `cx` from the request headers and opens a
  **`SERVER`** span under it (D1) — named `{method} /mcp` via `otel.name`, since
  `/mcp` serves POST, GET and DELETE and the macro's static `name` cannot vary.
  That span then publishes **its own** context as `McpTraceContext` in the
  request extensions, and each un-instrumented `#[tool]` delegate reads it back
  and runs `self.<tool>_traced(...).with_context(parent).await` (D2).

  Two details are load-bearing. The delegates attach the **server span's**
  context, *not* the extracted remote one: that is what makes the tool spans
  children of a local span, keeping them legitimately `INTERNAL`. And the channel
  is the request extensions rather than ambient context, because rmcp dispatches
  the tool on a `tokio::spawn`ed task — verified, not assumed: with the hand-off
  removed the tool span lands in a freshly minted trace. The extensions are known
  to survive that hop because `AuthBinding` already travels the same route.

This is the same discipline RFC 0038.3 uses to carry work across `tokio::spawn`,
applied here to the parent context — and it is one uniform contract, resolving
the earlier draft's split between an explicit parameter and a request extension.

### 3.4 The extractor shim

`opentelemetry::propagation::Extractor` is a two-method trait (`get`, `keys`).
**Two** adapters are needed, both in `receiver/propagation.rs`:

- `HeaderExtractor<'a>(&'a http::HeaderMap)` — the axum-side ingresses (OTLP/HTTP,
  the query API, the MCP tools), whose carrier is an `http::HeaderMap`.
- `MetadataExtractor<'a>(&'a tonic::metadata::MetadataMap)` — the OTLP/gRPC
  ingress. gRPC metadata *is* HTTP/2 headers, but tonic models it as
  `HeaderMap<MetadataValue>`, not `http::HeaderMap`, and exposes no cheap
  `&HeaderMap` view, so it needs its own carrier. `keys()` offers only the ascii
  half: a binary (`-bin`) key can never resolve as a text-map entry.

Both resolve through the propagator installed in §3.1, e.g.
`global::get_text_map_propagator(|p| p.extract(&HeaderExtractor(headers)))`.
`opentelemetry-http` ships an equivalent `HeaderExtractor`; the local pair avoids
that dependency and keeps both carriers described in one place.

### 3.5 Dependency promotion (the one production-surface change)

The ingress code needs `opentelemetry` types in production
(`Context`, `propagation::Extractor`, `trace::FutureExt::with_context`,
`global::get_text_map_propagator`), but `opentelemetry` is a production
dependency of `ourios-ingester`/`ourios-server` today only with the
**`metrics`** feature. This RFC adds the **`trace`** feature to that existing
dependency in both crates. The propagator *install*
(`opentelemetry_sdk::propagation::TraceContextPropagator`, §3.1) stays in
`ourios-telemetry`, which already depends on `opentelemetry_sdk`.

For the ingest and query arms that is the whole production-surface cost — one
added feature flag on a crate already depended on — because the parenting rides
the current-context bridge the `tracing-opentelemetry` layer already provides,
with no `set_parent` call.

> **Amendment (slice 3).** This section originally concluded that
> "`tracing-opentelemetry` is not needed in the ingress crates at all". True of
> the ingest and query arms; not of MCP. The `/mcp` SERVER span must hand *its
> own* context to the tool handlers across rmcp's dispatch spawn (§3.3 bullet D),
> and reading a tracing span's OTel context requires
> `OpenTelemetrySpanExt::context()`. So `ourios-server` gains
> `tracing-opentelemetry` as a production dependency — at the same 0.33 pin
> `ourios-telemetry` already uses, so no new version enters the tree. The
> alternative was building the `/mcp` span through the raw OTel API, which hands
> back a `Context` directly and needs no new dependency, but would have made it
> the one Ourios span that is not a `tracing` span (no log correlation, and a
> pattern break — `CLAUDE.md` §5.4).

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

**A shared `PropagationLayer` instead of per-handler extraction.** Attractive on
paper — one layer for every axum-side ingress — but it buys less than it looks.
Neither ingest arm can use it: their span is born past a `tokio::spawn`, so they
need the explicit `with_context` hand-off regardless (§3.3), and a layer that
extracted for them would only duplicate what the handler must do anyway. That
leaves the query arm as the sole beneficiary of a whole tower layer, for the two
lines it already spends extracting directly. Per-handler extraction also keeps
each site's carrier visible at the site, which is where OTel puts it.

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

> **Scenario RFC0039.6 — the MCP tool call joins the caller's trace, correctly
> shaped.**
> **Given** an MCP `tools/call` over `/mcp` carrying `traceparent` for trace `T`
> span `S`,
> **When** the tool executes,
> **Then** a `{method} /mcp` span of kind **`SERVER`** resolves to
> `trace_id == T` with parent span id `== S`; **and** the `execute_tool <tool>`
> span resolves to `trace_id == T` with its parent being that **local** server
> span — retaining kind `INTERNAL`, which the spec reserves for operations
> without remote parents. So an agent driving Ourios's tools sees the whole
> exchange inside its own trace, without either span misrepresenting its kind.

## 6. Testing strategy

Mapped to `CLAUDE.md` §6.2:

- **RFC0039.1 / .2 / .5 / .6** — integration tests in `ourios-server` /
  `ourios-ingester` using the RFC 0038 scoped-`InMemorySpanExporter` harness:
  drive `handle_query`, `handle_logs`, and (global-tracer binary, per RFC0038.1
  MCP arm) an MCP `tools/call`, each with an injected `traceparent` header, then
  assert `SpanData.span_context.trace_id()` / `.parent_span_id()`. The
  no-context and malformed-context cases assert a fresh, valid root and no error.
- **RFC0039.3** — `rfc0039_3_ingest_propagation.rs` (its own global-tracer
  binary, per RFC0028.2): call `LogsReceiver::export` and the HTTP router
  directly with a `traceparent`, and assert the `ingest logs` + `commit wal`
  spans carry the injected `trace_id`. It gets a binary of its own rather than
  extending `rfc0038_3_spawn_boundary.rs` — a process holds one tracer install,
  and that file owns the no-inbound-context case, so extending it would have
  meant editing a passing test's assertions (`CLAUDE.md` §6.2).
- **RFC0039.6** — `rfc0039_6_mcp_propagation.rs` (likewise its own binary):
  handshake **without** a `traceparent`, then one `tools/call` **with** one, so a
  single run covers both the propagated and the root case. Asserts the `SERVER`
  kind and remote parent of the `/mcp` span, and that the `execute_tool` span is
  `INTERNAL` with that server span as its *local* parent.
- **RFC0039.4** — a sampler test: with `OTEL_TRACES_SAMPLER=parentbased_always_on`
  (default), inject `-00` vs `-01` traceparents and assert exported-span presence.
  The parent-based resolution itself is upstream SDK behaviour; the test covers
  Ourios's wiring (that the extracted context reaches the sampler).
- The extractor shims get a unit test (round-trip a `traceparent` through a
  `HeaderMap` and back to a `SpanContext`).

## 7. Open questions

- [x] **Resolved (slice 2).** `FutureExt::with_context` does re-attach the
      extracted context inside the spawned `ingest_bound` task, on both
      transports — `rfc0039_3_ingest_propagation.rs` passes, and fails with
      each arm's span in a freshly minted trace when the two handler changes are
      reverted. Site A's carrier is the tonic `MetadataMap`, read in `export`
      (§3.4 amendment); no request extension and no `ingest_bound` signature
      change are involved.
- [x] **Resolved (slice 3): yes, the dedicated `/mcp` SERVER span is warranted —
      and required.** This question framed an INTERNAL span with a remote parent
      as "valid but slightly unusual". It is not valid: the [tracing
      API][spankind] defines `INTERNAL` as an operation "as opposed to an
      operations \[sic] with **remote parents** or children", and the concepts
      doc as one that "does not cross a process boundary" (both quoted verbatim —
      the grammar slip is upstream's). Meanwhile `tools/call` is JSON-RPC over
      HTTP, and both
      the RPC and HTTP conventions state the inbound server span's kind **MUST**
      be `SERVER`. So `/mcp` gains a SERVER span (§3.3 bullet D), the tool spans
      nest locally under it, and both kinds stay honest. Consequences recorded as
      amendments in §2 (the "no new spans" promise) and §3.5 (the
      `tracing-opentelemetry` dependency).
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
- OpenTelemetry span kinds — [`SpanKind`][spankind] (the normative definition of
  `INTERNAL` that §7's second question turned on), and the
  [RPC](https://opentelemetry.io/docs/specs/semconv/rpc/rpc-spans/#rpc-server-span)
  / [HTTP](https://opentelemetry.io/docs/specs/semconv/http/http-spans/#http-server-span)
  server-span conventions, both of which state the kind **MUST** be `SERVER`.
- Pinned: `opentelemetry` 0.32.0, `opentelemetry_sdk` 0.32.1,
  `tracing-opentelemetry` 0.33.0.

[spankind]: https://opentelemetry.io/docs/specs/otel/trace/api/#spankind
