---
rfc: 0044
title: Template-aware body equality — the two-arm compile for body ==
status: green
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-29
supersedes: —
superseded-by: —
---

# RFC 0044 — Template-aware body equality (`body ==` two-arm compile)

> **Status: `green` (2026-07-29).** All nine §5 criteria pass, landed in
> five slices the same day: #671 (the plan-time matcher), #672 (the
> two-arm compile — #664 closed there), #673 (.6 with the alias-exclusion
> refinement), #674 (.7/.8 pruning fixtures), #675 (.9 as a generative
> property **and** every line of the committed §3.3 corpus). Three
> refinements implementation forced on the spec, each stated inline:
> the template arm is a plan-time candidate *superset* with exactness at
> scan time (separators/overflow are per-record); alias-class expansion
> is excluded as wrong, not just unneeded (§3.3); and equality carries no
> `IS TRUE` wrap — under a filter NULL ≡ false, and the wrap defeats
> row-group pruning (`!=` keeps `IS NOT TRUE`, where three-valued logic
> genuinely bites). No thesis-gate applies; `accepted` is a maintainer
> flip.
>
> *(`specified`, same date: closes #664; equality only, other body
> operators §7; the design reuses RFC 0042 §3.3's two-arm pattern and is
> well-defined because `CLAUDE.md` §3.3 guarantees render fidelity.)*

## 1. Summary

