---
rfc: 0017
title: Read-time template registry & query-row rendering
status: red
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-06-20
supersedes: —
superseded-by: —
---

# RFC 0017 — Read-time template registry & query-row rendering

## 1. Summary

Make the querier return **rendered log lines**, not just a count: add
`records: Vec<LogRow>` to `QueryResult` (keeping the `rows` count). A
`LogRow` is a faithful OTLP LogRecord — every OTLP field ingest persisted,
plus the body (rendered for string bodies, returned as structure for
`AnyValue` bodies). Rendering needs each leaf's *versioned* tokens at read
time, so this RFC builds a **read-time template registry**
(`(template_id, template_version) → tokens`) by folding the tenant's audit
stream — and, because a template's initial creation is unaudited today,
**amends the audit contract to emit a `TemplateCreated` event** on leaf
creation. This delivers the typed-row payload RFC 0007 §4.1 specifies but
the engine never built, and is the prerequisite for RFC 0016's endpoint to
return actual logs.

This **amends RFC 0001**: scenario RFC0001.1 ("fresh-leaf creation does not
emit an audit event") is superseded — leaf creation now emits a
`template_created` event (§3.1). It remains a non-merge (`merges_total`
unchanged), so RFC 0001's merge-counting contract is untouched.

## 2. Motivation

A query returns `QueryResult { rows: u64, stats }` today — a count, no
rows. RFC 0007 §4.1 *specifies* `QueryResult` as "typed rows + stats",
but the engine implemented only the count; the typed-row payload was
never built (RFC 0007 §8 left result materialisation open). RFC 0016's
query-serving endpoint is hollow without real rows, and the *point* of an
operator query is to see the **logs**, which means reconstructing each
line from `(template_id, template_version, params, separators)` —
`template_version` selects the correct token set for that leaf over time
(§3.5) — per the CLAUDE.md §3.3 bit-identical
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
`event_kind` ordinal **`6`** — the next free value after the existing
`0`–`5` (`template_widened`=0, `template_type_expanded`=1,
`template_widening_rejected_degenerate`=2, `compaction`=3,
`alias_asserted`=4, `alias_retracted`=5 in
`crates/ourios-core/src/audit.rs`) —
paired with the `event_type` string `template_created` (an
**append-only** addition per RFC 0005 §3.7 — new ordinal, no renumber, so
old readers are unaffected and §3.5 migration holds). It reuses the
existing audit columns: `new_template` = the initial tokens,
`new_version = 1`, and `old_template`/`old_version` left **`NULL`** — the
OPTIONAL "not applicable to this event kind" sentinel per RFC 0005 §3.7
(no prior template), not a zero/empty value. The in-memory
`TemplateChange::Created` variant carries **only** `new_template`: a leaf is
always born at version 1, so rather than carry-and-validate a `new_version`
field the invariant is made *unrepresentable* (there is no way to construct
a creation at another version). The writer supplies the canonical
`new_version = 1` for the on-disk column (`TEMPLATE_INITIAL_VERSION`); the
reader does not read it back into the variant. The miner emits it at leaf
creation, on the same WAL-before-ack path as the existing template events,
so by the time a v1 row reaches Parquet its `TemplateCreated` event is
durable.

### 3.2 `derive_template_registry` — fold the audit stream

A querier function mirroring `alias_store::derive_alias_map`
(`crates/ourios-querier/src/alias_store.rs:40`): scan the tenant's
`audit/tenant_id=…` Parquet
files, read the template events (`template_created`, `template_widened`,
`template_type_expanded`), and fold them — in the pinned deterministic
order `(timestamp, file path lexicographic, within-file row index)` (RFC
0005 §3.7.1) — into

```text
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
  versioned tokens + params + separators (bit-identical, CLAUDE.md §3.3);
- **lossy / parse-failure** (`RetainedVerbatim`) → the retained `body`
  verbatim;
- **structured** (`body_kind = Structured`) → the structured `AnyValue`,
  decoded from the `body` column's canonical JSON (RFC 0005 §3.3) and
  returned **as structure** — not flattened to a byte line, which would
  discard the map/array shape the OTLP `Body` is required to preserve
  (see §3.4).

A row whose `(template_id, version)` isn't in the registry (should not
happen once §3.1 lands; a corrupt/foreign row) renders `RetainedVerbatim`
from `body` (or empty) — never a panic, never a wrong line.

### 3.4 `LogRow` + `QueryResult` (B1/B2-compatible)

`QueryResult` keeps `rows: u64` (the count — B1/B2 and existing tests are
untouched) and **adds `records: Vec<LogRow>`** (the returned rows, ≤
`limit`). `LogRow` is Ourios-owned (H6 — no arrow/DataFusion type crosses
the boundary). The endpoint (RFC 0016) serialises `records`.

**Priority: OTLP fidelity outranks downstream API stability.** Ourios is
pre-release and OTLP-native; where faithful OTLP shape requires changing
or breaking a public type, that is acceptable — we do **not** compromise
the LogRecord shape to preserve a Rust API. `QueryResult` is not
`#[non_exhaustive]` today (`crates/ourios-querier/src/lib.rs:117` — only
`QueryError` is), so **both** adding a public field **and** marking the
struct `#[non_exhaustive]` are one-time Rust semver breaks for downstream
struct literals / patterns. Both are accepted — we don't compromise the
shape to preserve the API — and the `#[non_exhaustive]` mark buys that
*subsequent* field additions (the execution slice will add more) are
non-breaking. The change is in any case *behaviour*-compatible — B1/B2 and
existing tests read `rows`/`stats`, which are unchanged — so
"B1/B2-compatible" is the precise claim, not "non-breaking at the type
level".

