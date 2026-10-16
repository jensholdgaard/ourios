---
rfc: 0060
title: Background template-map refresh
status: red
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-10-13
supersedes: —
superseded-by: —
---

# RFC 0060 — Background template-map refresh

> **Status: `red`.** RFC0060.1–.7, .10 and .11 pass: in `ourios-querier`
> (`tests/it/rfc0060_refresh.rs`, `tests/rfc0060_refresh_observability.rs`)
> and `ourios-server` (`tests/it/rfc0060_refresh_config.rs`). RFC0060.8's
> span shape is asserted with .1; its weaver live-check of
> `acquire.trigger` is still an `#[ignore]`d `todo!` stub, the one open
> criterion before `green`. RFC0060.9 is a Dash0 measurement after
> release, not a CI test (§6).
> Specified 2026-10-13 (#990, maintainer-approved: the
> RFC 0033 §4 reversal, the two telemetry names and the §7 decisions
> as recorded below; checked against the OpenTelemetry docs). This RFC adds a querier
> background task on top of RFC 0033's acquisition path. It changes no
> artifact format, fold rule or publish rule of RFC 0033 (`accepted`), so
> it is a new RFC rather than an amendment. It **revisits** an
> alternative RFC 0033 §4 rejected, "a background refresher", on new
> evidence (§2.1). If this RFC is accepted, RFC 0033 §4 gets a dated
> pointer here.