`body == "literal"` today compiles against the physical body column
only. High-confidence mined records store `body` as NULL (the template
carries the text), so the predicate silently matches nothing and the
row group is pruned — a confident-looking wrong answer (#664). This RFC
compiles body equality to a **two-arm** predicate: the physical-column
arm (covering retained and lossy bodies, as today) **OR** a plan-time
**template arm** that resolves the literal against the tenant's template
store — zero-parameter templates become a prunable
`template_id IN (…)`, parameterized templates become
`template_id == T AND param(i) == v…`. Body stays a first-class
queryable field, as the OTel ecosystem expects (OTTL's `log.body`,
stanza's body-default paths), and correct empties stay cheap.

## 2. Motivation

**Silent empties violate the project's own ethos.** The DSL fails
loudly everywhere else (unpromoted group-by names its fix; unknown
fields list §7). `body ==` is the one accepted-but-wrong query — and it
bites hardest on the GenAI corpus, whose bodies are event names and
mine to zero-parameter templates with 100% confidence, i.e. the exact
records whose physical body is always NULL.

**Rejecting body equality is not an option.** The OTel ecosystem
treats body as the primary addressable content (Logs Data Model
top-level field; OTTL and stanza both path into it, stanza even
defaulting bare fields to `body.*`). A backend where `body ==` errors
out is the odd one out. (The *idiomatic* event filter is
`event_name` — RFC 0043's half — but the compatibility path must be
correct, not absent.)

**The machinery already exists.** RFC 0042 §3.3 established the
two-arm compile (typed arm OR stored-form arm). RFC 0033's cached
template map gives the querier plan-time template access. `param(n)`
is an existing DSL accessor. Reconstruction is property-tested
byte-identical (`CLAUDE.md` §3.3), which is what makes literal →
`(template, params)` inversion sound.

## 3. Proposed design

### 3.1 The two arms

For `body == L` (string literal `L`):

- **Physical arm** (unchanged): `body_col == L`. Covers low-confidence
  retention and lossy-reconstruction records (`CLAUDE.md` §3.1/§3.3
  rules), prunable via existing column statistics.
- **Template arm** (new): at plan time, match `L` against every
  template in the tenant's map.
  - A **zero-parameter** template matches iff its rendered text equals
    `L` byte-for-byte → contribute its id to a `template_id IN (…)`
    disjunct. Prunable via template_id min/max + bloom.
  - A **parameterized** template matches iff `L` unifies with its token
    structure (anchored, whitespace-exact per §3.3 capture) → contribute
    `template_id == T AND param(0) == v0 AND …` with the implied
    parameter values. A literal may unify with several templates; each
    contributes a disjunct.
- The compiled predicate is `physical-arm OR template-arm`. A row group
  is skipped only when **both** arms are impossible — so a literal that
  matches nothing anywhere still prunes everything (correct empties
  stay cheap, RFC0044.8).

### 3.2 `!=` and three-valued logic

`body != L` compiles as the negation with explicit NULL handling: a
mined record (physical body NULL) matches `!=` iff its template-side
value does not equal `L`; the physical arm's NULL must not silently
exclude mined records (the mirror image of the #664 bug). The typed
`!=` arm in RFC 0042 §3.3 (presence kept explicit) is the pattern.

### 3.3 Template-map freshness and versioning

- The template arm resolves against the **same tenant template map
  snapshot the query's read path uses** (RFC 0033) — the predicate can
  never be staler than the rendering the user sees.
- The plan-time match covers template **versions and renames**
  inherently: the registry folds *every* `(template_id, version)`'s
  tokens from the audit stream, and unification checks every entry — a
  template re-created under a new id or widened to a new version across
  deploys contributes each id/version whose tokens still unify with the
  literal. Missing this recreates #664 one deploy later.
  *(Refined at implementation time from "traverses versions and
  aliases": expanding RFC 0007 **alias classes** here would be wrong,
  not just unnecessary — alias classes group templates whose shapes
  differ, and byte-equality must never admit a record whose own tokens
  do not render the literal. `resolves_to(n)` remains the query for
  shape-crossing equivalence.)*

### 3.4 Structured bodies

A string literal never matches a structured body (RFC 0037): those
records are excluded by both arms by construction, and the docs point
at the structured accessors. No error — mixed corpora are normal.

### 3.5 Out of scope

Ordering (`<`, `>=`), substring, and regex against `body` are
**unchanged** by this RFC (a pattern matches unboundedly many rendered
forms; there is no bounded plan-time inversion). Whether those forms
share a silent-miss today and what to do about it is §7 — explicitly
not smuggled into this slice.

## 4. Alternatives considered

- **Reject body equality loudly.** Honest and cheap, but diverges from
  ecosystem expectations (§2) and removes the natural compatibility
  query for event-named bodies. Rejected.
- **Always store the body column.** Correct by brute force; destroys
  pillar #2's economics and the pruning value the thesis rests on.
- **Scan-time reconstruction (render every row, compare).** Correct but
  unprunable — every body query becomes a corpus scan, exactly what
  pillar #1 exists to avoid. The plan-time inversion keeps pruning.
- **Fix only via RFC 0043 (event_name).** Handles the event corpus but
  leaves `body ==` silently wrong for every other mined line — the bug
  class survives.

## 5. Acceptance criteria

- **RFC0044.1 — the #664 reproduction matches**
  - **Given** an ingested record whose body mined to a zero-parameter
    template (physical body NULL)
  - **When** `body == "<that exact body>"` runs
  - **Then** the record returns, with the row group scanned not pruned.
- **RFC0044.2 — parameterized unification**
  - **Given** records under a template with parameter slots
  - **When** `body ==` runs with a literal equal to one record's
    original line
  - **Then** exactly that record returns (template + implied params).
- **RFC0044.3 — retained bodies still match**
  - **Given** a low-confidence record whose original body is retained
  - **When** `body ==` runs with that body
  - **Then** it matches via the physical arm.
- **RFC0044.4 — `!=` does not silently drop mined records**
  - **Given** mined records with reconstructions ≠ `L`
  - **When** `body != L` runs
  - **Then** they all return despite NULL physical bodies.
- **RFC0044.5 — structured bodies are excluded, not errored**
  - **Given** a mixed corpus with RFC 0037 structured bodies
  - **When** `body == "<string>"` runs
  - **Then** structured-body records are absent and the query succeeds.
- **RFC0044.6 — versions and renames contribute every matching id**
  - **Given** records written under a template later re-created under a
    new id and under a widened version (the RFC 0010 drift shapes)
  - **When** `body ==` runs with the rendered text
  - **Then** records under every id/version whose tokens render the
    literal return — and no record whose own tokens do not. *(Refined
    at implementation time: alias-class expansion is excluded by
    design — see §3.3.)*
- **RFC0044.7 — pruning still engages**
  - **Given** a multi-file corpus where the matched template appears in
    a strict subset of row groups
  - **When** `body ==` runs
  - **Then** `row_groups_pruned` > 0 and results are complete.
- **RFC0044.8 — correct empties stay cheap**
  - **Given** a literal matching no template and no retained body
  - **When** `body ==` runs
  - **Then** the result is empty **and** every row group was pruned.
- **RFC0044.9 — the reconstruction invariant, driven through the
  predicate `[property]`**
  - **Given** every line of the mined corpus (the §3.3 property-test
    corpus)
  - **When** `body == <original line>` is compiled and evaluated
  - **Then** the originating record is found for every non-structured
    line — equality-through-templates is exactly as faithful as
    reconstruction itself.

## 6. Testing strategy

Unit tests for the plan-time matcher (zero-param, parameterized,
multi-template unification, alias traversal); integration through
ingest→query for .1–.8; RFC0044.9 as a `proptest` extension of the
existing reconstruction property suite (per `CLAUDE.md` §6.2,
reconstruction is always a property test — this drives the same
invariant through the predicate path). Pruning assertions read the
`stats` counters the query response already carries.

## 7. Open questions

- [ ] **Substring/ordering/regex on body** — do they share the silent
      miss today, and if so: loud rejection, opt-in reconstruction
      scan, or leave documented? Follow-up RFC either way (§3.5).
- [ ] **Plan-time match cost telemetry** — template counts are bounded
      (RFC 0023, C2), so the match is expected to be sub-millisecond;
      is a plan-phase duration attribute on the query span (RFC 0038)
      worth adding while instrumenting this?

## 8. References

- #664 — the reproduction this RFC closes.
- RFC 0042 §3.3 — the two-arm compile pattern (typed arm OR stored
  form) this design reuses for body.
- RFC 0033 — the cached tenant template map (plan-time access).
- RFC 0007 / RFC 0010 — alias storage and drift semantics the template
  arm must traverse.
- RFC 0037 — structured bodies (§3.4 exclusion rule).
- RFC 0043 — the idiomatic-path complement (`event_name` derivation);
  together they close the "how do I filter events" story.
- `CLAUDE.md` §3.3 (bit-identical reconstruction) — the invariant that
  makes the inversion sound; §3.1 (retention) — the physical arm's
  coverage; hazard #6 (DSL surface).
- OTel — OTTL `log.body` paths; Collector stanza field defaults
  (`body.*`); Logs Data Model (body as top-level field).
