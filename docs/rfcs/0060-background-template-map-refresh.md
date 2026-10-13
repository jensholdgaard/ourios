---
rfc: 0060
title: Background template-map refresh
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-10-13
supersedes: —
superseded-by: —
---

# RFC 0060 — Background template-map refresh

> **Status: `specified` (2026-10-13, #990, maintainer-approved: the
> RFC 0033 §4 reversal, the two telemetry names and the §7 decisions
> as recorded below; checked against the OpenTelemetry docs).** This RFC adds a querier
> background task on top of RFC 0033's acquisition path. It changes no
> artifact format, fold rule or publish rule of RFC 0033 (`accepted`), so
> it is a new RFC rather than an amendment. It **revisits** an
> alternative RFC 0033 §4 rejected, "a background refresher", on new
> evidence (§2.1). If this RFC is accepted, RFC 0033 §4 gets a dated
> pointer here.

## 1. Summary

The querier refreshes each tenant's template map in the background:
- once at start;
- then on a fixed interval;
- one tenant at a time;
- through the same per-tenant single-flight a query acquires through
  (`map_flight`).

The full folds that RFC 0033 still runs are paid by the background task
in most cases, not by a user query. These are the `no_fold_state`
bootstrap, the depth cap, and the `files_removed` / `out_of_order`
fallbacks. A query that arrives while the task is folding its tenant
joins that fold instead of starting a second one. The task never fails
the process, is bounded to one acquisition at a time, and is off when
configured off.

## 2. Motivation

RFC 0033's guarded incremental fold (§3.3 amendment 2026-10-12, #897)
made a stale refresh cheap. On the production node, over the first 24 h
of v0.13.0:
- 24 stale derives were incremental, p95 0.45 s;
- 2 derives were full folds: the one-time `no_fold_state` bootstrap, one
  per tenant;
- one of those took 49 s and 5,726 GETs, inside a user's
  `POST /v1/query`.

A querier restart is no longer cold: the artifact lives in the store, so
the first query after a restart is a hit or an incremental refresh.

Full folds still happen, and each lands on whichever query asks first:

- **Bootstrap.** The first acquisition after an artifact-format change
  (`no_fold_state`, `unknown_version`) or after a torn artifact.
- **Depth cap.** Every `INCREMENTAL_DEPTH_CAP` (256) incremental folds.
  That is about every ten days per tenant at the node's current rate,
  and sooner on a busier tenant.
- **Fallbacks.** `files_removed` (audit objects removed, e.g. by
  retention) and `out_of_order`.

For a short-timeout client a 50 s fold is an outage: the client gives
up, the fold completes for the next query (`map_flight`), and the user
saw an error. This was the failure mode in #853. Moving that cost to a
task nobody waits on is cheaper than making every client patient.

### 2.1 Revisiting RFC 0033 §4

RFC 0033 §4 rejected a background refresher. Its reasoning was that the
refresher adds a scheduling component and a staleness-window policy
"for something the write-through gets for free at the moment of
demand". Two things have changed since:

- **The moment of demand is not free.** A full fold costs a user up to
  50 s, and since #897 every remaining full fold is one that some query
  pays for (above).
- **No staleness window is introduced.** The task only pays early. It
  calls the unchanged acquisition, whose frontier check still decides
  freshness for every query, so the RFC 0033 correctness argument is
  untouched. A query never reads a map older than its own listing.

What remains is the scheduling component, the cost this RFC proposes to
pay.

**Why this layer.** The querier already owns acquisition, single-flight
and publish. A separate process or the ingester would need its own copy
of all three and would compete with the querier's CAS publish. RFC 0033
§4 rejected ingester-side write-through for the same reasons (fold code
on the ingest path, publishing for tenants nobody queries).

## 3. Proposed design

### 3.1 The task

When the querier role starts, it spawns one tokio task, `template-map
refresh`. Each **pass**:

1. lists the tenants that have an audit stream, one delimited `LIST` of
   `audit/` taking the `tenant_id=` common prefixes, decoded with the
   partition key's tenant encoding (the inverse of
   `percent_encode_tenant`);
2. for each tenant, in listing order, calls the same
   `TemplateMapFlights::acquire_fetched` a query uses, and waits for it
   to finish before starting the next tenant.

The first pass starts immediately at role start. Each later pass starts
`interval` after the previous pass **finished**, so passes never
overlap. Nothing new is decided: a background acquisition takes the
usual path (a hit, an incremental fold, or a full fold), publishes
through the usual write-through, and is subject to every RFC 0033 rule.

### 3.2 Interaction with queries

The background acquisition is a flight like any other:
- a query for the same tenant and frontier joins it and records
  `joined`;
- a query at a different frontier waits for it and lists again
  (`map_flight` semantics).

So a query never runs a fold concurrently with the background task for
its tenant. A query that arrives during a 50 s background fold still
waits for the rest of it. The task narrows that window rather than
closing it: it only removes the case where the query is the first to
ask. §7 lists pre-emptive full folds as an open question.

### 3.3 Bounds

- **Concurrency.** One background acquisition at a time, for the whole
  process. Its peak memory is one fold's (the RFC 0058 / #853 bound),
  never a sum over tenants.