> **Amendment 2026-10-16 — a pass skips the artifact read its listing
> proves unnecessary (#1008, `drafted`, pending maintainer approval).**
> Adds §3.7: a pass remembers, per tenant, the frontier and artifact
> `ETag` of its last verified hit. When its next listing shows the same
> frontier and the same `ETag`, it stops there instead of reading the
> artifact again. The skipped read could only have been a hit, so the
> freshness argument of §2.1 holds for every change that completed before
> the listing. A change that lands between the listing and the skipped
> read is repaired one pass late (§3.7). Queries are untouched. It
> does not shrink the per-tenant listing itself, which is most of a
> pass's requests on a tenant with a long audit history; §7 lists the
> options for that as open questions for the maintainer. In-place notes
> in §3.3, §3.5 and §6, and an amended RFC0060.10 (a hit or publish no
> longer forces an acquisition on the next pass, only a listing). New
> scenarios RFC0060.12–.16 (`drafted`); §9 records the history. The
> frontmatter status and RFC0060.1–.9 and .11 are unchanged.

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
- **Depth cap.** Every `INCREMENTAL_DEPTH_CAP` (256) *published*
  incremental steps (RFC 0033 §3.3). An abstained, failed or lost
  publish leaves the stored depth unchanged. If every one of the 24
  incremental derives a day observed above published, that is one cap
  fold per tenant per ten to eleven days. That is an upper bound on the
  frequency: each unpublished step makes the interval longer.
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
- **Cost on an idle tenant.** A tenant with a current artifact costs
  one audit `LIST` plus one artifact `GET` per pass (a hit). With the
  default interval that is negligible beside the ingest path's own
  requests. It is still paid per tenant, hence §7's question about
  large tenant counts.

  > *Amendment 2026-10-16 (#1008, `drafted`).* Measured on the
  > production node (one pass, 2026-10-16, five idle tenants): one
  > delimited `LIST` of `audit/`, then per tenant one `LIST` **per
  > 1,000 audit objects** (8 and 6 pages for the two oldest tenants, 1
  > for the rest) and one artifact `GET`. That is 23 requests a pass,
  > about 6,600 a day at the default interval, and the `LIST` pages grow
  > with each tenant's audit history. §3.7 removes the artifact `GET`
  > from a pass whose listing proves the tenant unchanged (5 of the 23).
  > The `LIST` pages stay; see §7.
- **Back-off when nothing persists.** An acquisition that ends without
  a publish and without a hit persists nothing. That covers a
  write-through abstention (RFC 0033 §3.2 size gate or ceiling, which
  may recur indefinitely), a failed publish, and a failed acquisition.
  Re-running it each pass would repeat the fold and its `GET`s every
  `interval` for no later query's benefit. So after `k` consecutive
  such acquisitions, the task skips that tenant for the next
  `min(2^k − 1, 11)` passes. The cap counts passes, not wall-clock
  time: at the default interval that is 11 × `interval` plus the
  skipped passes' own run time. A hit, a publish of its own, or a lost CAS race (someone
  else published) resets `k` to 0. The counter lives in process
  memory and resets on restart. A successful tenant listing drops the
  counters of tenants it no longer names, so the state follows the
  current tenants, not their history; a failed listing keeps them,
  since it names no current set. A query is never skipped: it acquires
  exactly as it does without the task.

### 3.4 Configuration

These keys sit under `querier.template_map.refresh.*` (RFC 0020;
`${env:…}` substitution as usual):

| key        | type     | default | meaning                                      |
|:-----------|:---------|:--------|:---------------------------------------------|
| `enabled`  | bool     | `true`  | spawn the task                               |
| `interval` | duration | `5m`    | idle time between passes; must be ≥ `10s`    |

`enabled: false` restores today's behaviour exactly. As with
`compaction.enabled` / `interval`, `interval` is parsed and validated
only when the task is enabled, so an unused bad value never blocks
startup. When enabled, an `interval` that is not a duration, or is
below the floor, is a startup configuration error naming the key. It
is not silently clamped, as RFC 0020 validates every other resolved
value.

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
`ourios.template_map.refresh.duration` are new names, registered in
ourios-semconv `v0.3.17` under the purpose-check and
maintainer-approval rule, after an OTel naming-guidance check.

> *Amendment 2026-10-16 (#1008, `drafted`).* A tenant that §3.7 skips
> still gets its `load template_map` span, with the listing's `CLIENT`
> spans beneath it and no `S3.GetObject`. It carries no
> `ourios.template_map.lookup.outcome` and records no `lookups` sample:
> no artifact was looked up. The effect is measured by the existing
> `ourios.storage.operation.duration{ourios.storage.caller=template_map,
> ourios.storage.operation=get}` count and the `S3.GetObject` spans. No
> new name; §7 asks whether a registered `unchanged` outcome is wanted.

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

### 3.7 Skipping a read the listing proves unnecessary (amendment 2026-10-16, #1008, `drafted`)

Every acquisition starts with one recursive listing of the tenant's
audit prefix (RFC 0033 §3.3). The artifact lives under that prefix, so
the listing already returns it, with its `ETag`, next to the audit
objects. Today the listing keeps only the `*.parquet` keys. A pass now
also keeps the artifact's listed `ETag`, and uses it like this.

**What a pass remembers.** Per tenant, at most one *verified listing*: a
frontier `F` and an artifact tag `T`. It is recorded only when the
pass's own acquisition was a **hit** (RFC 0033 §3.3: the artifact
decoded `Valid` with `folded_files == F`, the listed frontier), **and**
the artifact `GET` returned the same tag the listing showed. Nothing
else records one: a publish (the new object's tag is not known
without a read), a lost CAS race, an abstention, a failure, or a
joined flight. Any of those, or an error, drops the tenant's verified
listing.

**What a pass skips.** Before the acquisition it would start, a pass
compares its listing against the tenant's verified listing. If the
listed frontier equals `F` and the listed artifact tag equals `T`, the
pass stops: no artifact `GET`, no flight, no derive, no publish. Any
difference (a new audit object, a removed one, a replaced, removed or
unlisted artifact) runs the acquisition exactly as §3.1 describes,
against the same listing.

**Why the skip changes nothing a query sees.** The skipped acquisition
would have read the artifact the listing shows. That object has tag
`T`, and an object store's `ETag` names one object content, the
identity RFC 0033 §3.4's CAS publish already relies on. So the read
would have returned the bytes the verified hit read, which decode to a
`Valid` map at `F`, which is the listed frontier: a hit. A hit derives
nothing and publishes nothing. So the store after a skip is exactly the
store after the acquisition it replaces, and §2.1's argument holds
unchanged: the task still never serves anything, and every query still
lists and reads for itself. S3's listings are strongly consistent, so
the listing reflects every write and delete that completed before it,
by any process: another querier replica, an ingester elsewhere, or a
bucket lifecycle rule. That is why the proof needs no in-process
signal and holds in multi-process deployments.

**The one interleaving it does not cover.** The proof is about the
store as the listing saw it. The artifact `GET` is not pinned to the
listed version: it reads whatever is at the key when it runs. If
another process replaces or deletes the artifact after the listing and
before that `GET`, the acquisition the skip replaces would have read the
new state. On a deletion it would have folded and published. The skip
does neither. So "exactly the store after the acquisition it replaces"
holds for every change that completed before the listing, and not for
one that lands between the listing and the would-be `GET`. That change
is in the next pass's listing (a different tag, or no artifact) and is
repaired there. The repair is late by at most one pass, `interval` plus
the pass's run time. This RFC accepts that delay. Query correctness
does not depend on it: a query in that window lists and reads for
itself, and pays the fold that the old pass might have paid. The same
window already exists without §3.7 for a change that lands just after a
pass's `GET`. §3.7 moves the window's start from the `GET` to the
listing, a few milliseconds earlier. RFC0060.16 states the behaviour.

**Tags.** On the S3 backend the tag is the object's `ETag`, compared
after stripping one pair of surrounding double quotes, since some
S3-compatible stores spell it quoted in one response and unquoted in
the other. A weak (`W/`) tag never equals a strong one. A store that
lists no `ETag` never skips. On the local backend the tag is the
file's size, modification time (nanoseconds) and, on Unix, inode. The
listing takes it with a `stat`. The read takes it from the open file
handle it reads through, so it names the bytes read. A local publish
installs a fresh temp file by `rename` (RFC 0033 §3.4), which changes
the inode.

**Bounds.** One verified listing per tenant, the size of one frontier.
That is the same size as the listing a pass already holds for that
tenant. It lives in process memory and resets on restart, so a new
binary (and a new artifact format) always reads. A successful tenant
listing drops the verified listings of tenants it no longer names, as
§3.3 does for the back-off counters. Queries never consult it: a query
needs the map itself, and `map_flight` keeps no map once a flight ends.

**What it saves, and what it does not.** On the production node
(2026-10-16) the skip removes the artifact `GET` from about 1,150 of
the 1,255 daily refresh acquisitions (the hits), together with their
bytes. One tenant listing is still paid per tenant per pass, at one
`LIST` request per 1,000 audit objects. On a long-lived tenant those
pages are most of the cost (§3.3 note). Cutting them needs a signal
weaker than a listing, or a smaller listing. §7 lists the options; each
needs a maintainer decision.

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
> - **Given** a store whose tenant has an artifact without `fold_state`,
>   and a folded map the write-through publishes (not abstained, no
>   IO error)
> - **When** the querier starts with refresh enabled, and a query
>   arrives after the first pass has finished
> - **Then** the full fold ran with `acquire.trigger = refresh`
> - **And** the query's lookup is a `hit` with
>   `acquire.trigger = query`, and runs no fold.

> **Scenario RFC0060.2 — A query during a background fold joins it**
> - **Given** the task holding a tenant's flight on a gated fold
> - **When** a query for that tenant at the same frontier arrives
> - **Then** exactly one fold runs, and the query records `joined` and
>   receives the task's map
> - **And** the query's `joined` lookup carries
>   `acquire.trigger = refresh`: the attribute names who initiated the
>   shared acquisition, not who recorded the sample.

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
> - **Given** refresh enabled with `interval: 1s`, or a non-duration
>   value
> - **When** the configuration is resolved
> - **Then** startup fails with an error naming the key
> - **And** with `enabled: false` the same values resolve without
>   error.

> **Scenario RFC0060.7 — Shutdown is prompt**
> - **Given** a pass between tenants, or waiting on its interval
> - **When** the querier shuts down
> - **Then** the task exits without starting another acquisition
> - **And given** a pass whose acquisition is on the blocking pool,
>   gated, **when** the querier shuts down, **then** the task exits
>   without waiting for the gate, and once the gate is released the
>   flight still completes and publishes (it is not cancelled).

> **Scenario RFC0060.8 — Telemetry is registry-conformant**
> - **Given** RFC0060.1's run
> - **When** its spans and metrics are exported
> - **Then** the pass is one root `INTERNAL` span named
>   `refresh template_map`, with each tenant's `load template_map`
>   span beneath it, asserted through the in-memory span exporter
>   (span names are not registry entries)
> - **And** `acquire.trigger` and `refresh.duration` pass weaver
>   live-check against the ourios-semconv registry.

> **Scenario RFC0060.9 — Query-paid full folds drop** (Dash0, after
> deploy)
> - **Given** the node on the release that ships this RFC, and the
>   previous release, each over a 7-day window
> - **When** full folds are counted, normalised per tenant-day
> - **Then** `derive.duration{mode=full, acquire.trigger=query}` on
>   the new release is below the previous release's `mode=full` count.
>   A query that joins a background fold records no derive of its own,
>   so the remaining `query` full folds are those where a query became
>   the acquirer before the pass reached its tenant.

> **Scenario RFC0060.10 — A tenant that persists nothing is backed off**
> - **Given** a tenant whose write-through abstains on every acquisition
> - **When** consecutive passes run
> - **Then** the task acquires it on passes 1, 3 and 7 (skipping 1,
>   then 3), and on no more than one pass in 12 thereafter
> - **And** a query for that tenant still acquires on every request
> - **And** after a hit or a publish the tenant is acquired on the next
>   pass again
> - **And** once a successful listing no longer names the tenant, its
>   counter is dropped; a failed listing drops nothing.
>
> *Amendment 2026-10-16 (#1008, `drafted`).* The third **And** becomes:
> after a hit or a publish, back-off no longer skips the tenant. The next
> pass lists it, and acquires it unless §3.7's skip applies; RFC0060.12
> says when it does. After a publish the skip never applies on the next
> pass, since a publish leaves no verified listing (RFC0060.14), so the
> existing test's "a hit, on the next pass" after a publish still holds.

> **Scenario RFC0060.11 — Passes are spaced from their end, never overlap**
> - **Given** paused time, `interval: 5m`, and a pass whose fold is
>   gated for longer than `interval`
> - **When** the gate is released
> - **Then** no second pass starts while the first is running, and the
>   next pass starts `interval` after the first pass finished, with no
>   catch-up pass in between.

The scenarios below are added by the 2026-10-16 amendment (§3.7, #1008)
and are `drafted`.

> **Scenario RFC0060.12 — An unchanged listing skips the artifact read**
> - **Given** a tenant whose last pass acquired a hit, with the
>   artifact read's tag equal to the listed one
> - **When** the next pass lists the same frontier and the same artifact
>   tag
> - **Then** that pass reads no artifact and starts no flight, and
>   records no `lookups` or `derive.duration` sample for the tenant
> - **And** a query for the tenant afterwards still lists and reads the
>   artifact, and records a `hit` with `acquire.trigger = query`.

> **Scenario RFC0060.13 — Any change in the listing reads again**
> - **Given** a tenant with a verified listing
> - **When** before the next pass (a) a new audit object appears, (b) an
>   audit object is removed, (c) another writer replaces the artifact
>   with a valid one at the same frontier whose tag differs from the
>   verified one, or (d) the artifact is removed
> - **Then** that pass runs the acquisition: (a) and (b) are `stale`
>   lookups and fold, (c) is a `hit` that records a new verified
>   listing, and (d) is a `miss` that folds in full and publishes
> - **And** in every case the store afterwards equals what the
>   acquisition would leave without §3.7
> - **And** a replacement with the same bytes that keeps the same
>   `ETag` (a single-part S3 `PUT` of identical content) is not a
>   change: the pass skips it, correctly, since a read would return the
>   same bytes.

> **Scenario RFC0060.14 — Only a verified hit is remembered**
> - **Given** a tenant whose pass acquisition publishes (a stale or
>   missing artifact), abstains, fails, or reads an artifact whose tag
>   differs from the listed one
> - **When** the next pass that §3.3's back-off does not skip for that
>   tenant lists the same frontier (the very next pass after a publish
>   or a mismatched read; a later one after an abstention or failure)
> - **Then** it reads the artifact (no skip), and only after that read
>   is a hit with matching tags does a later pass skip
> - **And** the back-off is unchanged by §3.7: an abstention or failure
>   still skips the passes §3.3 says, and drops the verified listing
> - **And** once a successful tenant listing no longer names a tenant,
>   its verified listing is dropped; a failed listing drops nothing.

> **Scenario RFC0060.15 — The skip on S3, and its spans**
> - **Given** the S3 backend and a tenant with a verified listing
> - **When** a pass skips it
> - **Then** its `load template_map` span has its `S3.ListObjectsV2`
>   children and no `S3.GetObject` child, and carries no
>   `ourios.template_map.lookup.outcome`
> - **And** after another writer `PUT`s an artifact with different
>   bytes (so a different `ETag`), the next pass has an `S3.GetObject`
>   child again.

> **Scenario RFC0060.16 — A deletion after the listing is repaired one
> pass late**
> - **Given** a tenant with a verified listing
> - **When** another process deletes the artifact after a pass's tenant
>   listing and before the pass decides
> - **Then** that pass skips: it reads nothing and publishes nothing
> - **And** the next pass's listing shows no artifact, and that pass
>   acquires a `miss`, folds and publishes
> - **And** a query between the two passes answers correctly, paying the
>   fold itself.

## 6. Testing strategy

- RFC0060.1–.4, .7, .10 and .11 are integration tests (RFC0060.11
  under tokio paused time) in `ourios-querier` over a
  local store, using the `map_flight` hooks (`before_acquire` gating,
  acquisition and join counters) through the `testing` feature.
- RFC0060.5–.6 are `ourios-server` config tests (RFC 0020 style) plus
  one served-role test.
- RFC0060.8 asserts span names and nesting through the in-memory span
  exporter, and the registry names through the weaver live-check
  harness (`live_check`).
- RFC0060.9 is a Dash0 before/after measurement recorded on the release
  (the RFC0033.14 precedent), not a CI test.

No `proptest` or corpus test applies: the task adds scheduling, not
fold semantics, and the fold's own property tests (RFC0033.10–.13)
already cover what it calls.

> *Amendment 2026-10-16 (#1008, `drafted`).* Test map for §3.7:
>
> | Scenario | Test | Where |
> |:--|:--|:--|
> | RFC0060.12 | `rfc0060_12_an_unchanged_listing_skips_the_artifact_read` | `ourios-querier` `tests/it/rfc0060_refresh.rs`, local backend, acquisition counter + lookup metrics |
> | RFC0060.13 | `rfc0060_13_any_change_in_the_listing_reads_again` | same file; (a)–(d) as four cases over one store |
> | RFC0060.14 | `rfc0060_14_only_a_verified_hit_is_remembered` | same file, plus unit tests of the tag comparison in `map_refresh` / `audit_scan` |
> | RFC0060.15 | `rfc0060_15_the_skip_on_s3_reads_no_artifact` | same file, `#[ignore]`d, run by the `s3-integration` CI job (LocalStack) |
> | RFC0060.16 | `rfc0060_16_a_deletion_after_the_listing_is_repaired_one_pass_late` | same file; a test hook deletes the artifact right after the tenant listing |
>
> The local backend's tag is exercised by .12–.14; the S3 `ETag` path,
> including the listing that carries it, by .15.

## 7. Open questions

Decided for this RFC (maintainer, 2026-10-13); each is revisited only on
the Dash0 evidence named:

- [x] **Pre-emptive full fold near `INCREMENTAL_DEPTH_CAP`: deferred.**
      Cap folds come at most once per 256 published incremental steps
      per tenant (about ten days at the observed rate if every step
      published, rarer otherwise), so a 5-minute pass almost always
      reaches them first. Reopen, as an RFC 0033 derive-policy
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

Open for the 2026-10-16 amendment (#1008); each needs a maintainer
decision:

- [ ] **The listing pages (most of a pass's cost).** §3.7 is the only
      skip a listing can prove. Fewer `LIST` pages need one of these:
      1. **Shrink the listing: compact the audit stream** (recommended).
         Pages grow with audit objects, and the stream writes many small
         objects (one per audit flush and one per compaction event,
         RFC 0005 §3.7). Merging a tenant's closed audit days into a few
         objects shrinks every
         listing, the refresher's and every query's, and the full folds
         too. It changes no freshness rule. It is an RFC 0005 §3.7 and
         RFC 0033 change (merged objects change the frontier, so it
         must not cost a full fold each time), and so a separate RFC.
      2. **Probe recent days, list in full every `k` passes.** A pass
         lists only the current and previous UTC day of each tenant, and
         lists in full every `k`-th pass or when the probe shows a
         change. Most passes cost one page per tenant. But it misses a
         backdated audit object (a WAL replay) or a removed one (a
         lifecycle rule) for up to `k × interval`. In that window a
         query pays the `out_of_order` or `files_removed` full fold that
         the refresher exists to absorb. That weakens §2.1's guarantee
         to "within `k × interval`" for those two causes.
      3. **Back off unchanged tenants.** Lengthen a tenant's interval
         while its listing stays unchanged, as §3.3 does for tenants
         that persist nothing. This weakens freshness the same way as 2,
         for every cause. It also saves little where it matters: the
         tenants with many pages are the active ones, which move every
         few passes.
      4. **A per-tenant change marker.** Every audit writer rewrites a
         small marker object after each audit write, and a pass `HEAD`s
         it instead of listing. This is a new object in the store layout
         (CLAUDE.md §3.5), adds a `PUT` to each audit write, and cannot
         see removals by a lifecycle rule or writes by older binaries, so
         it still needs a periodic full listing. Same weakening as 2.
      5. **An in-process signal from the local ingester** (combined
         role). It sees only this process's writes: not another
         ingester, a lifecycle rule, or a separate-process deployment.
         Not a proof, so it is rejected as the basis of a skip.
- [ ] **An `unchanged` lookup outcome.** §3.5's note records a skip
      only through the storage counters and the span shape. Registering
      `unchanged` as a value of `ourios.template_map.lookup.outcome`
      would let a dashboard count skips directly. That is a new registry
      value under the purpose-check rule. Recommended: not now; the
      `GetObject` count is the quantity #1008 is about.

## 8. References

- RFC 0033 (cached template map): §3.3 freshness and the incremental
  fold, §3.4 CAS publish, §3.5 write-through, §3.7 instruments, §4 (the
  background refresher and ingester write-through alternatives).
- RFC 0038 §3.7 (query-phase spans), RFC 0020 (configuration), RFC 0005
  (partition layout), RFC 0058 (query resource limits).
- #853 (cold map / RSS), #897 (incremental fold), #934 (resumable
  bootstrap), #990 (this RFC's issue).
- CLAUDE.md §3.3, §3.7, §6.3.

## 9. Amendment history

- **2026-10-16 — a pass skips the artifact read its listing proves
  unnecessary (#1008, `drafted`, pending maintainer approval).** Adds
  §3.7: a per-tenant verified listing (frontier and artifact tag),
  recorded only on a hit whose read tag equals the listed one; a pass
  whose listing matches it reads nothing. In-place notes in §3.3 (the
  measured per-pass request count), §3.5 (no new name; the skip's span
  shape) and §6 (test map). Amends RFC0060.10: after a hit or a
  publish the next pass lists the tenant and acquires it unless §3.7's
  skip applies. States the one interleaving §3.7 does not cover, a
  change between the listing and the skipped read, and accepts its
  one-pass repair delay. New scenarios RFC0060.12–.16. New open
  questions in §7 on the listing pages and on an `unchanged` lookup
  outcome. RFC0060.1–.9, .11 and the frontmatter status are unchanged.
  Review (Copilot, #1028): the race qualification and RFC0060.16, the
  RFC0060.10 amendment, a different tag in RFC0060.13 (c), and the
  back-off-eligible pass in RFC0060.14.
