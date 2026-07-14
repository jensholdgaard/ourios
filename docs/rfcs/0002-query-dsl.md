---
rfc: 0002
title: Query DSL — the Ourios logs query language (Branch B, surface β)
status: green
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-04-24
supersedes: —
superseded-by: —
---

# RFC 0002 — Query DSL

> **Status note.** The prior decision (§3) is **resolved**: the predicate
> sublanguage takes **Branch B** (distance from OTTL), on the
> **β** (pipe-composable) top-level surface. Decided 2026-06-07 from the
> audience analysis in §3.6 (primary: Perses dashboard authors; future:
> MCP agents). This RFC is now **`green`** — all 11 §5 acceptance
> criteria (RFC0002.1–.11) have passing tests
> (`crates/ourios-querier/tests/rfc0002_dsl.rs`), landed across PRs
> #143 (this spec) and #144–#154 (the red gate + implementation): the
> Branch-B parser + structured surface → one IR, the
> IR→DataFusion compile, YAML-embeddability + the structured JSON Schema,
> and `resolves_to` alias-set expansion via the RFC 0001 §6.7 operator
> alias map. §6 gives the design, §7 the grammar, §5 the criteria. Per the
> `docs/rfcs/README.md` ladder, `validated` and finally `accepted` (a
> maintainer flip) follow the §9 validation.
> Hazard 6 (`CLAUDE.md` §4 — no DataFusion/SQL leakage) constrains the
> whole design.
> **Amendment 2026-07-15 (RFC 0031 L4):** §6.3/§7 gain the `param(n)`
> and `bucket(width)` aggregation group terms (grammar **v1.1**, a §6.6
> minor version) and §5 gains **RFC0002.12–.16** (aggregation
> execution). The `green` above refers to RFC0002.1–.11; the new
> scenarios enter **red** (`#[ignore]`'d stubs, the
> `docs/verification.md` §3 two-loop) via the next PR and are
> discharged by the implementing slices.

## 1. Summary

Ourios exposes a logs query DSL that does **not** leak DataFusion/SQL to
users (`CLAUDE.md` §4 hazard 6). This RFC specifies it:

- **Predicate sublanguage — Branch B (distance from OTTL).** An
  Ourios-native, query-ergonomic syntax over the OTel *data model* (the
  ingest contract): bare top-level fields (`body`, `severity`,
  `trace_id`), `resource.` / `attr.` prefixes, bare-identifier severity
  (`severity >= error`), first-class template + OTel-canonical primitives.
- **Top-level surface — β (pipe-composable).** A predicate followed by
  pipe stages: `… | range(-1h, now) | count by template_id | sort count
  desc | limit 10`. Compact, single-line, and embeddable as a YAML scalar
  in Perses dashboards.
- **Two front-ends, one core.** The string DSL (for humans, esp. Perses
  YAML) and a **structured JSON surface** (for MCP agents + programmatic
  clients) parse to the same query IR and compile to the same DataFusion
  `LogicalPlan`. Agents emit JSON, not syntax.

The design rests on `ourios-querier` (RFC 0007), whose execution layer
(predicate pushdown, tenant isolation, `QueryStats`) is already
implemented and tested (RFC 0007 §5 criteria all live; the RFC itself
stays `specified` pending this DSL); this RFC adds the *user-facing
language* in front of it.

## 2. Motivation

### 2.1 Why a DSL at all?

`CLAUDE.md` §4 hazard 6 commits Ourios to a DSL that does not leak DataFusion SQL.
The reasons are **stability** (evolve the backend without breaking user
queries), **safety** (full SQL exposes cross-tenant joins, unbounded
scans, recursive CTEs we cannot audit), and **fit** (logs are a narrow
domain; a narrow DSL is more ergonomic than a general one). This is
branch-agnostic.

### 2.2 Why the prior decision mattered

"OTTL-inspired" was not a free decision. Borrowing OTTL syntax in a query
context promises OTTL-literate users that their mental model transfers; if
the syntax looks the same but behaves differently (OTTL mutates; a query
filters), the surface actively misleads. §3 records how that decision was
made.

## 3. The prior decision (resolved): distance from OTTL

Both positions were defensible; §3.1–3.4 keep the honest case for each for
the record. §3.5 records the resolution; §3.6 the reasoning.

### 3.1 The case for borrowing (Branch A) — not chosen

- **Positive transfer for Collector-literate SREs.** Engineers who write
  Collector/OTTL pipelines reuse that mental model at zero onboarding.
- **Reduces bikeshed surface.** A pinned external spec inherits decisions
  rather than re-litigating them.
- **Ecosystem alignment.** Diverging on surface syntax in the OTel orbit
  can read as gratuitous.
- **OTTL's path grammar is correct about the data model**, which any
  alternative must address anyway.

### 3.2 The case for distancing (Branch B) — chosen

- **The OTTL-literate population is a minority of OTLP users** — most emit
  logs via an SDK and never touch OTTL.
- **Collector ergonomics become query verbosity.**
  `resource.attributes["service.name"] == "api"` is loud in a query.
- **Shared syntax + different semantics misleads.** Unfamiliar syntax is a
  safer failure mode than almost-familiar-but-wrong.
- **No evolving external spec to track** (OTTL has had breaking changes).
- **Design freedom** for query-context idioms (`severity >= error`,
  `attr.foo`).

### 3.3 What is shared regardless of branch

- The OTel **data model** is the schema of log records (the ingest
  contract, not a design choice): attributes, resources, severity, body,
  timestamps, trace context.
- The **template + correctness primitives** (`template_id`, `confidence`,
  `lossy`; drift-alias membership via `resolves_to`) are first-class
  (§6.3).
- The **compilation target** is a DataFusion `LogicalPlan`, no SQL leakage
  (§6.5).

### 3.4 Consequences

