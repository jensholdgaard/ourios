---
rfc: 0016
title: Query-serving endpoint — the HTTP query API over the logs DSL
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-06-19
supersedes: —
superseded-by: —
---

# RFC 0016 — Query-serving endpoint: the HTTP query API over the logs DSL

## 1. Summary

Wire the validated query engine (RFC 0007) into `ourios-server` as a
network-reachable **querier role**: an HTTP endpoint that accepts a logs
DSL query (RFC 0002), executes it through `Querier::run_query` /
`run_drift`, and returns the matching log rows plus pruning statistics as
JSON. It mirrors the receiver role's `serve(config) -> Handle` topology
(RFC 0003) — env-gated, graceful-shutdown, its own listen address. The
DSL is the public contract; DataFusion never surfaces (H6). This
closes the core product loop — **ingest → store → query** — and is the
keystone the deferred Perses datasource plugin waits on. Returning actual
log rows depends on the RFC 0007 §4.1 typed-row execution payload, whose
sequencing relative to this RFC is the central open question (§3.1, §7).

## 2. Motivation

The thesis is proven: B1/B2 (predicate pushdown + the `template_id`
index) pass authoritatively on real corpora (`benchmarks.md` §9.4), the
miner and storage are `accepted`/`green`, and the full ingest path is
live in the server binary. But the running binary **cannot answer a
query over the network**. `ourios-querier` (RFC 0007 `validated`, RFC
0002 DSL `green`) is a working library that `ourios-server` does not even
depend on; `main.rs` records the querier role as a *"follow-up"* and RFC
0007 §8 explicitly defers *"the querier role of the server binary (RFC
0003 sibling)"* to a sibling RFC. This is that RFC.

Everything Ourios proves is query-side — the value of Parquet pruning and
the template index is only realised when an operator can run a query. A
backend you can ingest into but cannot query is not a shippable product;
packaging it (container image, Helm chart, signed release) should wrap a
*complete* loop, not a write-only collector. The maintainer's
"prove-the-thesis-before-the-DSL-contract" sequencing (the engine drove
B1/B2 through a deliberately minimal `QueryRequest`) is now discharged —
the thesis holds, so the DSL can become the real, public query contract.

## 3. Proposed design

### 3.1 The pivotal dependency — typed-row payload

`Querier::run_query` today returns `QueryResult { rows: u64, stats:
QueryStats }`, where `rows` is a **count** and `stats` reports
`row_groups_{scanned,pruned}` + `bytes_read`. RFC 0007 §4.1 defers the
**typed-row payload** ("lands with the execution slice"). A serving
endpoint that returns only counts + pruning stats — never the actual log
lines — has little operator value.

This RFC therefore takes returning **log rows** as a requirement, which
makes the RFC 0007 §4.1 typed-row execution slice a **prerequisite**. Two
ways to sequence it (resolved in review, §7):

- **(a) Prerequisite RFC 0007 amendment** — land the typed-row payload as
  an engine-layer slice under RFC 0007 first; RFC 0016 is then a thin
  transport over it. *(Recommended — keeps the engine/transport split
  clean and matches the existing RFC ownership.)*
- **(b) Fold into RFC 0016** — this RFC carries both the row payload and
  the transport. Larger, blurs the RFC 0007 boundary.

Either way the endpoint's contract is the same; §5 is written assuming
rows are available.

### 3.2 Transport — HTTP/JSON, one querier role

A single querier role, mirroring the receiver:

- `ourios_server::querier::serve(QuerierConfig) -> Result<QuerierHandle,
  String>`, returning a handle that exposes the bound address and a
  `shutdown()` future, on the same `watch::channel(())` graceful-shutdown
  topology as `receiver::serve` (RFC 0003).