- **Shutdown.** The task stops at the next tenant boundary once
  shutdown begins. An acquisition already on the blocking pool runs to
  completion, as a dropped query's does (`map_flight`); shutdown does
  not wait for it.
- **Failure.** A failed acquisition is recorded (§3.5), and the pass
  moves on to the next tenant. A failed tenant listing ends the pass;
  the next pass retries. Neither failure stops the task or the process.
- **Cost on an idle tenant.** A tenant with nothing new costs one audit
  `LIST` plus one artifact `GET` per pass (a hit). With the default
  interval that is negligible beside the ingest path's own requests. It
  is still paid per tenant, hence §7's question about large tenant
  counts.

### 3.4 Configuration

These keys sit under `querier.template_map.refresh.*` (RFC 0020;
`${env:…}` substitution as usual):

| key        | type     | default | meaning                                      |
|:-----------|:---------|:--------|:---------------------------------------------|
| `enabled`  | bool     | `true`  | spawn the task                               |
| `interval` | duration | `5m`    | idle time between passes; must be ≥ `10s`    |

`enabled: false` restores today's behaviour exactly. An `interval`
below the floor is a startup configuration error naming the key, not a
silent clamp, as RFC 0020 validates every other resolved value.

### 3.5 Observability (CLAUDE.md §6.3)

- **Span.** Each pass is one root `INTERNAL` span, `refresh
  template_map`. It has no remote parent or child, so `INTERNAL` is the
  kind, and the name follows OTel's `{action} {target}` pattern beside
  `load template_map`. Each tenant's acquisition is the existing
  `load template_map` phase span, carrying `ourios.tenant`, with its
  CLIENT spans beneath it, nested under the pass as it nests under
  `POST /v1/query` today (RFC 0038 §3.7.2). Under the default
  `parentbased_always_on` sampler these roots are always sampled.
- **Errors** (OTel *Recording errors*). A tenant's failed acquisition
  sets error status and `error.type` on that tenant's
  `load template_map` span. The pass handled it and continued, so the
  pass span and the pass sample carry no error for it. Only a pass
  that ends early (a failed tenant listing) sets `error.type`, on both
  the pass span and its `refresh.duration` sample, with the same value.
  So the span and its duration metric describe the same operation, as
  OTel recommends.
- **Metrics.** The RFC 0033 lookup and derive instruments gain one
  attribute, `ourios.template_map.acquire.trigger` = `query` |
  `refresh`. This splits the existing series so a dashboard can tell a
  fold a user paid for from one the task paid for. The criterion of
  this RFC is that `query`-triggered full folds approach zero.
- **Pass.** One histogram, `ourios.template_map.refresh.duration` (s),
  one sample per pass, named per OTel's `{operation}.duration` rule.
  Tenants and tenant counts are not attributes: OTel's cardinality
  guidance keeps ID-like values off metrics, and they are on the spans.

