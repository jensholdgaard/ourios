---
rfc: 0017
title: Read-time template registry & query-row rendering
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-06-20
supersedes: —
superseded-by: —
---

# RFC 0017 — Read-time template registry & query-row rendering

## 1. Summary

Make the querier return **rendered log lines**, not just a count. The
engine reconstructs each matching row's body via
`reconstruct::render(record, tokens)`, which needs the leaf's tokens at
read time. We build that **read-time template registry**
(`(template_id, template_version) → tokens`) by **folding the tenant's
audit stream** — the same deterministic pattern `derive_alias_map`
already uses — rather than the deferred cached-map artifact. That fold is
only *complete* if every template version's tokens are in the audit
stream; widening/type-expansion events already carry them, but a
template's **initial creation is unaudited today** — so this RFC also
**amends the audit contract to emit a `TemplateCreated` event** on leaf
creation. The querier then returns Ourios-owned `LogRow`s (a new
`records: Vec<LogRow>` on `QueryResult`, alongside the existing `rows`
count), each carrying the rendered line + reconstruction marker. This
delivers the typed-row payload RFC 0007 §4.1 ("Crate shape") *specifies*
but the engine never implemented (it returns only a count), plus the
query-time rendering that needs it, and is the prerequisite for RFC
0016's HTTP endpoint to return actual logs.

## 2. Motivation

A query returns `QueryResult { rows: u64, stats }` today — a count, no
rows. RFC 0007 §4.1 *specifies* `QueryResult` as "typed rows + stats",
but the engine implemented only the count; the typed-row payload was
never built (RFC 0007 §8 left result materialisation open). RFC 0016's
query-serving endpoint is hollow without real rows, and the *point* of an
operator query is to see the **logs**, which means reconstructing each
line from `(template_id, params, separators)` per the §3.3 bit-identical
contract — or returning the retained `body` for lossy/parse-failure rows.