**OTLP fidelity is a first-class requirement of this RFC, not a v1
best-effort.** Ourios is an OTLP-native log backend, so a returned row
MUST carry **every OTLP LogRecord field that ingest persisted** — a read
that drops fields the wire carried and the schema stored is a fidelity
bug. The storage path (RFC 0005 §3.2 schema; `ourios-core` `record.rs` /
`otlp.rs`) already persists the full record, so `LogRow` mirrors it
field-for-field as Ourios-owned typed fields:

- `time_unix_nano` (required) and `observed_time_unix_nano` (optional);
- `severity_number` + `severity_text`;
- trace context — `trace_id` (16 B), `span_id` (8 B), `flags`;
- `event_name`;
- `attributes` and `resource_attributes`, **decoded** from the stored
  canonical JSON (RFC 0005 §3.3) into structured key/values — not handed
  back as an opaque JSON blob;
- `scope_name` / `scope_version`;
- `dropped_attributes_count` (carried verbatim, never recomputed);
- the **body** (below), with its `Reconstruction` marker.

**Body — the OTLP `Body` is an `AnyValue` (string *or* structured).** The
storage path already distinguishes the two via the `body_kind`
discriminator (RFC 0005 §3.2) and stores structured bodies as canonical
JSON (RFC 0005 §3.3). `LogRow` models the body as a sum type so invalid
states are unrepresentable rather than a flat `line` + side flags:

```rust
enum LogBody {
    /// body_kind = String — the §3.3 three-zone result.
    Rendered { line: Vec<u8>, reconstruction: Reconstruction },
    /// body_kind = Structured — the AnyValue decoded from canonical JSON,
    /// returned as structure (map/array), never flattened to a line.
    Structured(AnyValue),
}
```

A string body yields `Rendered` (clean → `Faithful`; lossy/parse-failure
→ `RetainedVerbatim`, §3.3). A structured body (`body_kind = Structured`)
yields `Structured`, preserving the map/array shape the OTLP spec mandates
`Body` retain — this is the `render`-contract `Faithful` case (the
canonical JSON in `body` round-trips, no template walk). Its one edge: a
structured row whose `body` is **absent** (a corrupt row — there is no
structure to return) falls back to `Rendered { line: empty,
RetainedVerbatim }`, never `Structured` over nothing, matching
`ourios_miner::reconstruct::render`'s
`BodyKind::Structured → (empty, RetainedVerbatim)` arm. So the
`Reconstruction` marker lives on `Rendered`; a `Structured` value is
faithful by construction.

The three OTLP fields ingest does **not** persist today —
`InstrumentationScope.attributes`, and the per-resource / per-scope
`schema_url` (dropped at the receiver, RFC 0003 §6.8 / §9) — are
consequently not returnable. Closing those is an **ingest-side** fix
(RFC 0003), out of scope here; this RFC's contract is that `LogRow`
returns everything the schema holds. Flagged in §7 as the residual
fidelity gap.

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

> **Scenario RFC0017.3 — a clean row renders bit-identically (CLAUDE.md §3.3)**
> - **Given** a stored clean-path row (`Faithful`-eligible) and the
>   derived registry
> - **When** the querier renders it via the registry tokens
> - **Then** the rendered line equals the originally-ingested line
>   byte-for-byte (the CLAUDE.md §3.3 invariant), and the row's
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

> **Scenario RFC0017.6 — typed-row payload is returned, B1/B2-compatible**
> - **Given** a query with a `limit`
> - **When** it runs
> - **Then** `QueryResult.records` holds up to `limit` `LogRow`s
>   (rendered/structured body + marker + the OTLP fields per §3.4),
>   **and** `QueryResult.rows` (the count) and `stats` are unchanged
>   so B1/B2 and existing tests still pass
> - **And** `QueryResult` is marked `#[non_exhaustive]` (which, with
>   the field addition, is an accepted one-time semver break per §3.4)
>   so that *subsequent* field additions are non-breaking

> **Scenario RFC0017.7 — no engine internals leak (H6)**
> - **Given** the public `LogRow` / `QueryResult` surface
> - **When** inspected
> - **Then** no `arrow`/`DataFusion`/SQL type or text appears in it;
>   all fields are Ourios-owned