| Dimension | Branch A (borrow) | Branch B (distance) |
|---|---|---|
| Onboarding for Collector-literate SREs | Near-zero | Mild (new syntax, familiar semantics) |
| Onboarding for SDK / dashboard users | Same (OTel data model) | Same |
| Maintenance cost | Track pinned OTTL, amend on bumps | Own the grammar |
| Same-syntax/different-meaning confusion | Real | Avoided |
| Spec size | Smaller | Larger (owned) |
| Ecosystem signalling | Aligned with OTel | Independent (in the data model: still aligned) |
| Design freedom | Constrained by OTTL | Free within the OTel data model |

### 3.5 Resolution

**Branch B (distance from OTTL), surface β (pipe-composable).** Decided
2026-06-07 by the maintainer, on the audience analysis in §3.6 in lieu of
the formal user research originally gated here (§9 now scopes that
research to the `accepted` gate, not `specified`).

### 3.6 Why — the two audiences

The decision turns on two audiences that re-weight §3.1–3.4:

1. **Primary — Perses dashboard authors (declarative YAML/CRDs).** Queries
   live as **string scalars in versioned YAML**. Brevity and low
   bracket/quote density win (readable scalars, clean diffs); the audience
   thinks in *dashboard* query languages (PromQL/LogQL), not OTTL. Branch
   B's flat syntax + the β pipe surface embed cleanly on one line; Branch A
   on surface α would be multi-line and bracket-heavy.
2. **Future — MCP agents.** Borrow-but-diverge (Branch A + the §6
   divergence list it required) is the **worst case for LLMs**: strong
   public-OTTL priors pull a model toward real-OTTL constructs we do not
   support → plausible-but-invalid queries. A small, self-owned grammar
   (Branch B) has no priors to fight, is cheaper to embed in an MCP tool
   schema, and is enforceable with grammar-constrained decoding. And —
   decisively — agents need not generate syntax at all: they target the
   **structured surface** (§6.4).

The one strong case for Branch A (onboarding + signalling for
Collector-literate SREs) lands on the audience that is *not* primary here,
while its costs (semantic-confusion in the overlap zone; an
externally-driven breaking cadence against long-lived dashboards + cached
agent schemas) land squarely on these two. Distancing on *surface syntax*
costs little ecosystem goodwill because we stay faithful to the OTel
**data model** (§3.3) and because bespoke query syntax is the norm (LogQL,
PromQL, CloudWatch Insights all diverge from any transformation language).

The full audience analysis is the drafting-assistance recommendation that
informed this decision; its three OTel-ecosystem questions (is there an
OTel *query* language to align with? is OTTL the expected *querying*
surface? Perses+OTel query conventions?) are folded into §9.

## 4. Design principles

1. **Familiarity beats cleverness.** A first-time reader understands a
   query within 30 seconds without a reference. No heavy sigils.
2. **No DataFusion/SQL leakage** (`CLAUDE.md` §4 hazard 6). If explaining a surface
   form requires naming a DataFusion type, the form is wrong.
3. **Predicate, then pipeline.** A query is a *predicate* (the `where`)
   followed by ordered *stages* (range, aggregate, sort, limit, project).
   Each reads independently.
4. **Template + OTel-canonical fields are first-class vocabulary**, not
   pseudo-columns: `template_id`, `confidence`, `lossy` (drift-alias
   membership via `resolves_to`);
   `service`, `trace_id`, `span_id`, `scope` (the primary
   correlation/query dimensions per OTel maintainer guidance, §6.2).
5. **Every query has a time range** — explicit `range(...)` or a
   tenant-configurable default window. Never an unbounded scan.
6. **One core, two surfaces.** The string DSL and the structured surface
   are equivalent front-ends over one IR (§6.4); neither can express a
   query the other cannot.
7. **YAML-embeddable.** A query is expressible as a single-line scalar
   that survives a YAML round-trip — a first-class constraint for the
   Perses audience, not an afterthought.
8. **The grammar is owned and versioned by this RFC** (§7), not "inspired
   by" anything. Compatibility pledges are written, not implied.

## 5. Acceptance criteria

> `Given/When/Then`, ids greppable from tests: each test carries the
> `docs/verification.md` §2.2 doc-comment form —
> `/// Scenario RFC0002.<n> — <title>.` plus
> `/// See docs/rfcs/0002-query-dsl.md §5.`. These specify the parser +
> compiler that front-ends the (already-implemented, RFC 0007 §5)
> execution layer.

