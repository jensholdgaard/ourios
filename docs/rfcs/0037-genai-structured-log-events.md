---
rfc: 0037
title: GenAI / structured-event log handling
status: green
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-22
supersedes: —
superseded-by: —
---

# RFC 0037 — GenAI / structured-event log handling

> **Status: `green` (2026-07-23).** All five §5 acceptance scenarios pass.
> Implementation landed in three slices — §3.1 event-keyed templates (#599),
> §3.2 fidelity + `structured_body_bytes` observability (#600), §3.3
> group-by-promoted-attribute (#601) — carrying **RFC0037.1** (miner
> `rfc0037_1_*` example + property tests), **RFC0037.3** (miner
> `rfc0037_3_*` unit + `tests/rfc0037_structured_body.rs` integration), and
> **RFC0037.4** (`ourios-querier` `rfc0037_4_count_by_promoted_attribute`).
> **RFC0037.2** (structured-body reconstruction) and **RFC0037.5**
> (absent-body parity) are covered by the standing reconstruction property
> (RFC 0024 generates structured bodies) and `rfc0025_absent_body.rs`
> respectively — see §6. `validated` waits on the v9 corpus (§3.4).
>
> **Direction resolved 2026-07-22 (maintainer): the §3.2 fork is decided
> — Option A (full fidelity + observability).** Structured bodies are never
> truncated; the guard against hazard #2 is a `structured_body_bytes`
> metric plus a per-service alert, not a size cap. The opt-in
> `structured_body_byte_limit` (former Option B) is dropped and recorded as
> a rejected alternative (§4). The remaining §7 items are minor scoping
> defaults, not forks.

## 1. Summary

OpenTelemetry now models GenAI/LLM agent activity as **log events**: a
`LogRecord` carrying an `event_name` (e.g.
`gen_ai.client.inference.operation.details`) and a **structured** body — an
`AnyValue` array/kvlist such as `gen_ai.input.messages` — rather than a
string. Ourios already ingests, stores, and reconstructs these records
correctly today (structured body → canonical JSON in the `body` column,
`event_name` as a first-class column, RFC 0025 covering the absent-body
event shape). This RFC does three things on top of that working base:
(1) closes a hazard-#2 gap where structured bodies bypass the
`param_byte_limit` cardinality guard entirely; (2) folds `event_name` into
the structured-template key so distinct event types get distinct
`template_id`s instead of collapsing to one `(severity, scope)` sentinel;
and (3) extends the `count … by` surface to group by a **promoted**
attribute column, so the canonical GenAI aggregation ("completions by
`gen_ai.request.model`") is expressible. Scope stays within §1 — these are
logs, not a metrics or evaluation backend.

## 2. Motivation

**Why now.** The OpenTelemetry GenAI semantic conventions have moved to a
dedicated `semantic-conventions-genai` repository and stabilized the
"chat history as a log event" shape (`gen_ai.input.messages` /
`gen_ai.output.messages`, the consolidated
`gen_ai.client.inference.operation.details` event, opt-in via
`OTEL_SEMCONV_STABILITY_OPT_IN=gen_ai_latest_experimental`). The
opentelemetry-demo `main` branch adds an `agent` service (LangGraph ReAct
+ LLM calls) emitting exactly these records; the demo's next release is the
planned `corpus/otel-demo-v9` capture (issue #546). When that corpus lands,
Ourios should already have a specified, tested contract for the workload
rather than discovering its behavior empirically.

**Why at this layer.** GenAI event bodies are wordy, structured, unique per
request, and carry the payload operators most want to retain verbatim
("show me the exact prompt and completion"). That intersects three Ourios
invariants/hazards directly:

- **Hazard #2 / §3.2 (parameter cardinality).** A single
  `gen_ai.input.messages` array can be tens of kilobytes. The
  `param_byte_limit` overflow guard (`crates/ourios-miner/src/overflow.rs`)
  protects the *string* path only; structured bodies are stored whole
  (`cluster.rs` `ingest_structured`) with **no size bound**. This is an
  unguarded ingress for large blobs.
- **Invariant §3.3 (bit-identical reconstruction).** Operators want the
  chat history back byte-for-byte. Any bounding of body size must either
  preserve fidelity or explicitly flag the record lossy — never silently
  truncate.
- **Hazard #6 (query DSL surface).** The value of promoting a `gen_ai.*`
  attribute to a column is grouped counting; today promotion buys
  filtering only, and `count … by` rejects attribute-column group keys.

An OTLP-native logs backend that mishandles the AI-observability workload
of 2026 misses the moment. This RFC makes the handling deliberate.

## 3. Proposed design

### 3.0 Current behavior (verified, unchanged)

The following already hold and are **not** changed by this RFC; they are
recorded so the delta is unambiguous:

- A non-string `AnyValue` body becomes `Body::Structured`
  (`crates/ourios-core/src/otlp.rs`) and is stored whole as canonical JSON
  in the nullable `body` column with `body_kind = Structured` (ordinal 1).
  The Drain tree is never walked for structured bodies.
- `event_name` is captured end-to-end and stored as a nullable `Utf8`
  column (`crates/ourios-parquet/src/lib.rs`, `columns::EVENT_NAME`).
- RFC 0025 represents the event-shaped absent-body record (`event_name` +
  attributes, no body) as `body_kind = Absent` (ordinal 2), `body = NULL`.
- A `gen_ai.*` resource/log attribute can be promoted to a pruned,
  full-operator, **filterable** column via
  `storage.promoted_attributes.{resource,log}` (RFC 0022), advertised in
  the `ourios://query-schema` resource (RFC 0032).

### 3.1 Structured-template key includes `event_name`

Today `ingest_structured` keys the per-tenant structured-template map on
`(severity_number, scope_name)`, so every GenAI event in one scope collapses
to a single `template_id` regardless of `event_name`. Fold `event_name` into
the key: `(severity_number, scope_name, event_name)`. Consequences:

- `count … by template_id` distinguishes `…inference.operation.details`
  from a tool-call event from an application log line in the same scope.
- Row-group pruning on `template_id` (pillar #1) becomes selective for
  event types, not just severity/scope.
- The change is additive to the template population; it re-bases RFC 0024
  calibration for any corpus containing structured events (§6).

The `template_id` allocation stays deterministic and per-tenant; the
`confidence = 1.0`, `lossy_flag = false`, empty-params invariants for
structured records (RFC 0001 §6.1) are preserved.

### 3.2 Bounding the structured body against hazard #2

Structured bodies must not be an unguarded ingress, but §3.3 forbids silent
truncation and the structured body *is* the payload the operator wants back
verbatim. **Resolved (Option A): never truncate; guard by observation, not
by a cap.**

- **No size cap, ever.** A structured body is retained whole regardless of
  size. Fidelity (§3.3) is never at risk, and `lossy_flag` stays `false`
  for the structured path (RFC 0001 §6.1).
- **`structured_body_bytes` metric.** A histogram observing the
  canonical-JSON byte length of every structured body, dimensioned by
  service so an operator can see which service emits large bodies.
  Instrumented via an OTel meter (Ourios's self-observability convention),
  not a Prometheus client.
- **Per-service alert** when the structured-body byte rate crosses a soft
  threshold, mirroring the existing §3.2 param-overflow-rate alert. This is
  the operational signal that a service is shipping oversized payloads —
  actionable at the source (the emitter), which is where the fix belongs.

Rationale for no cap: hazard #2's failure mode is *dictionary-encoding
collapse* on a column of otherwise-repeating values, and the `body` column
is **not** dictionary-encoded — the Parquet writer disables the dictionary
on `body` by design (`crates/ourios-parquet/src/writer.rs` §3.6: bodies are
unbounded, high-entropy, and dictionary encoding is the wrong choice for
them). So a large structured body has no dictionary to collapse; the hazard
is structurally absent for this column. The remaining cost of a large body
is raw storage size — an *operational* signal, not a correctness threat —
and capping it would trade the operator's payload (the thing they most want)
for bytes. The write-side layout work (RFC 0036) already governs
file/row-group sizing, so large bodies are bounded at the *storage* layer
without ever discarding data.

**`docs/hazards.md` §2 is amended** to state that structured bodies are
retained whole and guarded by the `structured_body_bytes` metric + alert
(not a length cap) — closing the current silent gap in the written
invariant, which reads as if the `param_byte_limit` covers all bodies.

### 3.3 Group a count by a promoted attribute column

Extend the `count … by` group surface so a `GroupTerm` may reference a
**promoted** attribute column (`service` already works; generalize to
`resource.<key>` / `attr.<key>` when the key is in the effective promoted
set and its column is present in the scanned union schema). The compiler
(`crates/ourios-querier/src/compile.rs` `field_group_expr`) currently
rejects `Field::Resource(_)` / `Field::Attr(_)` group keys; allow them
**only** when promoted, falling back to the same typed-NULL literal that
`service` uses for partitions predating the promotion. A non-promoted
attribute stays rejected for grouping (it has no pruned column and grouping
by an unpruned JSON `LIKE` scan is a footgun) — the error message points the
user at promotion, and the `ourios://query-schema` cost model already
classifies the distinction.

This makes the canonical GenAI aggregation expressible:
`gen_ai.operation.name == "chat" | count by attr.gen_ai.request.model, bucket(1h)`.
It reuses the L4 machinery RFC 0031 measures; the `(bucket, group_key)`
comparison unit is unchanged.

### 3.4 Corpus & calibration

The v9 GenAI corpus (#546) is the representative corpus for validating this
RFC's miner and reconstruction behavior. Sequencing is unchanged from #546:
port already done (#547), capture v9 when the demo releases, freeze, then a
v9 calibration manifest (RFC 0024) that now accounts for `event_name`-keyed
structured templates (§3.1). Until v9 exists, acceptance runs on a synthetic
GenAI-shaped fixture (structured `input.messages`/`output.messages` bodies,
`event_name` set, `gen_ai.*` attributes) checked into `testdata/`.

**A real, available-now AI-agent source: Claude Code's OTLP export.** Claude
Code — the agent authoring this project — can export OpenTelemetry over OTLP
(metrics plus log events carrying token usage, cost, and tool activity). It is
therefore a genuine, self-hosted AI-agent telemetry stream we can point at
Ourios today, ahead of the v9 demo release, and it fits the existing dogfood
posture (Ourios already exports its own logs via the OTLP bridge).

Claude Code emits its *own* event schema (its namespace, not the `gen_ai.*`
semconv), so *raw* it exercises the structured-event **shape** — event-heavy,
token-bearing, wordy — more than the exact promoted keys of §3.5. But that gap
closes upstream, **where it belongs**: an intermediary OpenTelemetry Collector
running OTTL (a `transform` processor, or the purpose-built `gen_ai_normalizer`
processor) rewrites `claude_code.*` events into the `gen_ai.*` semconv before
they reach Ourios. This is not a workaround — it is the architecturally correct
placement. Ourios's OTLP-only stance is that schema normalization is the
Collector's job, never an in-product parser; doing it in OTTL keeps that
boundary clean and turns the dogfood stream into a **`gen_ai.*`-conforming**
corpus that validates §3.3's promoted keys, not merely the shape. It reuses the
existing collector-interop harness (real `otelcol-contrib` → Ourios). Treated
as an *additional* corpus, not a replacement for the v9 capture — the demo
gives us instrumentation-native `gen_ai.*` with no transform in the path, which
is the cleaner validation of the promotion set; the Claude Code stream gives us
real AI-agent data now.

### 3.5 Recommended promotion set — and why not a GenAI vertical slice

A **true GenAI vertical slice** (a separate ingest path + a `gen_ai`-typed
Parquet schema + a GenAI-specific query surface) was considered and
**rejected**. The attraction of a slice is typed, prunable columns for the
scalar GenAI fields; but promotion (RFC 0022) already delivers exactly that
as additive per-deployment config, so a slice would fork the write path,
schema, compaction/retention, and every cross-cutting concern (tenancy §3.7,
WAL §3.4, object-storage truth §3.6) — two products in one binary — while
pinning an on-disk schema to a `development`-stability, freshly-relocated
semconv (CLAUDE.md §3.5 caution). The residual work to get the slice's
*value* is small: carry the last few promotions the GenAI SIG has already
identified as the correct low-cardinality dimensions, plus the three deltas
in §3.1–§3.3. This RFC takes that path.

The recommended default promotion set mirrors the SIG's own metric-dimension
guidance — "safe as a metric dimension" is precisely "low-cardinality, safe
to promote and group," and the same source says to keep prompt/completion
text and user IDs as *payloads*, not dimensions:

- **Promote (low-cardinality, group- and filter-friendly):**
  `gen_ai.operation.name` (enum: `chat`, `embeddings`, `execute_tool`,
  `invoke_agent`, …), `gen_ai.provider.name` (enum: `openai`, `anthropic`,
  `aws.bedrock`, …), `gen_ai.request.model`, `gen_ai.response.model`,
  `gen_ai.output.type` (enum), and for agent workloads `gen_ai.agent.name`,
  `gen_ai.tool.name`, `gen_ai.tool.type`. These are the natural
  `count … by` keys enabled by §3.3.
- **Promote for *filtering* only, never group:** `gen_ai.conversation.id`,
  `gen_ai.response.id` — high cardinality. Promotion gives a pruned equality
  filter ("find this one conversation"); grouping by them is nonsense. A
  reminder that §3.3's "group by any promoted column" needs operator
  judgement — promotable ≠ groupable.
- **Never promote (body payload):** `gen_ai.input.messages`,
  `gen_ai.output.messages`, `gen_ai.system_instructions`,
  `gen_ai.tool.call.arguments`/`result`, `gen_ai.retrieval.documents` — the
  content. Sensitive (the SIG flags PII), unique, large → structured body,
  filter-by-scan at most.

Two boundaries this set makes explicit:

- **Promotion is verbatim projection + dictionary encoding, not template
  mining.** An attribute value is already the isolated discrete token that
  mining exists to *extract* from an unstructured body; it has no
  constant-plus-variable structure to collapse. Low-cardinality repetition
  is captured by the promoted column's dictionary encoding (RFC 0022), which
  is the physical analogue of what a `template_id` is for a body. The miner
  is never invoked on attribute values.
- **Token counts are measures, not dimensions — and summing them is out of
  scope (CLAUDE.md §1).** `gen_ai.usage.input_tokens` / `output_tokens` are
  integers you would *sum* or take percentiles of, not group by; and
  promotion projects string values only, so they are not promotion targets
  regardless. The headline "tokens/cost **by** model per hour" query is a
  numeric *measure aggregation* (SUM/AVG/percentile), which is the province
  of the OTel `gen_ai.client.token.usage` **metric**, not a logs backend.
  Ourios lets an operator *find and filter* by token count and *count
  events* by model (the L4 frequency class); it deliberately does **not**
  sum tokens. This boundary keeps §1 ("not a metrics backend") intact.

## 4. Alternatives considered

- **Mine the structured body (walk the AnyValue tree, extract inner fields
  as params).** Rejected: it re-derives what the emitter already
  structured, invites semantic template merges across message shapes
  (hazard #1), and couples our schema to a fast-moving semconv. Storing the
  canonical tree whole and promoting the few *scalar* attributes that
  matter is both safer and more faithful.
- **A dedicated `gen_ai`-typed body column / sub-schema, or a full GenAI
  vertical slice.** Rejected as premature schema commitment (CLAUDE.md
  §3.5) and a fork of the "one stack, thin glue" thesis (§2); see §3.5 for
  the full rationale. `body_kind = Structured` + `event_name` + the
  recommended promoted scalar set already give queryability without pinning
  the on-disk format to a `development`-stability, freshly-moved semconv.
- **Truncate large structured bodies by default.** Rejected: violates
  §3.3. Fidelity is unconditional (§3.2, Option A).
- **Opt-in `structured_body_byte_limit` with a lossy flag (former
  Option B).** Considered and **rejected** (maintainer, 2026-07-22): even as
  an off-by-default knob it adds a lossy structured-body code path and a
  reconstruction branch for a case the metric + per-service alert (§3.2)
  already surface at the source. The emitter, not the store, is where an
  oversized payload is fixed; a store-side cap trades the operator's payload
  for bytes that RFC 0036's storage-layer sizing already bounds.
- **Group by non-promoted attributes too (JSON `LIKE` group key).**
  Rejected: no row-group pruning, unbounded scan, silently expensive —
  exactly the DSL footgun hazard #6 warns against. Promotion is the gate.
- **A separate RFC per delta.** The three deltas share one workload, one
  corpus, and one test fixture; splitting them triples process overhead for
  changes that land together. Gap 3 is the most separable and could be
  peeled off if review prefers.

## 5. Acceptance criteria (frozen 2026-07-23)

One scenario per invariant/hazard touched; each id is referenced from the
test code so the mapping is greppable (`docs/verification.md` §2):

> **Scenario RFC0037.1 — event-keyed structured templates (§3.1).**
> - **Given** two structured records in one `(tenant, severity_number,
>   scope_name)` whose `event_name`s differ
> - **When** they are mined
> - **Then** they receive distinct `template_id`s
> - **And** `… | count by template_id` separates them into distinct groups.

> **Scenario RFC0037.2 — structured-body reconstruction fidelity (§3.3
> invariant).**
> - **Given** a structured GenAI body (an `AnyValue` array/kvlist)
> - **When** it is stored and rendered back from Parquet
> - **Then** the canonical JSON round-trips byte-for-byte
> - **And** `lossy_flag = false` (a property test over generated bodies).

> **Scenario RFC0037.3 — unbounded fidelity + observability (§3.2 /
> hazard #2).**
> - **Given** an arbitrarily large structured body
> - **When** it is mined and stored
> - **Then** it round-trips byte-for-byte and is never truncated
>   (`lossy_flag = false`)
> - **And** the `structured_body_bytes` metric observes its canonical-JSON
>   length, dimensioned by service.

> **Scenario RFC0037.4 — grouped count by a promoted attribute (§3.3 /
> hazard #6).**
> - **Given** `gen_ai.request.model` promoted to a column
> - **When** `… | count by attr.gen_ai.request.model, bucket(1h)` runs
> - **Then** the `(bucket, model) → count` map equals a brute-force baseline
> - **And** the same query against a *non-promoted* key is rejected with a
>   promotion hint (never a silent unpruned scan).

> **Scenario RFC0037.5 — absent-body event parity (§3.5 / RFC 0025).**
> - **Given** an event record with `event_name` and attributes but no body
> - **When** it is stored
> - **Then** `body_kind = Absent` and the `body` cell is `NULL`
> - **And** RFC 0025's absent-body read-path parity is unbroken.

## 6. Testing strategy

Each scenario maps to a greppable test (`docs/verification.md` §2):

- **RFC0037.1 (§3.1):** `ourios-miner` `cluster.rs`
  `rfc0037_1_event_name_distinguishes_structured_templates` (example) and
  `rfc0037_1_structured_key_is_the_whole_template_identity` (`proptest` over
  arbitrary `(severity, scope, event_name)` tuples), plus
  `snapshot.rs` `structured_template_record_without_event_name_restores_as_none`
  (the `#[serde(default)]` migration).
- **RFC0037.2 (structured-body reconstruction):** RFC 0024's `ourios-*`
  property tests generate structured `AnyValue` bodies and assert they
  round-trip **canonical-JSON equal** — `canonical::decode_any_value` on the
  rebuilt bytes equals the original `AnyValue` (decoded value equality;
  string bodies round-trip bit-identically, structured bodies by decoded
  equality). **Byte-for-byte** retention of the stored canonical JSON is
  pinned directly by `rfc0037_3_structured_body_retained_byte_for_byte`
  (slice B). Plus the miner RFC0001.9 canonical-body round-trip
  (`rfc_internal.rs`) and the Parquet Structured-row round-trips in
  `rfc0025_absent_body.rs` / `rfc0021_arrow_upgrade.rs`.
- **RFC0037.3 (§3.2 fidelity + observability):** `ourios-miner` `cluster.rs`
  `rfc0037_3_structured_body_retained_byte_for_byte` (colocated unit, byte
  identity + non-lossy) and `tests/rfc0037_structured_body.rs`
  `rfc0037_3_structured_body_unbounded_fidelity_and_observability` (the
  `structured_body_bytes` histogram records the canonical-JSON length under
  the required `ourios.tenant` + `ourios.service` attribute set, via the
  in-memory meter).
- **RFC0037.4 (§3.3 grouped count):** `ourios-querier`
  `tests/it/rfc0002_dsl.rs` `rfc0037_4_count_by_promoted_attribute` — the
  `(bucket, model) → count` map equals a brute-force oracle (RFC 0031 L4
  shape); the non-promoted key is rejected with a hint naming the raw config
  key + sublist.
- **RFC0037.5 (absent-body parity):** covered by RFC 0025's
  `rfc0025_absent_body.rs` (`body_kind = Absent`, `body` cell `NULL`).
- **Calibration (deferred to v9):** RFC 0024 manifest regenerated; C1/C2
  per-service reconstruction over the GenAI corpus. This is the remaining
  work for `validated`.

## 7. Open questions

- [x] **The fork (§3.2):** ~~cap or no cap?~~ **Resolved 2026-07-22 —
      Option A** (full fidelity + `structured_body_bytes` metric + alert; no
      cap). Former Option B rejected (§4).
- [x] **`event_name` in the string-path template key?** **No** — scoped to
      the structured path only. String log records rarely carry
      `event_name`, and keeping the string Drain key unchanged minimises the
      calibration re-base and the blast radius. (Default decision; reversible
      if a corpus shows string-path event records in practice.)
- [x] **Gap 3 (group-by promoted attribute) in-scope here?** **Yes** — it is
      the queryability payoff for the promoted `gen_ai.*` columns this RFC
      motivates, and it reuses the RFC 0031 L4 unit rather than new
      machinery. It remains the most separable slice if review prefers to
      peel it into an RFC 0002 amendment.
- [x] **Implicit promotion of any `gen_ai.*` keys?** **No** — always
      deployment config (`storage.promoted_attributes`). Implicit promotion
      would pin the schema to a `development`-stability, recently-moved
      semconv (invariant §3.5 caution).
- [ ] **v9 timing (sequencing, not a fork):** this RFC reaches `green` on the
      synthetic GenAI fixture (§3.4) and only `validated` once the demo
      releases and `corpus/otel-demo-v9` is captured (#546).

## 8. References

- OpenTelemetry GenAI semantic conventions (moved to
  `open-telemetry/semantic-conventions-genai`); `gen_ai.input.messages` /
  `gen_ai.output.messages` / `gen_ai.client.inference.operation.details`
  event; `OTEL_SEMCONV_STABILITY_OPT_IN=gen_ai_latest_experimental`.
- Issue #546 — `corpus/otel-demo-v9` readiness (k6 port #547; demo `agent`
  service).
- RFC 0001 (template miner; §6.1 structured-record invariants), RFC 0022
  (promoted attribute columns), RFC 0024 (calibration), RFC 0025
  (absent-body representation), RFC 0031 (L4 grouped-count comparison unit),
  RFC 0032 (query-schema resource).
- CLAUDE.md §1 (scope — logs only), §3.2 (parameter cardinality), §3.3
  (bit-identical reconstruction), §3.5 (schema migration caution); hazards
  #1, #2, #6.