> **Scenario RFC0017.8 — every persisted OTLP field round-trips on read**
> - **Given** a stored row whose ingest carried the full OTLP
>   LogRecord field set (timestamps, severity number + text, trace
>   context, scope name/version, attributes, resource attributes,
>   dropped count, event name)
> - **When** the querier returns it as a `LogRow`
> - **Then** each of those fields equals what the schema stored
>   (RFC 0005 §3.2), `attributes` / `resource_attributes` are decoded
>   to structured key/values (not an opaque JSON blob), and **no
>   stored OTLP field is dropped on the read path**

> **Scenario RFC0017.9 — a structured (`AnyValue`) body is returned as structure**
> - **Given** a stored row with `body_kind = Structured` (the OTLP
>   `Body` was a map/array, canonical JSON in `body`, RFC 0005 §3.3)
> - **When** the querier returns it
> - **Then** the body is `LogBody::Structured(AnyValue)` preserving the
>   original map/array shape — **not** flattened into a byte line — and
>   round-trips the ingested `AnyValue`

## 6. Testing strategy

- **RFC0017.1** — a miner unit/integration test asserting a
  `template_created` event on first leaf allocation (with tokens), plus an
  audit-schema test that the new `event_kind`/`event_type` is appended
  (existing ordinals unchanged).
- **RFC0017.2 / .5** — `derive_template_registry` unit tests over a
  synthetic audit stream (creation + widening), asserting completeness and
  per-version keying; deterministic-order test mirroring the alias-map
  tests.
- **RFC0017.3** — a **property test** reusing the CLAUDE.md §3.3 invariant: for a
  corpus of mined rows, registry-rendered line == original (or flagged
  lossy). Cross-references `ourios-miner`'s reconstruction property test.
- **RFC0017.4** — fixtures for lossy + parse-failure + structured rows →
  expected verbatim/canonical body + marker.
- **RFC0017.6** — querier test asserting `records` length ≤ `limit`, the
  rendered content, and that `rows`/`stats` are unchanged (a B1/B2-style
  count assertion still holds).
- **RFC0017.7** — a grep-style guard that the public crate surface has no
  `arrow`/`datafusion` types (mirrors the RFC0007.3 / H6 guard).
- **RFC0017.8** — a querier test that ingests a record populating **every**
  OTLP field, stores it, queries it back, and asserts each `LogRow` field
  equals the ingested value (a field-completeness assertion over the RFC
  0005 §3.2 column set), with `attributes` / `resource_attributes` decoded
  to structured key/values. The assertion enumerates the field set so a
  newly-added stored column that the read path forgets fails the test.
- **RFC0017.9** — a property/round-trip test: for structured-body inputs
  (`AnyValue` maps/arrays), `LogRow.body == LogBody::Structured(v)` where
  `v` equals the ingested `AnyValue` (decoded canonical JSON), never a
  flattened line. Cross-references the `ourios-core` canonical
  encode/decode property tests.

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
- [x] **Structured-body rendering** — *resolved* (§3.3 / §3.4): the OTLP
  `Body` is an `AnyValue`, and the storage path already preserves the
  structured case (`body_kind = Structured`, canonical JSON in `body`,
  RFC 0005 §3.2/§3.3). `LogBody::Structured(AnyValue)` returns it
  as structure; only string bodies walk the template. No flattening.
- [ ] **Residual ingest-side fidelity gap** — `LogRow` returns every OTLP
  field the schema stores, but three are dropped at the *receiver* today
  and so cannot be returned: `InstrumentationScope.attributes`, and the
  per-resource / per-scope `schema_url` (RFC 0003 §6.8 "out of scope" /
  §9). For a backend whose thesis is OTLP-native fidelity these are worth
  closing — but at ingest (an RFC 0003 schema addition + RFC 0005 columns),
  not in this read-path RFC. Track as an RFC 0003 follow-up; this RFC is
  faithful to the *stored* record by construction.
- [ ] **Backfill** — existing audit streams predate `template_created`;
  templates created before this lands won't have a creation event, so their
  v1 rows aren't in the registry and hit the §3.3 not-in-registry fallback.
  **Caveat:** that fallback renders `RetainedVerbatim` from `body`, but a
  clean-path `body_kind = String` row has **no** `body` (absent by design,
  RFC 0005 §3.2) — so the fallback yields an **empty** line, not the
  original, unless tokens are recovered. Options: accept empty-line for
  pre-`template_created` clean rows (pre-release, leaning this), a one-time
  audit backfill, or recover v1 tokens from a surviving v1 row's shape
  (the §4 "reconstruct v1 from a surviving row" alternative, rejected there
  as fragile). Pre-release lean: acceptable + documented.

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
  pattern); `ourios_miner::reconstruct::render`;
  `crates/ourios-core/src/audit.rs` (`TemplateChange`);
  `ourios_miner::tree::OwnedToken`.