- **RFC0002.1 — A Branch-B predicate parses and compiles to a filter `[CLAUDE.md §4 hazard 6]`**
  - **Given** a Branch-B predicate (e.g. `template_id == 42 and severity >= error`)
  - **When** it is parsed and compiled
  - **Then** it yields the query IR and an **internal** DataFusion
    `Filter` (a private compilation artifact — never surfaced through the
    public API, RFC0002.3). Predicates
    over RFC 0007 §4.3's pushdown keys prune the scan per that section's
    split — `template_id` skips row groups (B1), `time_unix_nano` prunes
    partitions and row groups, `tenant_id` prunes partition directories
    (not row groups); for the subset the current `ourios_querier`
    structured request can express (template + time) the DSL result is
    identical to it. Severity compiles via the §6.2/RFC0002.5
    `severity_number` mapping (the column is RFC 0005's `severity_number`),
    not the `severity_text` equality the current request supports, and
    predicates
    over non-indexed fields (`service`, `attr.*`) compile to a correct
    `Filter` with no row-group-pruning claim (indexed `service.name`
    pushdown would be a future RFC 0005 §3.6 amendment).

- **RFC0002.2 — String DSL and structured surface compile to the same plan `[§6.4]`**
  - **Given** a query expressed both as a β string and as the structured
    JSON surface
  - **When** both are compiled
  - **Then** they produce the *same* query IR (and hence the same
    `LogicalPlan`) — the one-core/two-surfaces invariant.

- **RFC0002.3 — No DataFusion/arrow/SQL leakage `[CLAUDE.md §4 hazard 6]`**
  - **Given** the public DSL API (parse, compile, error types)
  - **When** a query parses, compiles, or fails
  - **Then** no `datafusion`/`arrow`/SQL type or message appears in any
    public signature or error string (compile- and string-level boundary
    test, mirroring RFC0007.3).

- **RFC0002.4 — A query without an explicit range gets the tenant default window `[§4 P5]`**
  - **Given** a query with no `range(...)` stage
  - **When** it is compiled in a tenant context with a default window W
  - **Then** the plan carries a time-column filter equal to W — never an
    unbounded scan.

- **RFC0002.5 — Bare-identifier severity maps to its SeverityNumber `[§6.1]`**
  - **Given** `severity >= error` (and `warn`, `info`, `debug`, `trace`,
    `fatal`)
  - **When** compiled
  - **Then** each maps, case-insensitively, to the §6.1 `SeverityNumber`
    for that level (`error` → 17, etc.) and compiles identically to the
    numeric form (`severity >= 17`). The name→number mapping is the
    documented §6.1 one (Ourios's, aligned with the OTel ranges) — not an
    OTel-standardised threshold.

- **RFC0002.6 — First-class OTel-canonical fields resolve correctly `[§6.2]`**
  - **Given** `service`, `trace_id`, `span_id`, `scope` used as bare
    fields
  - **When** compiled
  - **Then** each resolves to the RFC 0001 §6.1 column / resource-attribute
    it names (`service` → `resource["service.name"]`), with no
    string-flattening required of the user.

- **RFC0002.7 — Parse/serialise round-trip is idempotent**
  - **Given** any well-formed query (property-generated)
  - **When** parsed → serialised → parsed
  - **Then** the second parse equals the first (AST idempotence).

- **RFC0002.8 — A malformed query yields a specific, leak-free error**
  - **Given** a syntactically or semantically invalid query
  - **When** parsed/compiled
  - **Then** it returns a specific error citing the offending
    token/clause and the §7 grammar — never a panic, never a DataFusion
    message.

- **RFC0002.9 — Template primitives compile `[§6.3]`**
  - **Given** `template_id == 42`, `resolves_to(42)`, `lossy == true`,
    `confidence < 0.7`
  - **When** compiled
  - **Then** each compiles to the documented plan (`resolves_to` expands
    to the alias-set membership of RFC 0001 §6.7), without leaking the
    underlying representation.

- **RFC0002.10 — A query is a YAML-safe single-line scalar `[§4 P7]`**
  - **Given** the canonical serialisation of any well-formed query
  - **When** embedded as a YAML scalar and round-tripped through a YAML
    parser
  - **Then** the recovered string parses to the same query (the Perses-
    embedding guarantee).

- **RFC0002.11 — The structured surface validates against its published schema `[§6.4]`**
  - **Given** the structured (MCP) query surface
  - **When** a request is validated against the published JSON schema
  - **Then** well-formed requests pass and compile; malformed ones are
    rejected by the schema before reaching the planner.

> **Amendment 2026-07-15 — aggregation execution criteria (RFC 0031
> L4).** RFC0002.12–.16 below are added together with the §6.3
> `param(n)` / `bucket(width)` group terms; they specify lifting the
> querier's explicit aggregation-stage rejection (today
> `ourios-querier` `compile::validate` rejects `count`/`agg_fn` stages
> as "not yet supported") for the `count` family. Per the
> `docs/verification.md` §3 two-loop they enter **red** via the next
> PR (`#[ignore]`'d stubs) and turn green in the implementing slices;
> the status note's `green` refers to RFC0002.1–.11. Execution of the
> `sum`/`min`/`max`/`avg` stages remains a later obligation — their
> stages keep the explicit rejection until a future amendment adds
> their criteria.

- **RFC0002.12 — `count [by …]` executes end-to-end and matches a naive oracle `[§6.5]`**
  - **Given** a populated tenant store and a query
    `<predicate> | range(…) | count by <field, …>` over ordinary §7
    fields (e.g. `template_id`, `service`) — and the bare `count`
    (no `by`)
  - **When** the querier executes it
  - **Then** the result is the `group_key → count` map (bare `count`:
    the single total) and equals a naive oracle computed outside the
    query path by filtering and counting the same rows — and the
    `count` stage is no longer rejected by `compile::validate`.

- **RFC0002.13 — `count by param(n), bucket(w)` yields the L4 grouped-count map `[§6.3 amendment; RFC0031.5]`**
  - **Given** a predicate that pins exactly one `template_id`
    (§6.3 amendment pinning rule) and the stage
    `count by param(0), bucket(5m)` over rows of that template
  - **When** the querier executes it
  - **Then** the result is the `(bucket, group_key) → count` map —
    buckets the half-open epoch-aligned windows
    `[k·w, (k+1)·w)` over the effective timestamp (§6.2 amendment
    2026-06-11), group keys the stored string form of params slot
    `0` — equal to a naive oracle, and shape-identical to the map
    RFC 0031 §3.5 / RFC0031.1 compares for L4 equivalence.

- **RFC0002.14 — `param(n)` misuse is a specific compile-time error `[§6.3 amendment]`**
  - **Given** (i) `service == "api" | count by param(0)` (no
    `template_id` pin), (ii)
    `template_id == 4 or template_id == 7 | count by param(0)` (a
    disjunction pins nothing), (iii)
    `resolves_to(4) | count by param(0)` (an alias *set*, not a pin),
    and (iv) `param(0)` outside a `by`-list (as a predicate path or a
    `project` field)
  - **When** each is parsed/compiled
  - **Then** each fails with a specific, leak-free error (RFC0002.8):
    (i)–(iii) at compile time, citing the single-template pinning
    rule; (iv) at parse time — `group_term` is grammatically confined
    to `by`-lists (§7 v1.1) — citing the grammar. No query reaches
    execution.

- **RFC0002.15 — short/NULL params rows are excluded and tallied `[§6.3 amendment]`**
  - **Given** rows of the pinned template whose `params` list is
    shorter than `n + 1` (or whose slot `n` value is NULL) alongside
    rows carrying slot `n`
  - **When** `count by param(n), …` executes
  - **Then** the short/NULL rows contribute to **no** group (no
    synthetic absent bucket), the returned groups equal the naive
    oracle over the remaining rows, and the number of excluded rows
    is reported per query (a `QueryStats` field, surfaced on the
    RFC 0016 query-metrics path) so the exclusion is observable, not
    silent.

- **RFC0002.16 — the aggregation path's honest bytes total is the group-column scan alone `[RFC 0031 §3.6]`**
  - **Given** an L4-shaped query (`template_id == N | range(…) |
    count by param(n), bucket(w)`) executing with the RFC 0031 §3.6
    honest-total accounting
  - **When** the harness sums the per-query components
  - **Then** the total is the count-scan component only: within the
    surviving (unpruned) row groups, the column chunks read are those
    of the predicate + group-term columns — `template_id`, the
    effective-time column, and `params` iff a `param(n)` term is
    present — and never `body`/`separators`; the row-materialization
    component is zero (an aggregation returns the map, not rows) and
    the template-map-acquisition component
    (`registry_bytes_read`) is zero (nothing is rendered). This is
    the pruning claim RFC0031.5 divides against Loki.

## 6. Design

### 6.1 Predicate sublanguage (Branch B)

A predicate is a boolean expression over **paths**, **operators**, and
**literals** against the OTel log data model. The bare literal `true` is
the **match-all** predicate (for queries that filter only by `range`/other
stages); `false` matches nothing.

**Paths.**

- **Top-level fields** are bare identifiers mapping to the OTel log
  data-model fields: `body` (Body — an OTel `AnyValue`: string, bool, int,
  double, bytes, array, or kvlist/map), `severity` (SeverityNumber), `ts`
  (Timestamp), `observed_ts`
  (ObservedTimestamp), `trace_id` (TraceId), `span_id` (SpanId), `scope`
  (InstrumentationScope name), `flags` (TraceFlags). (Backend treatment of
  structured `body` vs `attr.*` is not uniform across the ecosystem; the
  DSL keeps the split explicit rather than flattening.)
- **Resource attributes**: `resource.<key>` where `<key>` is the OTel
  attribute key taken literally including dots (`resource.service.name` →
  resource attribute `"service.name"`). Bracketed form `resource["..."]`
  for any key not expressible as dotted bare identifiers — characters
  outside the bare-identifier set, a segment starting with a digit, or a
  reserved-word collision (`resource["k8s.pod.name"]`, `resource["3rd.party"]`).
- **Log-record attributes**: `attr.<key>` (`attr.http.status_code` →
  attribute `"http.status_code"`); bracketed `attr["..."]` for the same
  non-bare-identifier cases.
- **Severity**: `severity` compares against a **bare severity name**
  (`severity >= error`), case-insensitive, or a **numeric** form
  (`severity >= 17`). All severity comparisons — **including ordering**
  (`<`/`<=`/`>`/`>=`) — are defined on the OTel **`SeverityNumber`**,
  **never** on the free-form `severity_text` (per the OTel *comparing
  severity* guidance). Bare names map to the **floor of the matching OTel
  `SeverityNumber` range**: `trace`→1, `debug`→5, `info`→9, `warn`→13,
  `error`→17, `fatal`→21. The spec standardises the *ranges* and says to
  compare on `SeverityNumber`; this name→number mapping is **Ourios's**,
  aligned with those ranges, not separately mandated by OTel.

**Operators.** Comparison: `==`, `!=`, `<`, `<=`, `>`, `>=`, `=~`
(regex match), `!~` (regex non-match). Boolean: `and`, `or`, `not`, with
terse aliases `&&`, `||`, `!`; grouping with `()`.

**Literals.** Double-quoted strings (`"api"`), numbers (`500`, `0.7`),
booleans (`true`/`false`), `null`, duration literals (`30s`, `1h`, `1d`,
`1w`), and RFC 3339 timestamps.

**Functions** (read-only, bespoke names tuned for queries) — boolean
predicate terms: `matches(path, regex)`, `contains(path, s)`,
`starts_with(path, s)`, `ends_with(path, s)`. They require a **string
operand**: applying one to a non-string path (`severity`, a numeric/bool
attribute, `lossy`, `ts`) is a compile-time type error (RFC0002.8), not a
silent coercion. (Scalar-returning functions such as `len(path)` are
**deferred**: the grammar admits a call only as a boolean term, so a
numeric `len(...) > n` would need a scalar-comparison form — added under a
future minor version when a need surfaces.)

**Worked predicate.**

```text
service == "api" and severity >= error and attr.http.status_code == 500
```

### 6.2 First-class OTel-canonical fields

Per OpenTelemetry maintainer guidance (the primary dimensions a log
backend is judged on), these get **named, bare** surface rather than
hand-written attribute lookups, resolving the last open question of the
prior draft:

| Surface | Resolves to (RFC 0001 §6.1) |
|---|---|
| `service` | `resource["service.name"]` |
| `trace_id`, `span_id` | the dedicated columns (log↔trace correlation) |
| `scope` | `scope_name` |
| `severity` | `severity_number` (via the §6.1 mapping) |
| `ts` | `time_unix_nano` (the verbatim event timestamp) |
| `observed_ts` | `observed_time_unix_nano` |

> **Amendment 2026-06-11 — `range(...)` filters the effective
> timestamp.** This table previously noted that `ts` /
> `time_unix_nano` is "what `range(...)` filters". Per RFC 0005
> §3.2 (amendment of the same date), the time window **shall**
> compile against the derived `effective_time_unix_nano` column —
> `time_unix_nano` when non-zero, else
> `observed_time_unix_nano.unwrap_or(0)` (RFC 0005 §3.2 is the
> normative derivation; a record with neither timestamp stays at
> `0`). The implementing slice follows this amendment; until it
> lands, the querier filters `time_unix_nano` directly. The change
> makes records whose source timestamp
> is unknown (`time_unix_nano = 0` — ~15 % of real OTel-Demo
> corpora, per the OTLP logs data model's "Use `Timestamp` if it
> is present, otherwise use `ObservedTimestamp`" recommendation)
> addressable by time. The bare `ts` field is unchanged — it
> still resolves to `time_unix_nano`, the verbatim wire value
> (RFC 0001 scenario RFC0001.10). For files written before the
> column existed the window applies `effective :=
> time_unix_nano` (the RFC 0005 §3.9 documented default — exactly
> the pre-amendment behaviour), **not** the absent-OPTIONAL-column
> ⇒ predicate-false convention. The window bounds are
> **half-open** — `range(from, to)` selects
> `from <= effective < to`. The half-open shape is what the
> querier already implements today (over `time_unix_nano`) and
> matches RFC 0010's locally-pinned `[from, to)` (which noted
> this RFC had not pinned boundary semantics; it now does).

`trace_id` / `span_id` literals are **hex strings** (32 and 16 hex digits
respectively, no separators), **parsed case-insensitively** so uppercase
OTLP/JSON ids are accepted; the canonical/serialised form is lowercase.
The compiler hex-decodes them to match the stored byte columns — the
OTLP/JSON id convention, consistent with RFC0003.6.

### 6.3 Template + correctness primitives

First-class vocabulary — **Ourios-specific** extensions (RFC 0001
§6.3/§6.7), **not** OpenTelemetry log-data-model fields; they live in the
Ourios schema + query layer alongside the OTel-canonical fields of §6.2:

- `template_id == 42` — exact template; resolves to the `template_id` column.
- `resolves_to(42)` — `X` plus its drift aliases (the RFC 0001 §6.7 drift
  question); compiles to alias-set membership over `template_id`.
- `confidence` — miner confidence (e.g. `< 0.7`); the `confidence` column.
- `lossy` — the lossy-reconstruction flag; resolves to the RFC 0001 /
  RFC 0005 **`lossy_flag`** column (`lossy == true`).
- `render` (pipe stage, §6.5) reconstructs the original line, honouring
  `lossy`.

The drift *question* is answered by `resolves_to` (alias membership). A
bare `drift` predicate ("has this template drifted?") is **deferred**:
per RFC 0001 §6.7 drift is an audit-stream property, not a column in the
RFC 0005 data files, so it needs an audit-stream query path — a future
capability, not a row predicate in this grammar.

> **Amendment 2026-07-15 — aggregation group terms `param(n)` and
> `bucket(width)` (RFC 0031 L4).** RFC 0031 §3.4 fixes a must-win
> query class **L4** — count of one template over time, grouped by an
> extracted param — and scenario RFC0031.5 names the Ourios surface it
> measures: *"columnar `GROUP BY` on `template_id` + a typed param
> column"*. The DSL had no way to say that; this amendment adds it,
> discharging the §9 `params[N]` open item on its positional half. Two
> **group terms** join the `by`-list of the `count` and `agg_fn`
> stages (grammar delta in the §7 amendment). They are *not* general
> fields: `project`, predicates, and `sort` do not admit them — the
> §7 v1.1 `group_term` production confines them grammatically, so use
> outside a `by`-list is a parse error (RFC0002.14).
>
> - **`param(n)`** — the template's parameter slot `n`, zero-based,
>   addressing the positional RFC 0005 §3.2 `params` list
>   (`List<Struct{type_tag, value}>`; RFC 0001 §6.1). Valid **only**
>   when the query's predicate **pins exactly one `template_id`**:
>   params are positional *per template*, so grouping across templates
>   by position aggregates unrelated values — meaningless, and
>   rejected at compile time (RFC0002.14), never silently computed.
>   The pinning rule is syntactic and decidable on the
>   associative-normalised IR: the predicate must carry, at its top
>   conjunctive level, at least one `template_id == <N>` comparison,
>   and all such comparisons must name the same `N`. A
>   `template_id == N` under `or`/`not` pins nothing.
>   `resolves_to(n)` does **not** pin: it expands to an alias *set*,
>   and drift aliases do not guarantee positional param alignment
>   across the class — cross-alias positional grouping is exactly the
>   meaningless case. (Named parameters via the template schema, the
>   still-open §9 half, are the future route to alias-safe grouping.)
>
>   **Value semantics.** The group key is the param's **original
>   string form** — the stored `value` bytes of slot `n` as UTF-8
>   (`ourios_core::record::Param.value`). The stored `type_tag` is
>   recorded metadata, **not a type promotion**: `param(n)` never
>   parses, coerces, or numerically compares the value, so a slot that
>   captured `"500"` groups as the string `"500"`. A param that
>   overflowed the RFC 0001 §6.5 byte limit groups by its stored
>   marker form — consistently "the string form on disk". Typed
>   (promoted) grouping is future work, not this amendment.
>
>   **Short/NULL disposition.** A row whose `params` list is shorter
>   than `n + 1`, or whose slot-`n` value is NULL, is **excluded**
>   from the aggregation — it contributes to no group, and there is no
>   synthetic "absent" bucket. Rationale: (1) *equivalence* — LogQL
>   extraction (the RFC0031.1 comparator) produces no sample for a
>   line the pattern does not match, so an absent-bucket key on the
>   Ourios side would make the RFC 0031 §3.5 `(bucket, group_key) →
>   count` maps structurally unequal for reasons that are artifacts of
>   our sentinel, not of the data; (2) no sentinel key can be chosen
>   that cannot collide with a real param value (params are arbitrary
>   strings); (3) within one pinned `template_id`, a short params list
>   is the anomaly path (version arity drift), and presenting an
>   anomaly as a data value would be a quiet lie. The exclusion is
>   **not silent**: the per-query excluded-row count is reported
>   (RFC0002.15).
>
> - **`bucket(width)`** — fixed-width time bucketing usable in the
>   same `by`-list (e.g. `count by param(0), bucket(5m)`), and freely
>   without `param(n)` (e.g. `count by service, bucket(1h)` — no
>   pinning requirement of its own). `width` is the existing §7
>   `duration` lexical form (`30s`, `5m`, `1h`, `1d`, `1w`); it must
>   be positive (`bucket(0s)` is a compile-time error). It buckets the
>   **effective timestamp** — the same derived
>   `effective_time_unix_nano` column `range(...)` filters (§6.2
>   amendment 2026-06-11; RFC 0005 §3.2 derivation, including the
>   §3.9 old-file default) — into half-open, epoch-aligned UTC windows
>   `[k·width, (k+1)·width)` in nanoseconds since the Unix epoch. The
>   epoch is UTC-midnight-aligned, so `bucket(1d)` windows are UTC
>   calendar days; `bucket(1w)` windows are epoch-aligned 7-day spans
>   (starting Thursdays), **not** ISO calendar weeks. The bucket key
>   in the result map is the window start (serialised RFC 3339 UTC).
>   A `by`-list admits **at most one** `bucket(...)` term and at most
>   one `param(n)` per `n`; duplicates are a compile-time error.
>   (The RFC 0031 harness pins the LogQL `step`/`start` to the same
>   epoch-aligned boundaries so the two systems' bucket keys
>   coincide — a harness obligation, noted here for RFC0031.1.)
>
> On the **structured surface** (§6.4) the group terms are the stage
> `by`-array elements `{ "param": <n> }` (non-negative integer) and
> `{ "bucket": "<duration>" }` (the §7 duration lexical string); the
> RFC0002.2 one-IR invariant extends to them, and the published JSON
> Schema gains both forms additively (§6.6 amendment). In the IR the
> `by`-list element widens from a plain field to a group term
> (`Field::Param(u32)`-style positional variant + a bucket term) — the
> exact Rust modelling is the implementing slice's choice; this RFC
> constrains the surfaces and the semantics.

### 6.4 Two front-ends, one core

```mermaid
flowchart LR
  A["string DSL (β)<br/>Perses YAML, humans"] --> P[parser]
  B["structured surface<br/>JSON, MCP agents + clients"] --> V[schema validate]
  P --> IR[query IR]
  V --> IR
  IR --> C["compiler<br/>(no SQL leakage)"]
  C --> LP["DataFusion LogicalPlan<br/>(RFC 0007 execution layer)"]
```

- **String DSL (surface β)** is the human surface (esp. Perses YAML). A
  query is a predicate optionally followed by `|`-separated stages:

  ```text
  service == "api" and severity >= error | range(-1h, now) | count by template_id | sort count desc | limit 10
  ```

  A predicate-only / "no filter" query uses the match-all atom `true`,
  e.g. `true | range(-1h, now) | limit 100`.

  Stages: `range(from, to)` (each bound a relative duration, the `now`
  keyword, or an RFC 3339 timestamp — the §7 `time` form; defaults per
  §4 P5), `count [by <field, …>]` (comma-separated, per the §7
  `field_list`) and other aggregations (`sum`, `min`,
  `max`, `avg` over a path), `sort <field-or-aggregate> [asc|desc]`
  (the §7 `sort_key` — a field or an aggregate output like `count`),
  `limit <n>`,
  `project <field, …>` / `render`. The whole query is expressible on one
  line as shown (whitespace around `|` is optional) — the §4 P7 YAML
  constraint.

- **Structured surface** is the machine contract (MCP tool schema +
  programmatic clients): a top-level object
  `{ "predicate": <node>, "stages": [ <stage>, … ] }` (`stages` optional,
  default `[]`). A **`field`** is **structured** (no DSL path syntax for
  agents to build or escape): a bare top-level name string (`"service"`,
  `"severity"`, `"body"`, `"trace_id"`, …) or an attribute object
  `{ "resource": "<key>" }` / `{ "attr": "<key>" }` (`<key>` the raw OTel
  attribute key, e.g. `"k8s.pod.name"`). An **`op`** is a §7 `cmp_op`
  string (`"=="`, `">="`, `"=~"`, …); a **`value`** is a JSON
  primitive (string / number / bool / null), with durations and timestamps
  carried as their §7 lexical strings (`"1h"`, RFC 3339). A **`<node>`** is
  a **comparison node** `{ "field": …, "op": …, "value": … }`, a **call
  node** `{ "call": "<fn>", "args": [ … ] }` whose `args` follow the §7
  typed signatures — `matches`/`contains`/`starts_with`/`ends_with` take
  `[ <field>, <string> ]` (`<field>` as above), `resolves_to` takes
  `[ <number> ]`, a **constant node** `{ "const": true | false }` (the §7
  `bool_lit` match-all / match-none — `{ "const": true }` is the "no
  filter" predicate), or a **boolean node**
  (`{ "and": [ <node>, … ] }` / `{ "or": [ <node>, … ] }` with a child
  array; `{ "not": <node> }` **unary**, per §7). Each **`<stage>`** is a
  tagged object covering the full §7 stage set —
  `range`/`count`/`sum`/`min`/`max`/`avg`/`sort`/`limit`/`project`/`render`.
  Its **JSON Schema is published and versioned with the parser** (snapshot-
  tested like the §7 grammar; RFC0002.11), and it compiles to the same IR
  as the string surface (RFC0002.2). It is the formalised, extended
  successor to the existing `ourios_querier::QueryRequest` (the RFC 0007
  structured API) and is the stable surface agents target — no grammar
  generation required.

Both parse/validate to the **same query IR** and compile identically
(RFC0002.2). The tenant is **not** expressed in either surface — it is
supplied by the executing context (`CLAUDE.md` §3.7 multi-tenancy;
enforced per RFC0007.5); a query
without a tenant is an API usage error, not a cross-tenant scan.

### 6.5 Compilation target

Every construct compiles to a DataFusion `LogicalPlan`:

| DSL construct | DataFusion logical node |
|---|---|
| implicit `from logs` | `TableScan` on the tenant's log table |
| predicate / `range` | `Filter` (range → time-column predicate) |
| `count` / aggregations | `Aggregate` |
| `sort` | `Sort` |
| `limit` | `Limit` |
| `project` | `Projection` |
| `render` | custom projection honouring the three-zone reconstruction model |
| `resolves_to(42)` | custom node expanding to alias-set membership |

All but `render` and `resolves_to` are DataFusion's built-in algebra;
those two are the only Ourios extensions, both surface-independent.

> **Amendment 2026-07-15.** The §6.3 group terms lower **inside** the
> existing `count` / aggregations row: `param(n)` compiles to a
> list-element extraction over the `params` column and `bucket(width)`
> to a fixed-width truncation of the effective-time column, both as
> grouping *expressions* within the `Aggregate` node. No new logical
> node — the "only `render` and `resolves_to`" statement above is
> unchanged.

### 6.6 Stability and versioning

The grammar (§7) is owned and versioned by this RFC. Additions (new
functions, new first-class fields) are minor versions. Behavioural changes
that could alter a query's result set are major versions, require an
amending RFC + a deprecation window, and — because the Perses/MCP
audiences persist queries (git-versioned dashboards, cached agent schemas)
— ship with a documented migration. There is no external spec to shadow,
so major versions are deliberate, not inherited.

> **Amendment 2026-07-15.** The §6.3 group terms are an addition (new
> first-class grammar surface, no behavioural change to any existing
> query), i.e. a **minor version** under this section: grammar
> **v1.1**, v1.0 being the surface green as RFC0002.1–.11. The
> grammar carries no in-code version constant today, so this amendment
> record *is* the version record. The published structured schema's
> `$id` major (`…/structured-query/v1.json`) is unchanged — the
> additions are backward-compatible (`by` arrays accept two new object
> forms). Extending `structured_query.schema.json` + its snapshot
> test, and the parser-side grammar snapshot (§8), are obligations of
> the implementing slice.

## 7. Grammar specification (owned by this RFC)

A compact EBNF; the canonical machine-readable grammar lives beside the
parser and is snapshot-tested (§8). Kept small and regular so it doubles
as a constrained-decoding grammar for the MCP surface (§3.6).

```ebnf
query        = predicate , { "|" , stage } ;
predicate    = or_expr ;
or_expr      = and_expr , { ("or" | "||") , and_expr } ;
and_expr     = unary , { ("and" | "&&") , unary } ;
unary        = [ "not" | "!" ] , ( comparison | call | bool_lit | "(" , predicate , ")" ) ;
bool_lit     = "true" | "false" ;   (* match-all / match-none; a bare `true` = no filter *)
comparison   = severity_cmp | scalar_cmp ;
severity_cmp = "severity" , ord_op , ( severity_name | number ) ;
scalar_cmp   = scalar_path , cmp_op , literal ;
ord_op       = "==" | "!=" | "<" | "<=" | ">" | ">=" ;   (* no regex — severity is numeric *)
cmp_op       = ord_op | "=~" | "!~" ;
call         = str_fn , "(" , path , "," , string , ")"
             | "resolves_to" , "(" , number , ")" ;
str_fn       = "matches" | "contains" | "starts_with" | "ends_with" ;
path         = field | "resource" , key_tail | "attr" , key_tail ;
scalar_path  = nonsev_field | "resource" , key_tail | "attr" , key_tail ;
field        = nonsev_field | "severity" ;
nonsev_field = "body" | "ts" | "observed_ts" | "trace_id" | "span_id"
             | "scope" | "flags" | "service" | "template_id"
             | "confidence" | "lossy" ;
key_tail     = ( "." , dotted_key ) | ( "[" , string , "]" ) ;
dotted_key   = ident , { "." , ident } ;
stage        = "range" , "(" , time , "," , time , ")"
             | "count" , [ "by" , field_list ]
             | agg_fn , "(" , path , ")" , [ "by" , field_list ]
             | "sort" , sort_key , [ "asc" | "desc" ]
             | "limit" , integer
             | "project" , field_list
             | "render" ;
agg_fn       = "sum" | "min" | "max" | "avg" ;
field_list   = field , { "," , field } ;
sort_key     = field | ident ;          (* ident = an aggregate output, e.g. count *)
literal      = string | number | boolean | "null" | duration | timestamp ;
severity_name = "trace" | "debug" | "info" | "warn" | "error" | "fatal" ;  (* case-insensitive; only as a `severity` RHS *)
time         = "now" | ( [ "-" ] , duration ) | timestamp ;   (* e.g. now , -1h *)
integer      = digit , { digit } ;
(* lexical: ident = letter , { letter | digit | "_" } ;
   string = '"' , { char | escape } , '"' ;
   char   = any Unicode scalar except '"' , '\' , or a line terminator
            (a literal newline must be written as the \n escape — queries
            are single-line, §4 P7 / RFC0002.10) ;
   escape = '\' , ( '"' | '\' | "n" | "t" | "r" | ( "u" , 4 * hex ) ) ;
   number = integer | float ;  float = integer , "." , digit , { digit } ;
   boolean = "true" | "false" ;
   duration = integer , ( "s"|"m"|"h"|"d"|"w" ) ;  timestamp = RFC 3339 ;
   digit = "0".."9" ;  letter = "a".."z" | "A".."Z" ;
   hex = digit | "a".."f" | "A".."F"
   — strings are double-quoted with backslash escapes; YAML embedding
   (RFC0002.10) wraps the whole query in a single-quoted YAML scalar so
   these double quotes need no YAML-level escaping *)
```

> **Amendment 2026-07-15 — grammar v1.1 (aggregation group terms).**
> The EBNF above is **v1.0**, the surface shipped green as
> RFC0002.1–.11. v1.1 replaces the two aggregation stages' `field_list`
> with a `group_list` and adds two productions:
>
> ```ebnf
> stage        = (* v1.0 alternatives unchanged, except: *)
>                "count" , [ "by" , group_list ]
>              | agg_fn , "(" , path , ")" , [ "by" , group_list ] ;
> group_list   = group_term , { "," , group_term } ;
> group_term   = field
>              | "param" , "(" , integer , ")"
>              | "bucket" , "(" , duration , ")" ;
> ```
>
> `field_list` remains as-is for `project`; `duration` and `integer`
> are the existing productions. Semantics — the single-template
> pinning rule for `param(n)`, string-form group keys, the
> excluded-short-rows disposition, epoch-aligned half-open buckets
> over the effective timestamp — are the §6.3 amendment's. The
> canonical machine-readable grammar and its snapshot update with the
> implementing slice (§6.6 amendment).

## 8. Testing strategy

Mapping to `CLAUDE.md` §6.2 and `docs/verification.md` §3 (red→green
two-loop: `#[ignore]`'d stubs first, implementations second).

- **Unit tests** — every grammar production has a positive and negative
  parse test.
- **Property tests** — generate well-formed queries; assert the §5
  round-trip idempotence (RFC0002.7) and that every generated query is a
  YAML-safe single-line scalar (RFC0002.10).
- **Compilation golden tests** — every construct has a golden `LogicalPlan`
  (debug-rendered) checked in; the no-leakage boundary (RFC0002.3) is a
  compile + string test.
- **Equivalence tests** — string vs structured surface compile to the
  same IR (RFC0002.2); a DSL query and the equivalent `ourios_querier`
  structured request return identical results + `QueryStats` (RFC0002.1).
- **Grammar snapshot** — the EBNF / parser grammar is committed and
  snapshot-tested so changes are PR-visible (Branch B owns its grammar).
- **End-to-end** — against the `docs/benchmarks.md` §1 corpora, pinned
  expected results for a query set spanning each construct.

## 9. Open questions

*Narrowed by the §3 resolution. Must be resolved before `accepted`.*

- [ ] **Pre-`accepted` validation.** The §3.6 audience analysis stands in
      for instinct, not for evidence: before `accepted`, run a readability
      pass on 10–20 sample queries with non-author reviewers, and a
      migration sketch from LogQL/Insights into β. (Replaces the prior
      §9 user-research gate; not required for `specified`.)
- [x] ~~**OTel ecosystem alignment**~~ *Resolved:* OpenTelemetry defines
      the logs data model + API/SDK but **no standard query/read
      language**, and **OTTL is a Collector *transformation* language, not
      a querying surface** (see the OTTL README and the OTel logs spec
      linked in §11 References). There
      is no canonical OTel read syntax, and no Perses-specific query
      convention, to align to. Bespoke query syntax over the OTel data
      model is the norm (LogQL, PromQL, CloudWatch Insights), so Branch B
      carries no ecosystem-divergence cost — the alignment that matters is
      at the **field semantics**, which §6.1/§6.2 honour
      (`ts`/`observed_ts`/`trace_id`/`span_id`/`flags`/`body`/`scope`/
      `severity` → canonical data-model fields; `severity` ordering on
      `SeverityNumber`).
- [ ] `--sql` advanced-mode escape hatch — gated + sandboxed, or never?
      (Currently: never; reconsider under a separate RFC.)
- [ ] Custom user functions — out for v1 (sandboxing is its own project).
- [x] ~~`params[N]` positional access vs named parameters via the
      template schema.~~ *Resolved in part (amendment 2026-07-15):*
      positional access is specified as the `param(n)` group term
      (§6.3 amendment; grammar v1.1, §7 amendment) — aggregation-only
      (`by`-lists), single-`template_id`-pinned, string-form group
      keys, scenarios RFC0002.12–.16. **Named parameters via the
      template schema stay open** — they are the future route to
      alias-safe (cross-version) grouping, which `param(n)`
      deliberately forbids.
- [ ] In-path query cost estimator ("this will scan 400 GB") before run.
- [ ] Pagination / streaming surface for large result sets (mirrors
      RFC 0007 §8).

Resolved by this RFC (were open in the draft): branch (B), top-level
surface (β), severity-text casing (case-insensitive, §6.1), agent-
friendliness (the structured surface, §6.4), and first-class OTel-
canonical fields (§6.2).

## 10. Alternatives considered

Alternatives that would replace the whole design, not just one branch.

- **Pure SQL (DataFusion dialect)** — zero parser cost, but violates
  `CLAUDE.md` §4 hazard 6 (cross-tenant joins, unbounded scans) and binds the user
  surface to DataFusion. Rejected as default; possible future gated,
  sandboxed escape hatch under a separate RFC.
- **LogQL clone** — label selectors are less expressive than the OTel log
  record; adopting them flattens structure and lies about the ingest
  contract. Rejected as the full DSL; its top-level shape survives as the
  chosen β surface.
- **CloudWatch Insights clone** — proprietary, no open spec; attribute
  model differs from OTel. Rejected; its verb-per-line readability is the
  γ alternative we did not pick.
- **Branch A (borrow OTTL) on any surface** — see §3; not chosen for the
  Perses/MCP audiences.

## 11. References

- OpenTelemetry log data model:
  https://opentelemetry.io/docs/specs/otel/logs/data-model/
- OpenTelemetry severity text conventions:
  https://opentelemetry.io/docs/specs/otel/logs/data-model/#field-severitytext
- OTTL (reference-only under Branch B):
  https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/main/pkg/ottl
- LogQL: https://grafana.com/docs/loki/latest/query/
- CloudWatch Logs Insights:
  https://docs.aws.amazon.com/AmazonCloudWatch/latest/logs/CWL_QuerySyntax.html
- Perses (CNCF dashboards-as-code): https://perses.dev/
- Apache DataFusion logical-plan documentation.
- RFC 0001 §6.1/§6.3/§6.7 (the columns + template/drift primitives);
  RFC 0007 (the execution layer this DSL targets); `CLAUDE.md` §4 hazard 6
  (no-leakage hazard) and `CLAUDE.md` §3.7 (multi-tenancy).

