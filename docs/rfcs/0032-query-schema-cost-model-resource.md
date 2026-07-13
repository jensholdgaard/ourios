---
rfc: 0032
title: Query-schema and cost-model resource for the MCP surface (RFC 0027 amendment)
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-13
supersedes: —
superseded-by: —
---

# RFC 0032 — Query-schema and cost-model resource for the MCP surface

## 1. Summary

Add a second MCP **resource** — `ourios://query-schema` — beside the
RFC 0027 grammar resource, carrying the stored-log **field
vocabulary** and a **query-class cost model**: the fixed OTLP log
columns (including the OTel 1–24 severity scale the DSL's severity
names compile onto), the promoted attribute columns of *this
deployment* (the running `PromotedAttributes` set, RFC 0022), and a
structural classification of predicate kinds into cost tiers
(index-backed / pruned / scan). RFC 0027 is `accepted` (terminal), so
this lands as a new RFC amending it, à la RFC 0022/0023/0024:
read-only, additive, no new tool — the existing
`list_resources`/`read_resource` hook gains one resource. The grammar
resource already teaches an agent *how to write* a query;
`list_templates` gives the *body* vocabulary; this completes the set
with the *field* vocabulary and with which query shapes the backend
answers cheaply.

## 2. Motivation

- **Agents guess field names; the deployment knows them.** An agent
  can learn the DSL from `ourios://dsl-grammar`, but the grammar's
  `resource.<key>` / `attr.<key>` productions are open-ended — which
  keys exist as typed, prunable columns is per-deployment
  configuration (`storage.promoted_attributes`) that no client can
  guess. OTel field practice supports the claim: an OTel-native
  vendor doing Drain-style mining reports that handing an agent
  semantic-convention + resource-attribute context makes it "way
  better at writing correct queries on the first try" (OTel Night
  Berlin 2025, sig-end-user transcript). Issue #465 is the scoping
  record.
- **The severity scale is a first-try failure mode.** `severity >=
  error` only works if the client knows severity is the numeric OTel
  `SeverityNumber` and the names are four-wide bands
  (`error` → 17..=20). That mapping is Ourios's documented choice
  (RFC 0002 §6.1, `SeverityName::floor`/`ceil`); it belongs in the
  protocol, not in each consumer's prompt.