`ourios.template_map.acquire.trigger` follows the precedent of the
upstream `faas.trigger` ("type of the trigger which caused this
invocation"). Its own values (`query`, `refresh`) are more precise here
than `faas.trigger`'s `http` / `timer`: a query can arrive over HTTP or
MCP, and the start pass is not a timer. With two values it at most
doubles the existing series.

`ourios.template_map.acquire.trigger` and
`ourios.template_map.refresh.duration` are new names. They are
registered in ourios-semconv under the purpose-check and
maintainer-approval rule before implementation, after an OTel
naming-guidance check.

### 3.6 Multi-tenancy and replicas (CLAUDE.md §3.7)

Each acquisition is per tenant, as today. Several querier replicas each
run their own task. Their publishes are CAS writes, so the worst case
is duplicated fold work, never a torn or regressed artifact (RFC 0033
§3.4). Coordinating replicas is out of scope (§7).

```mermaid
sequenceDiagram
    participant T as refresh task
    participant F as map_flight
    participant Q as user query
    T->>F: acquire(tenant A)
    Note over F: full fold (bootstrap / depth cap)
    Q->>F: acquire(tenant A), same frontier
    F-->>Q: joined (waits for the fold)
    F-->>T: map, published
    T->>T: next tenant
```

## 4. Alternatives considered

**Warm only at start.** Same task, one pass, no interval. Simpler, but
since #897 a restart is not the cold case: the artifact survives in the
store. The depth-cap and fallback full folds still land on queries.
Rejected as covering only the case that has already been fixed.

**Pre-emptive full fold before the depth cap.** The task would fold in
full when a tenant's depth nears the cap, so the cap is never reached
on the query path. This needs a new rule in RFC 0033's derive policy
(fold in full below the cap, but only off the query path). Deferred to
§7 so the task lands first and Dash0 can show whether cap folds still
reach queries.

**Serve stale while revalidating.** Answer a query from the stale
artifact and fold in the background. This breaks RFC 0033's guarantee
that a query renders against a map at its own frontier: rows written
after the artifact would render against missing or old bindings
(CLAUDE.md §3.3). Rejected.

**Raise client timeouts.** Not ours to control, and it hides a 50 s fold
instead of removing it from the request.

**Fold in the ingester.** See §2: the ingester does not own the read
path, and a second publisher contends for the CAS.

## 5. Acceptance criteria

> **Scenario RFC0060.1 — A start pass absorbs the bootstrap**
> - **Given** a store whose tenant has an artifact without `fold_state`
> - **When** the querier starts with refresh enabled, and a query
>   arrives after the first pass has finished
> - **Then** the full fold ran with `acquire.trigger = refresh`
> - **And** the query's lookup is a `hit` with
>   `acquire.trigger = query`, and runs no fold.

> **Scenario RFC0060.2 — A query during a background fold joins it**
> - **Given** the task holding a tenant's flight on a gated fold
> - **When** a query for that tenant at the same frontier arrives
> - **Then** exactly one fold runs, and the query records `joined` and
>   receives the task's map.

> **Scenario RFC0060.3 — One background acquisition at a time**
> - **Given** three tenants, each with a gated fold
> - **When** a pass runs
> - **Then** at most one background acquisition is in flight at any
>   instant, and the tenants are visited in listing order.

> **Scenario RFC0060.4 — Failure is contained**
> - **Given** two tenants, the first with an unreadable audit object
> - **When** a pass runs
> - **Then** the first tenant's `load template_map` span carries error
>   status and `error.type`
> - **And** the second tenant is still acquired, under the same pass span
> - **And** the pass span and its `refresh.duration` sample carry no
>   `error.type`, and the task runs its next pass
> - **And** a pass whose tenant listing fails ends early with the same
>   `error.type` on its span and its sample.

> **Scenario RFC0060.5 — Disabled means today's behaviour**
> - **Given** `querier.template_map.refresh.enabled: false`
> - **When** the querier starts and idles past `interval`
> - **Then** no acquisition runs, and no `refresh template_map` span or
>   refresh sample is emitted.

> **Scenario RFC0060.6 — Configuration is validated**
> - **Given** `interval: 1s`, or a non-duration value
> - **When** the configuration is resolved
> - **Then** startup fails with an error naming the key.

> **Scenario RFC0060.7 — Shutdown is prompt**
> - **Given** a pass between tenants, or waiting on its interval
> - **When** the querier shuts down
> - **Then** the task exits without starting another acquisition.

> **Scenario RFC0060.8 — Telemetry is registry-conformant**
> - **Given** RFC0060.1's run
> - **When** its spans and metrics are checked with weaver live-check
> - **Then** `refresh template_map`, `acquire.trigger` and
>   `refresh.duration` conform to the ourios-semconv registry.

> **Scenario RFC0060.9 — Query-paid full folds drop** (Dash0, after
> deploy)
> - **Given** the node on the release that ships this RFC, over 7 days
> - **Then** `derive.duration{mode=full, acquire.trigger=query}` counts
>   only folds whose query arrived during the background pass's own
>   fold, and is below the previous release's full-fold count.

## 6. Testing strategy

- RFC0060.1–.4 and .7 are integration tests in `ourios-querier` over a
  local store, using the `map_flight` hooks (`before_acquire` gating,
  acquisition and join counters) through the `testing` feature.
- RFC0060.5–.6 are `ourios-server` config tests (RFC 0020 style) plus
  one served-role test.
- RFC0060.8 uses the weaver live-check harness (`live_check`).
- RFC0060.9 is a Dash0 before/after measurement recorded on the release
  (the RFC0033.14 precedent), not a CI test.

No `proptest` or corpus test applies: the task adds scheduling, not
fold semantics, and the fold's own property tests (RFC0033.10–.13)
already cover what it calls.

## 7. Open questions

Decided for this RFC (maintainer, 2026-10-13); each is revisited only on
the Dash0 evidence named:

- [x] **Pre-emptive full fold near `INCREMENTAL_DEPTH_CAP`: deferred.**
      At about one cap fold per tenant per ten days, a 5-minute pass
      almost always reaches it first. Reopen, as an RFC 0033 derive-policy
      amendment, if `derive.duration{mode=full, acquire.trigger=query}`
      stays above zero after RFC0060.9.
- [x] **Large tenant counts: not in v1.** Every tenant is visited each
      pass. The preferred later shape is to refresh only tenants queried
      within a window. Reopen when `refresh.duration` approaches
      `interval`.
- [x] **Replicas: accept duplicated folds.** CAS publish keeps them
      correct (§3.6). A CAS lease object for a single refresher is the
      later shape, when replicas are deployed.
- [x] **Default `interval`: `5m`.** Revisit with publish and fold rates
      after release.

## 8. References

- RFC 0033 (cached template map): §3.3 freshness and the incremental
  fold, §3.4 CAS publish, §3.5 write-through, §3.7 instruments, §4 (the
  background refresher and ingester write-through alternatives).
- RFC 0038 §3.7 (query-phase spans), RFC 0020 (configuration), RFC 0005
  (partition layout), RFC 0058 (query resource limits).
- #853 (cold map / RSS), #897 (incremental fold), #934 (resumable
  bootstrap), #990 (this RFC's issue).
- CLAUDE.md §3.3, §3.7, §6.3.
