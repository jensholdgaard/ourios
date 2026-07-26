---
rfc: 0042
title: Typed numeric attribute promotion (RFC 0022 amendment)
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-26
supersedes: —
superseded-by: —
---

# RFC 0042 — Typed numeric attribute promotion (RFC 0022 amendment)

> **Status note (2026-07-27):** RFC0042.1–.8 are green in-repo across
> the implementation slices (#649 writer, #650 config, #651 scan +
> no-coercion adapter, #652 predicates + aggregation, and the
> compaction re-typing test). The one outstanding criterion is
> **RFC0042.9** — the dogfood corpus gate — which needs a fresh agent
> capture under the typed promotion set (PR #646); the status flips to
> `green` when that query returns real spend. Until then this RFC
> stays `specified` with the implementation ahead of the flip.

> Enacts RFC 0022 §7.1, whose deferral clause — "deferred until a
> consumer demands it" — has been met: the agent-FinOps loop's headline
> query is `sum(attr.cost_usd) by attr.model`, and `cost_usd` arrives
> as a `double` `AnyValue`, which RFC 0022 §3.1 projects as `NULL`.
> Discovered the honest way: a config-only promotion of the key
> (PR #646, now drafted awaiting this RFC) would have produced an
> always-`NULL` column and a **silently empty** `sum` — strictly worse
> than today's promotion-hint error.

## 1. Summary

RFC 0022 promotes string-valued attributes to dedicated `Utf8`
columns; every other `AnyValue` variant projects `NULL`. This
amendment adds two typed promotion classes — `i64` and `f64` — as a
per-key type declaration in `storage.promoted_attributes`. A typed key
projects to an `OPTIONAL` `Int64`/`Float64` column named by the same
DSL path, with min/max-statistics pruning for ordering predicates and
direct (cast-free) RFC0002.17 scalar aggregation. Files whose column
type does not match the declared type are handled by the same rule
RFC 0022 §3.3 already defines for pre-amendment files: the column
reads as absent. String promotion — the bare-string config entry — is
byte-for-byte unchanged.

## 2. Motivation

Numeric attributes are the common OTLP emission (`http.status_code`
as int; Claude Code's `cost_usd` as double, its token counts as
ints — verified against captured `api_request` events). Under RFC
0022 they are second-class twice over:

1. **Aggregation is impossible.** RFC0002.17 scalar aggregates
   (`sum`/`avg`/`min`/`max`) require a promoted column and `try_cast`
   `Utf8` → `Float64`. A numeric `AnyValue` projects `NULL` (RFC 0022
   §3.1), so
   the column a numeric key would get is always-`NULL` and the
   aggregate silently returns nothing. The FinOps loop — a source
   querying its own spend back through the RFC 0027 MCP surface — dies
   on exactly this.
2. **Ordering never matches.** `attr.http.status_code >= 500` is
   typed-arm-only (RFC 0022 §3.3); on a `NULL`-projected column it
   matches no
   rows, silently.

The responsibility split this serves (maintainer direction,
2026-07-26): the **source** computes cost and stamps it on its own
telemetry; Ourios stores it faithfully, gives it a column, and
aggregates it; pricing tables and FOCUS-shaped output belong to
consumers of the query surface. Typed promotion is the whole of
Ourios's share.

## 3. Proposed design

### 3.1 Type classes

Each promoted key carries a **class**, declared in config (§3.2):

| Class | Arrow / Parquet type | Projects | `NULL` when |
|---|---|---|---|
| `string` (default) | `Utf8` / `STRING` | string `AnyValue` | key absent, or value not a string — RFC 0022 §3.1, unchanged |
| `i64` | `Int64` / `INT64` | int `AnyValue` | key absent, or value not an int |
| `f64` | `Float64` / `DOUBLE` | double **or int** `AnyValue` (int widened) | key absent, or value neither |

- All typed columns are `OPTIONAL` and named by the DSL path exactly
  as in RFC 0022 §3.1 (`attr.cost_usd`). The class changes the
  column's Arrow type, never its name.
- `f64` widens ints because sources are inconsistent (a zero cost may
  arrive as int `0`); `i64` → `f64` is exact for `|v| ≤ 2^53`, and a
  key expected to exceed that belongs in `i64`. `i64` does **not**
  narrow doubles — no silent truncation.
- **String-encoded numbers do not parse.** A string `"500"` under an
  `i64` class projects `NULL`. Parsing would smuggle in coercion
  ambiguity (`"500"` vs `"5e2"` vs `" 500"`) that RFC 0022 §3.1's
  "byte-faithful
  or `NULL`" rule exists to keep out; a source that stamps numbers as
  strings gets the `string` class and lexicographic semantics,
  documented as such.
- Bool, bytes, array, kvlist classes are out of scope (§7).

### 3.2 Configuration

A list entry in `storage.promoted_attributes.{resource,log}` is either
the RFC 0022 bare string (class `string`) or a typed mapping:

```yaml
storage:
  promoted_attributes:
    log:
      - model                          # bare = string class, unchanged
      - { key: cost_usd, type: f64 }
      - { key: input_tokens, type: i64 }
```

- Modeled as a two-variant entry (bare | `{key, type}`), rejecting
  unknown `type` values and duplicate keys across both spellings at
  startup — RFC 0020's strict-parse posture.
- `type: string` is legal and identical to the bare spelling.
- The implicit `resource.service.name` promotion stays `string` and
  cannot be re-typed.
- Rollout ordering is RFC 0022 §3.2's, verbatim: a config carrying
  typed entries requires a binary at or above this RFC's `green`;
  upgrade first, extend the config second.

### 3.3 Cross-file type conflict (the re-typing rule)

Files written under different promoted sets already coexist (RFC 0022
§3.4). This amendment adds a new coexistence case: the same column
name with different physical types (a key promoted as `string`
historically, re-declared `i64` today — or vice versa).

**Rule: a file whose column type differs from the currently declared
class is read as if the column were absent from that file.** This is
the same class of behaviour RFC 0022 §3.3 assigns to pre-amendment
files —
column reads `NULL`, `==`/`!=` fall through to the JSON arm where one
exists, ordering and aggregation exclude those rows — so re-typing
degrades exactly as adding a promotion does, and compaction converges
history toward the current declaration as a side effect (§3.5).

Implementation note (binding): the scan must map per-file schemas onto
the declared scan schema — DataFusion's schema-adapter seam — casting
nothing. A mismatched column is projected as `NULL`, never coerced;
coercion would reintroduce the string-parse ambiguity this RFC's §3.1
rejects.

### 3.4 Predicate compilation (RFC 0022 §3.3 amendment)

For a key of class `i64`/`f64`, the DSL literal must be numeric (the
grammar already has numeric literals — `confidence < 0.7`); a string
literal against a numeric-class key is a compile error with a hint
naming the declared class.

- **Ordering (`<` `<=` `>` `>=`)**: typed arm only, `P op v`, prunable
  via row-group min/max statistics. Same shape as RFC 0022 ordering,
  now on a column whose statistics are numeric rather than
  lexicographic.
- **Equality (`==` `!=`) on `i64`**: two arms, as RFC 0022 §3.3 —
  typed arm plus a JSON fallback arm for files where the column is
  absent or type-mismatched. Canonical integer formatting is unique,
  so the JSON arm is exact.
- **Equality on `f64`**: **typed arm only.** JSON text carries no
  canonical float formatting (`0.1` vs `1e-1`), so a fallback arm
  would be wrong in both directions. Consequence, stated plainly:
  float equality never matches rows in pre-amendment or
  type-mismatched files. Float equality is a degenerate query
  regardless; ordering is the supported idiom.
- **Regex (`=~` `!~`)**: compile error on numeric classes — regex over
  a number is a category error the string class already serves.

### 3.5 Aggregation, encodings, compaction, telemetry

- **RFC0002.17 scalar aggregates** read a numeric-class column
  directly — no `try_cast`. The `Utf8` `try_cast` path remains for
  string-class keys, unchanged. `NULL` cells stay excluded; `sum` over
  an all-`NULL` group returns `NULL`, not `0` (DataFusion semantics,
  now load-bearing: an unpromotable variant must not fabricate a zero
  cost).
- **Encodings (RFC 0005 §3.6 table extension):** page index and
  statistics **yes** for both classes; bloom filter **yes** for `i64`
  (equality is exact and index-backed), **no** for `f64` (equality is
  discouraged — §3.4); dictionary encoding left to writer defaults.
- **Compaction (RFC 0009)** re-projects rewritten rows with the
  *current* typed declaration, exactly as RFC 0022 §3.4 — including
  across a re-typing, which is how history converges. RFC0036.4
  byte-identity holds within a fixed config, as today.
- **Telemetry:** the existing
  `ourios.storage.parquet.promoted.size` instrument covers typed
  columns with no new names or attributes — the promoted-column-name
  attribute already identifies the column. No weaver-registry change.

### 3.6 Schema evolution (CLAUDE.md §3.5)

Additive `OPTIONAL` columns, same evolution class as RFC 0022 §3.4 /
RFC 0018: pre-amendment readers see unknown columns and ignore them;
this reader sees absent columns as `NULL`. No historical rewrite; the
§3.3 rule is the migration plan for the one new conflict case. The
`attributes`/`resource_attributes` JSON columns remain the source of
truth; typed columns are projections, and the RFC 0017 read path never
consumes them — OTLP fidelity untouched.

## 4. Alternatives considered

- **Stringify numerics into the existing `Utf8` columns.** No schema
  change, `sum` works via the existing `try_cast`. Rejected: float
  formatting makes `==` unreliable (the exact reason RFC 0022 §3.1
  projects
  `NULL` today), lexicographic min/max statistics cannot prune numeric
  ordering (`"9" > "10"`), and the cast burns per-row CPU at query
  time forever.
- **Parse string-encoded numbers into typed columns.** Helps sources
  that stamp `"500"`. Rejected: coercion ambiguity (§3.1), and it
  makes projection behaviour depend on value *content* rather than
  variant — untestable by exhaustion over variants.
- **Type-suffixed column names** (`attr.cost_usd#f64`) to make
  re-typing conflicts structurally impossible. Rejected: leaks the
  class into the on-disk schema forever and doubles the RFC 0022 §3.3
  fallback
  surface; the schema-adapter rule handles the rare conflict without
  permanent naming debt.
- **Automatic type inference from observed values.** Rejected:
  write-time inference makes the schema a function of traffic —
  irreproducible files, and hazard #5 with extra steps. The class is
  an explicit operator declaration, like promotion itself.

## 5. Acceptance criteria

- **RFC0042.1 (typed projection).** Given a config promoting
  `attr.cost_usd` as `f64` and `attr.input_tokens` as `i64`, when
  records carrying those keys as double/int `AnyValue`s are ingested,
  then the written file carries `OPTIONAL` `Float64`/`Int64` columns
  of those names holding the values, and the JSON attribute columns
  are byte-identical to an unpromoted run.
- **RFC0042.2 (projection totality).** Given any `AnyValue` variant
  under each class, when projected, then the cell is the §3.1-table
  value or `NULL` — never a panic, never a coerced parse; in
  particular int widens into `f64`, double does not narrow into
  `i64`, and strings project `NULL` under both numeric classes.
- **RFC0042.3 (aggregation).** Given a corpus with promoted numeric
  keys, when `sum`/`avg`/`min`/`max(attr.<key>) by <group>` runs,
  then results equal the oracle computed from the JSON attributes,
  records lacking the key are excluded, and an all-`NULL` group
  yields `NULL`, not `0`.
- **RFC0042.4 (ordering + pruning).** Given multi-row-group files
  with disjoint numeric ranges, when an ordering predicate on a typed
  key runs, then row counts match the JSON oracle and the RFC 0016
  scanned/pruned counters show at least one pruned row group.
- **RFC0042.5 (absent and mismatched files).** Given a scan spanning
  a pre-amendment file, a file with the key promoted as `string`, and
  a file with the current `i64` declaration, when equality and
  ordering predicates and a `sum` run, then the query does not error,
  `==` on the `i64` key answers correctly across all three files (JSON
  arm), and ordering/`sum` cover exactly the current-declaration file.
- **RFC0042.6 (config).** Given bare, typed, and mixed entries, when
  the config parses, then bare entries behave identically to RFC 0022;
  and given an unknown `type`, a duplicate key across spellings, or a
  re-typed `service.name`, then startup fails with an error naming the
  offence.
- **RFC0042.7 (compile errors).** Given a string literal compared
  against a numeric-class key, or a regex on one, then compilation
  fails with a hint naming the declared class; and float `==` compiles
  typed-arm-only.
- **RFC0042.8 (compaction re-projection).** Given input files written
  under the `string` declaration for a key now declared `i64`, when
  compaction rewrites them, then output files carry the `Int64`
  column projected from JSON truth, and RFC0036.4 byte-identity holds
  for repeated compaction under the fixed config.
- **RFC0042.9 (the consumer demand).** Given a captured Claude Code
  corpus ingested under the dogfood promotion set (PR #646's keys,
  typed), when `sum(attr.cost_usd) by attr.model` runs over the MCP
  surface, then it returns non-empty per-model spend matching the
  JSON oracle.

## 6. Testing strategy

- **Property tests (`proptest`):** RFC0042.2 — arbitrary `AnyValue`
  variants × classes; projection is total and never coerces.
  RFC0042.3's oracle comparison over generated numeric corpora
  (reusing the RFC 0024 envelope generators).
- **Unit:** RFC0042.6 config parsing next to the RFC 0020 config
  tests; RFC0042.7 compile errors next to the RFC0002.17 tests.
- **Integration (querier):** RFC0042.1, .3, .4, .5 over written
  fixtures, with the scanned/pruned counters as the pruning oracle
  (the RFC0022.5 pattern).
- **Integration (compaction):** RFC0042.8 in `compaction.rs`'s
  existing re-projection suite.
- **Corpus:** RFC0042.9 against the captured agent-telemetry corpus
  once recaptured under the typed dogfood set.

## 7. Open questions

- [ ] **Bool class** — `attr.<key> == true` for flags like
  `cache_hit`; cheap once the seam exists, no consumer yet.
- [ ] **Timestamp class** — attribute-carried epoch times; wants its
  own design (unit ambiguity), not this RFC.
- [ ] The remaining RFC 0022 §7 items (per-tenant sets, demotion,
  bloom sizing) are untouched by this amendment.

## 8. References

- RFC 0022 — Queryable attribute columns (amended by this RFC; §3.1
  string-only rule, §3.3 predicate arms, §7.1 reservation).
- RFC 0002 §6 / RFC0002.17 — scalar aggregates over promoted columns.
- RFC 0020 — configuration file schema (strict parse posture).
- RFC 0009 / RFC 0036 — compaction re-projection, byte-identity.
- RFC 0027 — the MCP surface the FinOps consumer queries through.
- `CLAUDE.md` §3.5 (schema evolution), §4 hazards #2/#4/#5.
- PR #646 (drafted) — the config change this RFC unblocks.