- **The cost model is structural, and agents can exploit it.** The
  RFC 0031 comparative program (§9.13, epic #498) demonstrated the
  durable shape of Ourios query cost: exact-id lookups and promoted
  equality prune to a handful of row groups, time windows prune by
  statistics, body-substring browses scan. That tiering is true by
  construction — it follows from which columns the writer
  bloom-filters and page-indexes — so it can be published as
  *structure* without ever publishing benchmark numbers (which rot).
  A consumer that knows `trace_id == …` is cheap and
  `contains(body, …)` is a scan writes better queries at zero
  per-query cost to the backend, and is steered onto the backend's
  structural strengths (pillar #1/#2, `CLAUDE.md` §2).
- **Cheap by construction.** The `list_resources`/`read_resource`
  hook, the config plumbing (`storage.promoted_attributes` is already
  resolved into a `PromotedAttributes` at startup,
  `ourios-server/src/main.rs`), and the tier facts (which columns are
  bloomed, `ourios-parquet/src/writer.rs`) all exist. The new work is
  one JSON document and threading the promoted set into the querier
  role's MCP handler.

## 3. Proposed design

### 3.1 Placement

- A second `Resource` in the RFC 0027 module
  (`crates/ourios-server/src/mcp.rs`), URI **`ourios://query-schema`**,
  MIME `application/json`. `list_resources` returns both resources;
  `read_resource` dispatches on URI.
- The querier role today does not receive the resolved
  `PromotedAttributes` (only the receiver's write path does). The
  server threads `config.promoted` into `mcp_router` →
  `OuriosMcp`, and the resource document is built **once at role
  startup** from that set — configuration is startup-static (RFC
  0020), so the document is immutable for the process lifetime, like
  the grammar section.
- **Read-only contract untouched**: no new tool, no write, no
  tenant-scoped data. The document derives exclusively from static
  configuration and compiled-in schema facts — never from ingested
  telemetry, so it is the one MCP payload that carries no
  untrusted-content caveat.

### 3.2 The document

The resource body is one versioned JSON object (`format_version`
evolution hook per the RFC 0033 precedent: consumers treat an unknown
version as "fetch nothing, fall back to the grammar + docs"):

```json
{
  "format_version": 1,
  "fields": [
    { "name": "ts",          "type": "timestamp" },
    { "name": "observed_ts", "type": "timestamp" },
    { "name": "severity",    "type": "integer" },
    { "name": "body",        "type": "string" },
    { "name": "trace_id",    "type": "hex_string" },
    { "name": "span_id",     "type": "hex_string" },
    { "name": "scope",       "type": "string" },
    { "name": "flags",       "type": "integer" },
    { "name": "service",     "type": "string" },
    { "name": "template_id", "type": "integer" },
    { "name": "confidence",  "type": "float" },
    { "name": "lossy",       "type": "boolean" }
  ],
  "severity": {
    "comparison": "numeric, OTel SeverityNumber 1-24",
    "names": [
      { "name": "trace", "floor": 1,  "ceil": 4 },
      { "name": "debug", "floor": 5,  "ceil": 8 },
      { "name": "info",  "floor": 9,  "ceil": 12 },
      { "name": "warn",  "floor": 13, "ceil": 16 },
      { "name": "error", "floor": 17, "ceil": 20 },
      { "name": "fatal", "floor": 21, "ceil": 24 }
    ]
  },
  "promoted_attributes": {
    "resource": ["service.name", "k8s.namespace.name"],
    "log": ["http.route"]
  },
  "cost_model": {
    "tiers": ["index_backed", "pruned", "scan"],
    "classification": [
      { "kind": "exact_equality", "fields": ["trace_id", "span_id", "template_id"],
        "tier": "index_backed", "mechanism": "bloom" },
      { "kind": "ordering_or_equality", "fields": ["severity"],
        "tier": "index_backed", "mechanism": "statistics" },
      { "kind": "promoted_attribute_equality",
        "fields": ["service", "resource.<promoted key>", "attr.<promoted key>"],
        "tier": "index_backed", "mechanism": "bloom" },
      { "kind": "time_window", "fields": ["ts", "observed_ts"],
        "tier": "pruned", "mechanism": "statistics" },
      { "kind": "non_promoted_attribute_predicate",
        "fields": ["resource.<other key>", "attr.<other key>"],
        "tier": "scan" },
      { "kind": "body_substring_or_regex", "fields": ["body"],
        "tier": "scan" },
      { "kind": "unscoped_browse", "fields": [],
        "tier": "scan" }
    ]
  }
}
```

Normative content rules:

- **`fields`** — exactly the RFC 0002 §7 `field` production (the DSL
  surface, not the raw Parquet schema; hazard §4.6 — the resource
  must not leak storage columns the DSL does not expose). Each entry
  MAY carry a short `description` string; the shape above is the
  minimum.
- **`severity`** — the six names with their `floor`/`ceil` bands MUST
  equal the DSL's `SeverityName::floor`/`ceil` mapping
  (`crates/ourios-querier/src/dsl/ir.rs`): ordering comparisons use
  the floor, equality tests the band. This is the resource's answer
  to "how do I write `severity >= ERROR`".
- **`promoted_attributes`** — the *effective* running set from the
  threaded `PromotedAttributes` (`resource_keys()` / `log_keys()`):
  `service.name` always present and first, configured keys after, in
  the deduplicated config order. This is the per-deployment half an
  agent cannot guess, and it is what makes the `cost_model`
  deployment-specific: `promoted_attribute_equality` is index-backed
  *for exactly these keys, in this instance*; the same predicate on
  any other key is `non_promoted_attribute_predicate` (the RFC 0022
  §3.3 JSON-`LIKE` fallback — correct, unpruned).
- **`cost_model`** — structure only, **never numbers**: no
  latencies, no byte counts, no ratios. The tier facts are true by
  construction of the writer:
  - `bloom` mechanism entries correspond one-to-one to the columns
    `writer.rs` actually bloom-filters today: `template_id` (RFC
    0005 §3.6), `trace_id`/`span_id` (the RFC 0031 L3 fix), and
    every promoted attribute column (RFC 0022 §3.1).
  - `severity` carries **no bloom filter** — its predicates prune
    through min/max page statistics (ordinal data, where statistics
    are the right index); the resource says `statistics`, not
    `bloom`, because claiming index-backing that the writer does not
    provide is exactly the drift RFC0032.4 gates against.
  - `time_window` is the `range(t1, t2)` stage pruning on the time
    columns' statistics; `unscoped_browse` (no `range` stage beyond
    the default look-back) and body substring/regex predicates are
    scans — *expensive, still correct*.

### 3.3 Tool-description placement rule

Each of the three RFC 0027 tool descriptions gains **one advisory
sentence** pointing at the resource, e.g. for `query_logs`: *"Read
the ourios://query-schema resource first for the queryable fields,
the severity scale, and which predicates are index-backed."* The full
tiering lives **only** in the machine-readable resource — tool
descriptions are prompt real estate in every client context, and the
tiers would otherwise be paraphrased into prose that drifts. One
pointer, one source of truth.

### 3.4 What this RFC does not change

No Parquet schema change, no DSL change, no new tool, no new crate,
no change to any RFC 0027 tool's arguments or output. The RFC 0027 §5
suite must pass verbatim after this lands (RFC0032.6).

## 4. Alternatives considered

- **Static-only resource** (fixed columns + severity scale, no
  config plumbing — issue #465's first fork). Trivial to ship, but
  it omits exactly the half an agent cannot guess: which
  `resource.<key>`/`attr.<key>` predicates are typed, prunable
  columns *here*. Without the promoted set the cost model cannot be
  stated honestly either (promoted equality and non-promoted
  fallback land in different tiers). Rejected; the plumbing is one
  threaded value.
- **Put the schema in the tool descriptions.** Descriptions ship
  into every client's context on `tools/list`; a schema + cost table
  there is paid on every session and invites clients to treat prose
  as data. A resource is fetched on demand and machine-readable.
  Rejected — this RFC pins the one-advisory-sentence rule instead
  (§3.3).
- **Extend the grammar resource** instead of adding a second one.
  The grammar resource's contract is byte-identity with RFC 0002 §7
  (RFC0027.6) — appending deployment-specific JSON would break that
  invariant and mix a static doc with dynamic config. Rejected.
- **A `describe_schema` tool.** Tools imply arguments and per-call
  work; this content is constant per process and tenant-independent.
  MCP resources exist precisely for this. Rejected (also keeps the
  RFC 0027 deny-list — "exactly the §3.2 three tools" — intact).
- **Serve `ourios-semconv` names.** Wrong vocabulary: that crate
  holds Ourios's *own emitted-telemetry* names (how the backend
  describes itself), not the stored-log query surface (issue #465
  notes this explicitly). Rejected.
- **Include benchmark-derived cost numbers.** The RFC 0031 numbers
  are corpus- and channel-dependent and rot with every writer
  change; the tier *structure* is what is durable. Rejected — the
  cost model is structural by rule (§3.2).

## 5. Acceptance criteria

Scenario ids `RFC0032.<m>`, referenced from test code.

> **Scenario RFC0032.1 — listed and readable.** Given
> `querier.mcp.enabled`, When a client lists resources, Then exactly
> two resources are advertised — the RFC 0027 grammar resource and
> `ourios://query-schema` (`application/json`); When the client reads
> `ourios://query-schema`, Then the body parses as JSON with
> `format_version: 1` and carries the §3.2 top-level keys (`fields`,
> `severity`, `promoted_attributes`, `cost_model`); And `tools/list`
> still advertises exactly the RFC 0027 §3.2 three — no new tool.

> **Scenario RFC0032.2 — content matches the running config.** Given
> `storage.promoted_attributes` configured with resource and log
> keys, When the resource is read, Then `promoted_attributes` equals
> the effective `PromotedAttributes` set — `service.name` first,
> configured keys deduplicated in order; And with the section
> omitted, `promoted_attributes.resource` is `["service.name"]` and
> `.log` is empty; And two servers with different promoted sets serve
> different resource bodies (the per-deployment property).

> **Scenario RFC0032.3 — severity scale correctness.** Given the
> resource body, Then the `severity.names` entries equal the DSL's
> `SeverityName` mapping — for each of the six names, `floor` equals
> `SeverityName::floor` and `ceil` equals `SeverityName::ceil` — the
> test asserts against the `ourios-querier` functions, not repeated
> literals, so the resource cannot drift from the compiler.

> **Scenario RFC0032.4 — cost-tier classification stability.** Given
> the resource body, Then every `cost_model.classification` entry
> with `mechanism: "bloom"` names only columns the writer actually
> bloom-filters — the test derives the expected set from the writer's
> properties for the configured `PromotedAttributes` (`template_id`,
> `trace_id`, `span_id`, and every `PromotedAttributes::column_names`
> column) and asserts the resource's index-backed equality kinds
> cover exactly the DSL fields backed by that set; And `severity`'s
> entry carries `mechanism: "statistics"`, never `"bloom"`; And no
> classification entry carries a numeric cost value (structure,
> never numbers).

> **Scenario RFC0032.5 — tool-description placement.** Given
> `tools/list`, Then each of `query_logs`, `list_templates`, and
> `template_drift` carries exactly one advisory sentence naming
> `ourios://query-schema`, And no tool description enumerates tiers,
> severity bands, or promoted keys (the full tiering lives only in
> the resource).

> **Scenario RFC0032.6 — read-only contract preserved.** Given the
> amendment applied, Then the RFC 0027 §5 suite passes verbatim
> (same tools, same outputs, grammar resource byte-identical), And
> reading `ourios://query-schema` performs no query, touches no
> tenant data, and its body contains no ingested-telemetry-derived
> content; And an unknown resource URI still returns the
> resource-not-found error.

## 6. Testing strategy

Mapped to `CLAUDE.md` §6.2:

- **RFC0032.1/.2/.5/.6** — integration tests in
  `crates/ourios-server/tests/it/rfc0027_mcp.rs`'s harness shape
  (in-process router, MCP JSON-RPC over `/mcp`): `resources/list`,
  `resources/read`, `tools/list` against servers built with distinct
  `storage.promoted_attributes` configs; `.6` additionally re-runs
  the existing RFC 0027 suite untouched (tests are specifications —
  none may be weakened).
- **RFC0032.3/.4** — unit tests beside the resource builder in
  `mcp.rs`, asserting against `SeverityName::floor`/`ceil` and
  against the writer-properties bloom set derived from the same
  `PromotedAttributes` value, so both halves of the document are
  pinned to the code they describe rather than to literals.
- At validation, the RFC 0027 §5.2 precedent applies: the official
  MCP inspector CLI (an independent client) lists and reads the
  resource against the served release binary, extending
  `scratch/validation/rfc0026-0027-validate.sh`.

## 7. Open questions

- [ ] **Template-vocabulary hints.** Should the resource carry a
  pointer at (or a sample of) the template vocabulary, or does
  `list_templates` already cover the body-shape half cleanly? Current
  position: the resource stays tenant-independent and static per
  process; templates are per-tenant, queryable data and belong to the
  tool. Confirm before green.
- [ ] **Config reload.** Configuration is startup-static today, so
  the document is built once. If a future RFC makes
  `storage.promoted_attributes` reloadable, the resource must follow
  and MCP `listChanged`/subscription semantics become relevant —
  out of scope here, but the once-at-startup build is the assumption
  to revisit.
- [ ] **`severity_text` exposure.** The stored schema carries
  `severity_text`, but the DSL deliberately compares on the numeric
  scale (RFC 0002 §6.1). If the DSL ever exposes it, the resource's
  `fields` follows the grammar automatically — noting so the two
  don't drift silently.
- [ ] **Tier vocabulary stability.** `index_backed`/`pruned`/`scan`
  are this RFC's names; if RFC 0031's docs settle on different
  public terminology for the query classes, align before green
  (renames after clients consume the resource cost a
  `format_version` bump).

## 8. References

- Issue #465 — the scoping record, including the 2026-07-13
  maintainer comment adding the query-class cost model and the
  placement rule.
- RFC 0027 — the MCP query surface this RFC amends (`accepted`,
  terminal); §3.2 resource precedent, §5.2 inspector-validation
  precedent.
- RFC 0022 — promoted attribute columns: `PromotedAttributes`,
  `storage.promoted_attributes`, the promoted-vs-fallback compile
  split the cost model encodes
  (`crates/ourios-parquet/src/promoted.rs`).
- RFC 0002 §6.1/§7 — the DSL field surface and the severity
  name→number choice (`crates/ourios-querier/src/dsl/ir.rs`,
  `SeverityName`).
- RFC 0005 §3.6 / RFC 0031 (L3, trace-context blooms) — the bloom
  set the tiers rest on (`crates/ourios-parquet/src/writer.rs`).
- RFC 0033 §3.2 — the `format_version` evolution-hook precedent for
  small versioned JSON artifacts.
- OTel Logs Data Model — `SeverityNumber` 1–24 and the
  compare-on-number mandate.
- OTel sig-end-user, OTel Night Berlin 2025 transcript — the
  schema-context-for-agents motivation.
- `CLAUDE.md` §2 (pillars #1/#2 — the pruning structure being
  published), §4.6 (DSL vs engine leakage — the resource describes
  the DSL surface only), §3.7 (tenancy — the resource is
  tenant-independent by design).