- **HTTP only** for v1 (axum, the receiver's HTTP stack). gRPC is
  deferred (§4): operators and the future Perses plugin query over HTTP;
  the OTLP gRPC path is an *ingest* concern, not a query one.
- Env-gated exactly like the receiver: `OURIOS_QUERIER_ENABLED`
  (`1`/`true`/`yes`), `OURIOS_QUERIER_HTTP_ADDR` (default `0.0.0.0:4319`),
  reusing `OURIOS_BUCKET_ROOT` for the store. The two roles compose: a
  binary may run receiver-only, querier-only, or both.

### 3.3 Request

`POST /v1/query`, body the DSL query. Both DSL front-ends already exist
(`dsl::parse_statement` for the text grammar, `parse_structured_statement`
for the JSON form), so the endpoint accepts either by `Content-Type`:

- `application/json` → `{ "query": "<dsl text>" }` *or* the structured
  JSON IR (per RFC 0002's structured surface);
- `text/plain` → the raw DSL statement.

The parsed `Statement` dispatches: `Logs(Query)` → `run_query`,
`Drift(DriftQuery)` → `run_drift` (RFC 0010). The server supplies `now`
(wall clock) and the configured default time window to the executor, as
the DSL compiler expects.

**Tenancy.** Tenant is required (the engine already enforces it
structurally — `QueryError::TenantRequired`, partition-rooted scan). v1
takes it from a required `X-Ourios-Tenant` header (kept out of the query
body so the DSL grammar stays tenant-agnostic). Authn/z beyond
tenant-scoping is out of scope for v1 (§7).

### 3.4 Response

`200` with `application/json`: the matching rows (shape from the §4.1
payload) plus the pruning stats (`row_groups_scanned`,
`row_groups_pruned`, `bytes_read`) so callers see the pillar-1 win
directly. Result-encoding details (a JSON array vs NDJSON streaming for
large results, the default `limit` and its hard cap) are §7 open
questions. Drift queries return the RFC 0010 `DriftResult` shape.

### 3.5 Error model (H6)

All errors are Ourios-owned; **no DataFusion type, SQL string, or plan
ever appears** in a response. Mapping:

- DSL parse/validation (`DslError`, `QueryError::Compile`) → `400` with a
  structured `{ "error": { "kind": ..., "message": ... } }`.
- `QueryError::TenantRequired` / missing header → `400`.
- Execution failure → `500`, message scrubbed of engine internals.

### 3.6 Observability

The querier role emits metrics through the OTel meter surface (RFC 0001
§6.8 model — per the established "OTel meters, not the Prometheus client"
direction): query count, latency histogram, and the pruning ratio
(`row_groups_pruned / scanned`) so the thesis win is observable in
production. New metric/attribute names go through `semconv/registry/` +
weaver (no hand-written flat names).

## 4. Alternatives considered

**SQL passthrough.** Expose DataFusion SQL directly. Rejected by hazard
#6 and RFC 0007's "Not a SQL endpoint" line — leaking the engine's SQL
surface couples the public API to an implementation detail and forfeits
the DSL's template-aware primitives (`resolves_to`, `lossy`, drift).

**gRPC (instead of / in addition to HTTP) for v1.** A query gRPC service
is plausible, but adds a second transport and a `.proto` contract for no
v1 consumer — operators use HTTP and the Perses plugin will too. Deferred
until a concrete gRPC consumer exists.

**Counts-and-stats only for v1 (no row payload).** Ship the endpoint over
the engine *exactly as it is today* (return `rows: u64` + stats), defer
log-line retrieval. Rejected as the primary plan: an endpoint that can't
return logs isn't a usable query API and wouldn't justify the packaging
work that follows. Recorded because it is the minimal fallback if the
§4.1 payload slips.

**Serve queries from the receiver process / always-on.** Folding the
query listener into the receiver role couples ingest and read scaling and
removes the querier-only deployment topology. A separate env-gated role
(matching CLAUDE.md's two-role binary) keeps them independent.

**Skip the role gate (always serve).** Rejected — the binary's role model
(receiver / querier, each env-gated) is established by the receiver; a
querier-only or receiver-only deployment is a real operational shape.

## 5. Acceptance criteria

> **Scenario RFC0016.1 — querier role serves a DSL query end-to-end**
> - **Given** a populated store and `ourios-server` started with
>   `OURIOS_QUERIER_ENABLED=1` and `OURIOS_BUCKET_ROOT` set
> - **When** a client `POST`s a logs DSL statement to `/v1/query`
>   with an `X-Ourios-Tenant` header
> - **Then** the server parses it via the RFC 0002 front-end,
>   executes it through `Querier::run_query`, and returns `200`
>   with the matching rows and the pruning stats
>   (`row_groups_scanned`, `row_groups_pruned`, `bytes_read`)

> **Scenario RFC0016.2 — tenant scoping is enforced at the API**
> - **Given** two tenants with disjoint data in the store
> - **When** a query is sent with `X-Ourios-Tenant: A`
> - **Then** only tenant A's rows are ever read or returned, and a
>   request with no tenant header is rejected `400`
>   (`TenantRequired`) without scanning any data

> **Scenario RFC0016.3 — a drift query routes to the drift path**
> - **Given** an audit stream with template widening events
> - **When** a `drift from <t1> to <t2>` statement is posted
> - **Then** the endpoint dispatches the `Drift` arm to
>   `run_drift` and returns the RFC 0010 `DriftResult` shape

> **Scenario RFC0016.4 — malformed DSL is a clean 400, no engine leak**
> - **Given** the querier role running
> - **When** a syntactically invalid or uncompilable DSL statement
>   is posted
> - **Then** the response is `400` with an Ourios-owned error body,
>   and **no** DataFusion type, SQL string, or plan text appears in
>   the response (H6)

> **Scenario RFC0016.5 — role gating and graceful shutdown**
> - **Given** `OURIOS_QUERIER_ENABLED` unset
> - **When** the server starts
> - **Then** no query listener is bound; **and** when enabled and
>   then sent SIGINT/SIGTERM, the querier listener drains and the
>   process exits cleanly (mirroring the receiver handle)

> **Scenario RFC0016.6 — pruning is observable**
> - **Given** a selective query (time window or `template_id`) over a
>   multi-row-group corpus
> - **When** it runs through the endpoint
> - **Then** the response's `row_groups_pruned` is non-zero and a
>   query-latency + pruning-ratio metric is emitted via the OTel
>   meter surface

> **Scenario RFC0016.7 — receiver and querier compose in one binary**
> - **Given** both `OURIOS_RECEIVER_ENABLED` and
>   `OURIOS_QUERIER_ENABLED` set, with distinct addresses
> - **When** the server starts
> - **Then** both listeners bind and serve, sharing the one
>   `OURIOS_BUCKET_ROOT`, and shutdown drains both

## 6. Testing strategy

- **RFC0016.1 / .3** — integration tests in `ourios-server` (or
  `ourios-ingester`-style harness): start the role on `:0`, POST a DSL
  statement, assert rows + stats / drift shape. Reuses the querier's
  existing fixtures.
- **RFC0016.2** — a two-tenant fixture; assert isolation + the
  no-header-400 path. Mirrors the engine's RFC0007.5 partition-prune test
  at the API layer.
- **RFC0016.4** — table of malformed statements → 400; a grep-style
  assertion that the response body contains no `DataFusion` / `SQL` /
  `LogicalPlan` substrings (H6 guard).
- **RFC0016.5 / .7** — process-level tests: env permutations
  (neither / one / both roles), bind assertions, and a
  SIGINT-drains-cleanly check (the receiver already has this pattern).
- **RFC0016.6** — assert the pruning stat in the response and the OTel
  metric emission (testcontainers + the established meter test harness).

Each scenario id is referenced from the corresponding test so the
spec-to-test mapping is greppable (`docs/verification.md` §2).

## 7. Open questions

- [ ] **Typed-row payload sequencing (§3.1)** — land RFC 0007 §4.1 as a
  prerequisite engine slice (recommended), or fold it into this RFC?
- [ ] **Result encoding for large results** — a single JSON array, or
  NDJSON streaming once row counts are large? And the default `limit` +
  its hard cap.
- [ ] **Authn/z beyond tenant-scoping** — is v1 trusted-network only
  (tenant header, no auth), or is a token/mTLS story in scope? (Leaning
  trusted-network for v1; auth as a follow-up RFC.)
- [ ] **gRPC query service** — revisit when a concrete consumer needs it.
- [ ] **Default time window** — what window applies when a query has no
  `range(...)` stage (the compiler already has a default-window notion;
  pin the server-supplied value + make it configurable?).
- [ ] **Endpoint surface** — single `POST /v1/query` that dispatches
  Logs/Drift by statement type (proposed), or distinct paths?

## 8. References

- RFC 0002 (query DSL — the public query grammar), RFC 0007 (querier
  engine + §4.1 typed-row payload deferral + §8 serving-role open
  question), RFC 0003 (OTLP receiver — the `serve`/`Handle` + env-gating
  pattern this mirrors), RFC 0010 (drift queries), RFC 0001 §6.8 (OTel
  metric surface).
- `CLAUDE.md` §1 (not a managed service), §3.7 (multi-tenancy on every
  data path), §6.3 (observability), H6 (query DSL vs DataFusion
  SQL surface — do not leak engine specifics).
- `docs/roadmap.md` §5 (Perses datasource plugin parked behind a stable
  query API); `crates/ourios-querier/src/lib.rs` (`Querier::run_query`,
  `run_drift`, `QueryResult`); `crates/ourios-server/src/receiver.rs`
  (`serve` / `ReceiverHandle`).