Reconstruction needs the leaf's **tokens** at read time. RFC 0005 §3.7.1
already commits to the audit-stream-derivation model for read-time maps
(the alias map is derived this way; the cached artifact is "deferred, not
designed away" — the manifest fork #94/#147). So the registry should be
**derived from the audit stream**, consistent with the alias map. The
blocker: derivation is only correct if the audit stream records **every**
template version's tokens. It records widening (`new_template`) and
type-expansion, but **not a template's initial (version 1) creation** —
so v1 rows have no derivable tokens. Closing that gap (a `TemplateCreated`
audit event) makes the registry complete and the rendering correct.

## 3. Proposed design

### 3.1 The audit gap → a `TemplateCreated` event

When the miner allocates a new leaf it assigns a `template_id` /
`template_version = 1` but emits **no audit event**; the first event for
that leaf is its first *widening*. So v1 tokens live only in the miner's
in-memory tree, never durably in the audit stream — unrecoverable for a
read-time derivation once the originating rows age out.

Add a `TemplateChange::Created` variant (RFC 0001 §6.4) and a new audit
`event_kind` ordinal + `event_type` string `template_created` (an
**append-only** addition per RFC 0005 §3.7 — new ordinal, no renumber, so
old readers are unaffected and §3.5 migration holds). It reuses the
existing audit columns: `new_template` = the initial tokens,
`new_version = 1`, and `old_template`/`old_version` left **`NULL`** — the
OPTIONAL "not applicable to this event kind" sentinel per RFC 0005 §3.7
(no prior template), not a zero/empty value. The miner emits it at leaf creation, on the same WAL-before-ack
path as the existing template events, so by the time a v1 row reaches
Parquet its `TemplateCreated` event is durable.

### 3.2 `derive_template_registry` — fold the audit stream

A querier function mirroring `alias_store::derive_alias_map`
(`crates/ourios-querier/src/alias_store.rs:40`): scan the tenant's
`audit/tenant_id=…` Parquet
files, read the template events (`template_created`, `template_widened`,
`template_type_expanded`), and fold them — in the pinned deterministic
order `(timestamp, file path lexicographic, within-file row index)` (RFC
0005 §3.7.1) — into

```
TemplateRegistry = HashMap<(template_id: u64, version: u32), Vec<OwnedToken>>
```

keyed by `(template_id, new_version)`, value = the `new_template` tokens
parsed from the canonical `["lit", "<*>", …]` encoding (RFC 0005 §3.7,
the same shape the miner's `Vec<OwnedToken>` produces). It is derived once
per query (like the alias map), only when the query actually returns rows.

### 3.3 Query-time rendering

`Querier::execute` (the `lib.rs` count bottleneck) gains a row-returning
path: instead of only the `COUNT(*)` aggregate, it collects the matching
`RecordBatch`es (bounded by the DSL `limit`), decodes each into the
fields `reconstruct::render` needs, looks up
`registry[(template_id, template_version)]`, and renders — honouring the
three-zone model (RFC 0001 §6.3 / §6.6):

- **clean** (`Reconstruction::Faithful`) → the line rebuilt from the
  versioned tokens + params + separators (bit-identical, §3.3);
- **lossy / parse-failure** (`RetainedVerbatim`) → the retained `body`
  verbatim;
- **structured** → the §6.1 canonical body.

A row whose `(template_id, version)` isn't in the registry (should not
happen once §3.1 lands; a corrupt/foreign row) renders `RetainedVerbatim`
from `body` (or empty) — never a panic, never a wrong line.

### 3.4 `LogRow` + `QueryResult` (non-breaking)

`QueryResult` keeps `rows: u64` (the count — B1/B2 and existing tests are
untouched) and **adds `records: Vec<LogRow>`** (the returned rows, ≤
`limit`). `LogRow` is Ourios-owned (H6 — no arrow/DataFusion type
crosses the boundary): the projected schema columns plus the rendered
`line: Vec<u8>` and the `Reconstruction` marker. The endpoint (RFC 0016)
serialises `records`.

`LogRow` carries the OTLP log record's top-level fields as first-class
typed columns — `timestamp` / `observed_timestamp`, `severity_number` +
`severity_text`, trace context (`trace_id` / `span_id` / `trace_flags`),
and `attributes` — not just the rendered body line + marker. The rendered
`line` corresponds to the OTLP **`Body`** field; the other fields are
returned alongside it so a record is a faithful OTLP-shaped row, not a
bare string (OTel logs data model — these are the named top-level fields a
backend should preserve on read).

### 3.5 Version correctness

A row carrying `template_version = N` renders against the **N-version**
tokens (the event whose `new_version = N`), not the latest — so a line
ingested before a widening reconstructs as it was then. The registry is
keyed by `(template_id, version)` precisely for this.

### 3.6 Performance

Deriving the registry folds the audit stream per query — O(audit events),
the same cost profile as the alias map, acceptable for v1. The
materialised cache (the RFC 0005 §3.7.1 / manifest-fork artifact) is the
deferred latency/recovery optimisation, not required for correctness.
Rendering is bounded to the returned (`limit`-capped) rows.

## 4. Alternatives considered

**Derive ≥v2, reconstruct v1 from a surviving row.** Skip the
`TemplateCreated` event; if a v1 token set is missing, recover it from any
still-present v1 row's shape. Rejected — fragile and lossy: once every v1
row of a template is compacted/retention-expired, its tokens are
unrecoverable, so a later query over an older file that *does* reference
v1 renders wrong (or can't render). Auditing creation is the only
complete fix.

**Cached-map artifact first (the manifest fork #94/#147).** Persist the
registry as a published per-tenant file. Rejected as the *first* step:
it's a latency/recovery optimisation over the derivation (RFC 0005
§3.7.1 says exactly this), bigger, and entangled with the deferred
atomic-publish manifest decision. Derivation is correct and sufficient
once creation is audited; the cache can layer on later without changing
the contract.

**Store the rendered line in Parquet at ingest.** Write the reconstructed
line as a column so the querier needn't render. Rejected — it duplicates
the bytes the template/params reduction exists to avoid (pillar #2), and
re-introduces the storage cost the design removes.

**Push tokens / render client-side.** Return `(template_id, params,
tokens)` and let the client reconstruct. Rejected — leaks internal
representation through the public surface (H6) and pushes the
three-zone reconstruction logic onto every consumer.

**Don't render — structured rows only.** Return the columns, no line.
Rejected per the maintainer's decision: a query that can't show the log
line isn't a usable query API.

## 5. Acceptance criteria

> **Scenario RFC0017.1 — initial template creation is audited**
> - **Given** a miner ingesting a line that creates a new leaf
> - **When** the leaf (and its `template_id`) is allocated
> - **Then** a `template_created` audit event is emitted carrying
>   `(template_id, new_version = 1, new_template = the initial
>   tokens)` on the WAL-before-ack path
> - **And** the new `event_kind` ordinal / `event_type` string is an
>   append-only addition (no existing ordinal renumbered), per RFC
>   0005 §3.7

> **Scenario RFC0017.2 — the registry derives completely from the audit stream**
> - **Given** a tenant audit stream with `template_created`,
>   `template_widened`, and `template_type_expanded` events
> - **When** `derive_template_registry` folds it (deterministic
>   `(timestamp, path, row)` order)
> - **Then** the registry contains the tokens for **every**
>   `(template_id, version)` the stream describes, **including
>   version 1**, with later versions not clobbering earlier ones

> **Scenario RFC0017.3 — a clean row renders bit-identically (§3.3)**
> - **Given** a stored clean-path row (`Faithful`-eligible) and the
>   derived registry
> - **When** the querier renders it via the registry tokens
> - **Then** the rendered line equals the originally-ingested line
>   byte-for-byte (the §3.3 invariant), and the row's
>   `Reconstruction` marker is `Faithful`

> **Scenario RFC0017.4 — lossy / parse-failure rows return the retained body**
> - **Given** a row flagged lossy or with no template (parse
>   failure), whose `body` was retained
> - **When** the querier renders it
> - **Then** the returned line is the retained `body` verbatim and
>   the marker is `RetainedVerbatim` — no template walk, never a
>   wrong reconstruction

> **Scenario RFC0017.5 — rows render against their own template version**
> - **Given** a template that has widened (versions 1 and 2 both
>   present in the audit stream) and rows at each version
> - **When** the querier renders a `version = 1` row
> - **Then** it renders against the version-1 tokens, not the
>   widened version-2 tokens

> **Scenario RFC0017.6 — typed-row payload is returned, non-breaking**
> - **Given** a query with a `limit`
> - **When** it runs
> - **Then** `QueryResult.records` holds up to `limit` `LogRow`s
>   (rendered line + marker + columns), **and** `QueryResult.rows`
>   (the count) and `stats` are unchanged so B1/B2 and existing
>   tests still pass

> **Scenario RFC0017.7 — no engine internals leak (H6)**
> - **Given** the public `LogRow` / `QueryResult` surface
> - **When** inspected
> - **Then** no `arrow`/`DataFusion`/SQL type or text appears in it;
>   all fields are Ourios-owned

## 6. Testing strategy

- **RFC0017.1** — a miner unit/integration test asserting a
  `template_created` event on first leaf allocation (with tokens), plus an
  audit-schema test that the new `event_kind`/`event_type` is appended
  (existing ordinals unchanged).
- **RFC0017.2 / .5** — `derive_template_registry` unit tests over a
  synthetic audit stream (creation + widening), asserting completeness and
  per-version keying; deterministic-order test mirroring the alias-map
  tests.
- **RFC0017.3** — a **property test** reusing the §3.3 invariant: for a
  corpus of mined rows, registry-rendered line == original (or flagged
  lossy). Cross-references `ourios-miner`'s reconstruction property test.
- **RFC0017.4** — fixtures for lossy + parse-failure + structured rows →
  expected verbatim/canonical body + marker.
- **RFC0017.6** — querier test asserting `records` length ≤ `limit`, the
  rendered content, and that `rows`/`stats` are unchanged (a B1/B2-style
  count assertion still holds).
- **RFC0017.7** — a grep-style guard that the public crate surface has no
  `arrow`/`datafusion` types (mirrors the RFC0007.3 / H6 guard).

Each scenario id (`RFC0017.N`) is referenced from its test so the mapping
is greppable (`docs/verification.md` §2).

## 7. Open questions

- [ ] **Cached-map artifact** — when to materialise the registry (the RFC
  0005 §3.7.1 / manifest-fork optimisation) vs. always deriving. Deferred;
  derivation is the v1 contract.
- [ ] **Registry memory bound** — for tenants with very large template
  counts, is the per-query in-memory registry acceptable, or does it need
  a cap / lazy per-`(id,version)` lookup?
- [ ] **`TemplateCreated` payload** — does it also carry `slot_types`
  (like `TypeExpanded`), or just tokens? (Leaning tokens-only for v1;
  slot types are derivable / not needed for `render`.)
- [ ] **Structured-body rendering** — confirm the §6.1 canonical-encoding
  path needs no registry (it doesn't walk a template) and is rendered
  directly from `body`. The OTLP `Body` is an `AnyValue` — a plain string
  **or** a structured map/array — so the miner's template walk applies only
  to the string-body case; a structured body has no faithful single-line
  rendering and falls in the structured zone (canonical encoding into
  `line`). Open: does `LogRow` flatten a structured body into `line:
  Vec<u8>` (lossy on shape), or should it preserve the `AnyValue` structure
  so the consumer can re-render? Leaning canonical-bytes-into-`line` for
  v1; revisit if operators need the structured shape back.
- [ ] **Backfill** — existing audit streams predate `template_created`;
  templates created before this lands won't have a creation event.
  Acceptable (best-effort `RetainedVerbatim` fallback for those), or is a
  one-time backfill warranted? (Leaning acceptable for pre-release.)

## 8. References

- RFC 0001 §6.4 (template audit events), §6.6 (render contract), §6.7
  (audit stream); RFC 0005 §3.7 (audit schema; the append-only
  event-type rule, the canonical token encoding), §3.7.1 (derive-from-
  audit model; the deferred cached artifact / manifest fork #94/#147);
  RFC 0007 §4.1 (specifies `QueryResult` as typed rows + stats — the
  payload this RFC implements), §8 (result-materialisation open
  question); RFC 0002 (`render` stage); RFC 0010 (drift, the other
  audit-derived query); RFC 0016 (the query-serving endpoint that
  consumes `records`).
- `CLAUDE.md` §3.1 (audit events on template change), §3.3 (bit-identical
  reconstruction), §3.5 (schema migration — append-only audit types),
  hazard H6 (no DataFusion surface leak), §3.7 (multi-tenancy — the
  registry is per-tenant).
- `crates/ourios-querier/src/alias_store.rs` (`derive_alias_map`, the
  pattern); `ourios_miner::reconstruct::render`; `ourios-core` `audit.rs`
  (`TemplateChange`); `ourios_miner::tree::OwnedToken`.
