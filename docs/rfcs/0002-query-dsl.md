---
rfc: 0002
title: Query DSL — prior decision (borrow from OTTL or distance?) and candidate designs
status: draft
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-04-24
supersedes: —
superseded-by: —
---

# RFC 0002 — Query DSL

> **Status note.** This RFC presents a decision tree, not a finished
> answer. The DSL surface depends on user research we have not done
> yet (§9) and on a prior decision (§3) that has been raised but not
> resolved. The RFC cannot flip to `accepted` until §3 is decided and
> §4–§5 are narrowed to a single branch. Hazard 6 (`CLAUDE.md` §4) —
> no DataFusion leakage — constrains every branch equally.

## 1. Summary

Ourios exposes a logs DSL. The shape of that DSL depends on a prior
decision, which this RFC deliberately does **not** pre-commit:

- **Branch A** — borrow from OTTL for the predicate sublanguage,
  track a pinned spec version, enumerate divergences.
- **Branch B** — actively distance from OTTL; use the OTel *data
  model* (unavoidable — it is the ingest contract) but with an
  Ourios-native path and predicate syntax designed for query
  readability rather than Collector transformation.

Both branches share the same top-level query surface (one of three
candidates in §5.3), the same template and correctness primitives
(§5.4), and the same compilation target (§5.5 — DataFusion
`LogicalPlan`, no SQL leakage). The difference lives in one layer:
how paths and predicates are written.

Section 3 lays out the cases for each branch. Section 5 sketches
both designs concretely enough to compare side by side. Section 9
lists the prior decision as the first open question.

## 2. Motivation

### 2.1 Why a DSL at all?

`CLAUDE.md` §4 hazard 6 commits Ourios to a DSL that does not leak
DataFusion SQL through to users. The reasons are stability (we want
to evolve the backend without breaking user queries), safety (full
SQL exposes a surface area we cannot audit — arbitrary joins across
tenants, resource-exhausting scans), and *fit* (logs are a narrow
domain; a narrow DSL is more ergonomic than a general one).

This motivation is branch-agnostic. Whether we borrow from OTTL or
not, we ship a DSL.

### 2.2 Why the prior decision matters

"OTTL-inspired" sounds like a free decision — borrow the ergonomics,
skip the parts that do not fit. It is not free. Borrowing OTTL
syntax in a query context makes a promise to OTTL-literate users
that their mental model will transfer. If the promise is kept, the
win is real for that user segment. If the promise is broken — if
OTTL syntax looks the same but behaves differently in a query
context — the surface actively misleads users who would have been
better off learning an unfamiliar DSL from scratch. The decision is
load-bearing enough to deserve its own section.

## 3. The prior decision: borrow from OTTL, or distance from it?

Both positions are defensible. This section writes both down
honestly.

### 3.1 The case for borrowing (Branch A)

- **Positive transfer for the highest-leverage user segment.** SREs
  and platform engineers who write Collector processor pipelines
  already think in OTTL paths and predicates. Reusing that mental
  model in the query context is zero-onboarding for them.
- **Reduces bikeshed surface.** Adopting a pinned external
  specification cuts off a whole class of "why did we pick this
  syntax" debates. Decisions are inherited, not re-litigated.
- **Ecosystem alignment.** A project in the OTel orbit that
  gratuitously diverges on surface syntax looks politically odd and
  costs us contribution mindshare we do not need to spend.
- **OTTL's path grammar is near-minimal.** The OTel data model is
  the ingest contract, so we must address it anyway. OTTL's
  `resource.attributes["service.name"]` form is verbose, but it is
  *correct* about the data model, and any alternative has to answer
  the same disambiguation questions.

### 3.2 The case for distancing (Branch B)

- **The OTTL-literate population is smaller than "OTLP users."**
  The subset of OTLP users who write Collector configs is a
  minority. Most developers emit logs via an SDK and never touch
  OTTL. The familiarity-transfer argument applies to a narrower
  segment than §3.1 implies.
- **OTTL's Collector ergonomics become query verbosity.**
  `resource.attributes["service.name"] == "api"` is readable in a
  Collector processor config (where record-typing is explicit) and
  is unnecessarily loud in a query (where the user is asking a
  question about logs, not configuring a pipeline). Branch B can
  collapse this to `resource.service.name == "api"` or similar.
- **Shared syntax plus different semantics actively misleads.** A
  user who sees OTTL syntax reasonably expects OTTL semantics —
  batch mutation, pipeline forwarding. Query semantics are
  different. The safer failure mode is unfamiliar syntax than
  almost-familiar-but-not-quite syntax.
- **Maintenance burden of tracking an evolving spec.** OTTL's
  grammar has had breaking changes in the Collector-contrib
  repository. Pinning a version helps, but every time we revisit
  the pin we pay a review cost that we avoid if we never opted into
  the commitment.
- **Design freedom for the query context.** A distanced DSL can use
  bare-identifier severity (`severity >= error`), short prefixes
  (`attr.foo`), and other idioms that are tuned for typing speed
  and readability in the query context — which is where the DSL
  will actually live.

### 3.3 What the branches have in common

Whichever branch wins, the following remain unchanged:

- The OTel *data model* is the schema of log records. We address
  attributes, resources, severity, body, timestamps, trace context.
  This is the ingest contract, not a design choice.
- The top-level query surface (projection, aggregation, sort,
  limit, time ranges) is Ourios-specific and designed against
  LogQL/Insights idioms. See §5.3.
- Template primitives (`template_id`, `confidence`, etc.) are
  first-class in both branches. See §5.4.
- Compilation target is DataFusion logical plans, with no SQL
  leakage. See §5.5.

### 3.4 Consequences of each branch

| Dimension | Branch A (borrow) | Branch B (distance) |
|---|---|---|
| Onboarding for Collector-literate SREs | Near-zero | Mild (new syntax, familiar semantics) |
| Onboarding for SDK users | Same in both (OTel data model) | Same |
| Maintenance cost | Track pinned OTTL version, amend on bumps | Own every syntactic decision |
| Risk of semantic confusion (same-syntax, different-meaning) | Real | Avoided |
| Size of the spec we maintain | Smaller (borrow) | Larger (own) |
| Ecosystem signalling | Aligned with OTel | Independent |
| Design freedom | Constrained by OTTL | Unconstrained within OTel data model |

### 3.5 Status: unresolved

This RFC cannot recommend either branch without user research
(§9) and non-author review. Both branches are sketched in §5 in
enough detail for side-by-side comparison. Reviewers: pick one, or
push back on both and propose a third.

## 4. Design principles

These rails apply to both branches.

1. **Familiarity beats cleverness.** A user who reads an Ourios
   query for the first time should understand what it does within
   30 seconds, without reading a reference. Rules out
   heavily-sigil'd and overly abstract forms.
2. **No DataFusion leakage.** Hazard 6. If a surface form forces us
   to say "it's the DataFusion X" to explain it, the form is wrong.
3. **Strict separation of predicate from projection from
   aggregation.** A query has a clear *where*, a clear *return*,
   and a clear *roll-up*. Users should be able to read these
   sections independently.
4. **Template primitives are first-class.** `template_id`,
   `template_version`, `drift`, `confidence`, `lossy_flag` are
   vocabulary, not pseudo-columns.
5. **Every query has a time range, explicitly or by default.** An
   unbounded query is a footgun. Missing a `range` clause implies a
   tenant-configurable default window, never an infinite scan.
6. **Compatibility pledges are written, not implied.** If Branch A
   wins, "OTTL-compatible predicates" is pinned to a specific spec
   version with an enumerated divergence list. If Branch B wins, we
   own our grammar explicitly and version it ourselves. "Inspired
   by" without a pledge is marketing.

## 5. Proposed designs (two branches)

### 5.1 Predicate sublanguage — Branch A (borrow from OTTL)

Predicate expressions and path access follow **OTTL specification
v1.x** (version to be pinned when this RFC freezes) for:

- Path syntax into the OTel log record:
  `resource.attributes["service.name"]`, `attributes["http.method"]`,
  `body`, `severity_number`, `severity_text`,
  `instrumentation_scope.name`, `trace_id`, `span_id`, `flags`,
  `observed_timestamp`, `timestamp`.
- Boolean operators: `and`, `or`, `not`, grouping with parentheses.
- Comparison operators: `==`, `!=`, `<`, `<=`, `>`, `>=`, `=~`
  (regex), `!=~` (regex negation).
- Literal forms: strings, numbers, booleans, nil, duration literals
  (`1h`, `30s`), timestamp literals.
- Function calls: a *documented subset* of OTTL's function library:
  `IsMatch`, `Contains`, `Len`, `Int`, `Double`, `String`. Mutation
  functions (`set`, `delete`, etc.) are excluded — predicates do
  not mutate.

The RFC freezes the accepted subset at acceptance time. Future OTTL
spec versions are not automatically adopted; adoption requires an
amending RFC that enumerates the delta.

**Worked predicate in Branch A**:

```
resource.attributes["service.name"] == "api"
  and severity_number >= 17
  and attributes["http.status_code"] == 500
```

**Divergences from OTTL (Branch A only)** — see §6 for the full
enumerated list.

### 5.2 Predicate sublanguage — Branch B (distance from OTTL)

An Ourios-native predicate grammar designed for query readability.
Still addresses the OTel data model — attributes, resources,
severity, body, timestamps, trace context — but with a syntax chosen
to minimise noise in the query context.

- **Path syntax** into the OTel log record:
  - Top-level fields are bare identifiers: `body`, `severity`,
    `ts`, `trace_id`, `span_id`, `scope`.
  - Resource attributes are prefixed with `resource.`:
    `resource.service.name`, bracketed form
    `resource["service.name"]` when the key contains characters
    that are not valid bare identifiers.
  - Log-record attributes are prefixed with `attr.`:
    `attr.http.status_code`, bracketed form when needed.
  - `severity` accepts either a bare severity identifier
    (`severity >= error`, mapped to severity numbers by the OTel
    severity-text convention) or a numeric form
    (`severity >= 17`).
- **Boolean operators**: `and`, `or`, `not`. Aliases `&&`, `||`,
  `!` accepted for terseness.
- **Comparison operators**: `==`, `!=`, `<`, `<=`, `>`, `>=`,
  `=~`, `!=~`. Same as Branch A.
- **Literal forms**: same as Branch A.
- **Function calls**: a bespoke, named library designed for query
  ergonomics. Initial set: `matches(path, regex)`,
  `contains(path, substring)`, `len(path)`, `starts_with(path, s)`,
  `ends_with(path, s)`. Named to match Rust-ecosystem naming
  conventions, not OTTL's.

**Worked predicate in Branch B** (same semantics as §5.1 example):

```
resource.service.name == "api"
  and severity >= error
  and attr.http.status_code == 500
```

Notice: shorter by roughly a third, identifier-forward where
possible, no forced bracket-quoting for common attributes.

**Trade-off realised**: Branch B is more readable; Branch A is more
transferable. The choice is exactly the one in §3.

### 5.3 Top-level query surface — branch-agnostic

Three candidate surfaces. This decision is separate from §3 — any
of A/B/C can sit on top of either predicate branch.

#### Candidate α — SQL-ish, clause-structured

```
from logs
  where <predicate>
  range ts in [now() - 1h, now()]
  aggregate count() by template_id
  sort count desc
  limit 10
```

Pros: reads like English; clear separation; verbose in a way that
is usually a feature. Cons: drifts from LogQL/Insights idioms.

#### Candidate β — pipe-composable, LogQL-ish

```
{<predicate>}
  | range(-1h, now)
  | count by template_id
  | sort desc
  | limit 10
```

Pros: compact, composable, familiar to LogQL users. Cons: sigils
(`{}`, `|`); deeply nested pipes can become unreadable.

#### Candidate γ — Insights-ish

```
filter <predicate>
range 1h
stats count() by template_id
sort count desc
limit 10
```

Pros: familiar to CloudWatch users; minimal sigil density;
verb-per-line. Cons: forces linebreaks in simple queries; less
composable than β.

*Reviewers: surface choice (α/β/γ) and predicate choice (A/B) are
independent. Both decisions feed §9.*

### 5.4 Template and correctness primitives — branch-agnostic

Present in both branches. Straw-man forms in Candidate α:

```
# Find rows where the miner was not fully confident
where lossy_flag = true

# Find all rows belonging to a template, including drift/aliases
where template_id.resolves_to(X)

# Find templates that have drifted across versions
aggregate count() by template_id where template.drift = true

# Reconstruct the original line (honours lossy_flag)
select render(*) from logs where ...
```

The `resolves_to` construct answers the drift question in RFC 0001
§6.7. Equivalent forms exist under β and γ.

### 5.5 Compilation target — branch-agnostic

Every construct compiles to a DataFusion **LogicalPlan**:

| DSL construct        | DataFusion logical node                    |
|----------------------|--------------------------------------------|
| `from logs`          | `TableScan` on the tenant's log table      |
| `where` / filter     | `Filter`                                   |
| `range`              | `Filter` with a time-column predicate      |
| projection / `select`| `Projection`                               |
| `aggregate` / `stats`| `Aggregate`                                |
| `sort`               | `Sort`                                     |
| `limit`              | `Limit`                                    |
| `render(*)`          | Custom `ProjectionExec` that honours the three-zone model |
| `template_id.resolves_to(X)` | Custom logical node that expands to an `IN` over the alias set |

Most is DataFusion's built-in logical algebra. The two non-trivial
entries — `render` and `resolves_to` — are the only places Ourios
extends DataFusion. Both are branch-agnostic.

### 5.6 Stability and versioning — branch-dependent

Branch A: DSL versioning tracks (a) internal changes and (b) OTTL
compatibility bumps. An OTTL spec bump is always a DSL major
version.

Branch B: DSL versioning tracks only (a). There is no upstream
spec to shadow. Consequently Branch B has fewer unsolicited major
versions but more deliberated ones.

General rules (both branches):

- Additions (new functions, new pseudo-columns) are minor versions.
- Behavioural changes that could alter a query's result set are
  major versions, require an RFC amending this one, and ship with
  a deprecation window.

## 6. Enumerated divergences from OTTL (applies only if Branch A wins)

Known deviations from the pinned OTTL spec under Branch A. The list
is exhaustive at RFC freeze; any divergence added later requires an
amending RFC.

| Divergence | Reason |
|---|---|
| No mutation functions (`set`, `delete`, `keep_keys`, `limit_keys`) | Read-only query context |
| No `Statement` top-level form | Queries are not OTTL statements |
| `where` semantics apply to rows, not Collector batches | Different execution model |
| Ourios-specific paths (`template_id`, `confidence`, etc.) | Not in OTel data model |
| Duration literals accept Ourios forms (`1d`, `1w`) | Log time windows coarser than Collector processing |

If Branch B wins, this section is deleted and replaced with a
"Grammar specification" appendix owned by this RFC.

## 7. Testing strategy

Mapping to `CLAUDE.md` §6.2.

- **Unit tests** for the parser: every documented syntactic form
  has a positive and negative test.
- **Property tests**: generate random well-formed queries,
  round-trip through parser → AST → serialised form → parser,
  assert idempotence.
- **Compilation tests**: every DSL construct has a golden-file test
  of the resulting DataFusion LogicalPlan.
- **End-to-end query tests**: against the corpora from
  `docs/benchmarks.md` §1, pin expected query results; any change
  is a PR-visible diff.
- **Branch-specific:**
  - *Branch A only*: OTTL compatibility tests — the pinned OTTL
    predicate corpus is imported; every supported predicate parses
    and produces the expected AST; every unsupported predicate
    fails with a specific error message linking to §6.
  - *Branch B only*: grammar golden-file tests — every syntactic
    form is exercised against a committed-to-repo reference
    grammar snapshot; deviations are visible in PR diffs.

## 8. Alternatives considered (to the DSL as a whole)

These are alternatives that would replace *both* §5.1/§5.2, not
just one branch.

### 8.1 Pure SQL (DataFusion dialect)

- **Pros**: zero parser implementation; SQL users immediately
  productive; DataFusion's planner already supports it.
- **Cons**: violates hazard 6. Exposes SQL features we cannot
  safely permit (cross-tenant joins, unbounded windows, recursive
  CTEs) without a sandboxing layer that is its own project. Binds
  user-facing surface to DataFusion evolution.
- **Verdict**: rejected as the default. Possible future
  advanced-mode escape hatch, gated and sandboxed, under a
  separate RFC.

### 8.2 LogQL clone

- **Pros**: well-understood logs DSL; Loki migrators
  immediately productive.
- **Cons**: LogQL uses label selectors, which are less expressive
  than the OTel log record. Adopting LogQL means projecting OTel
  attributes into labels, losing structure and lying about the
  ingest contract.
- **Verdict**: rejected as the full DSL. Its top-level query shape
  survives as Candidate β in §5.3.

### 8.3 CloudWatch Logs Insights clone

- **Pros**: familiar to a large AWS population; clear
  verb-per-line; explicit aggregation idioms.
- **Cons**: proprietary syntax, no open spec to pin; attribute
  model differs from OTel.
- **Verdict**: rejected as the full DSL. Its readability idioms
  survive as Candidate γ in §5.3.

### 8.4 Full-custom DSL

- **Pros**: maximum fit; no external compatibility constraints.
- **Cons**: highest learning cost for every user regardless of
  background. Violates principle §4.1. "Own every decision" with
  no upstream pressure.
- **Verdict**: rejected as a green-field approach. Branch B is
  *not* full-custom — it borrows the top-level surface from
  LogQL/Insights and the data model from OTel; only the predicate
  syntax is Ourios-native. Full-custom would replace the query
  surface too.

## 9. Testing the premise (user research)

This RFC makes claims about what users will find familiar. Those
claims are currently unvalidated. Before acceptance, the RFC
requires at least:

- A **user persona document** identifying three-to-five archetypes
  (e.g. "SRE running a Collector pipeline", "Go developer
  debugging their service", "analyst exploring historical
  incidents"), with their expected prior DSL exposure. The
  Branch A/B decision pivots heavily on the weight of the
  Collector-operator segment among these.
- A **readability test** on 10–20 sample queries across:
  - each surface candidate (α/β/γ),
  - both predicate branches (A/B),
  with non-author reviewers scoring them on first-read
  comprehensibility and semantic-guess accuracy.
- A **migration feasibility sketch** from LogQL, Insights, and
  OTTL transformations into each branch × surface combination.

Skipping this step and picking on instinct is the path where we
build something elegant that nobody wants to use.

## 10. Open questions

*Must be resolved before status flips to `accepted`. The first
question is the prior decision; the rest are downstream.*

- [ ] **Prior: Branch A (borrow from OTTL) or Branch B (distance
      from OTTL)?** Decided by §9 user research + explicit
      reviewer vote.
- [ ] Which candidate surface (α, β, γ) wins, and under what
      criteria?
- [ ] If Branch A: which OTTL spec version is pinned, and how are
      upstream bumps handled operationally?
- [ ] If Branch B: does the bare-identifier severity form
      (`severity >= error`) collide with OTel severity-text
      casing conventions?
- [ ] Is there a `--sql` advanced-mode escape hatch, and if so,
      what are its safety constraints?
- [ ] How are saved queries and dashboards versioned across DSL
      major versions?
- [ ] Does the DSL expose custom user functions? If so, where do
      they run, and what is the sandboxing story?
- [ ] How does the DSL address the multi-tenant boundary —
      implicit in the executing tenant's context, or explicit in
      the query?
- [ ] Do we expose `params[N]` positional access, or name
      parameters via the template schema?
- [ ] Error messages: how specific can we be about *why* a query
      was rejected (e.g. linking to §6 divergence table under
      Branch A, or to the grammar appendix under Branch B)?
- [ ] Is there a query cost estimator in-path, so users see "this
      will scan 400 GB" before running?
- [ ] Is the DSL rendered identically in the CLI and in the UI,
      or does the UI add affordances the spec does not dictate?
- [ ] Is the DSL designed to be **agent-friendly** — callable from
      a CLI with stable, machine-parseable output schemas (JSON,
      JSONL) suitable for consumption by Claude Code, Cursor,
      Grafana's GCX, or comparable agentic tools? This is a UX
      layer question that does not change the A/B predicate-branch
      decision, but should be answered before §9 user research
      because the answer changes who counts as a "user".

The DSL is the most user-facing surface in the project. More open
questions here is healthier than fewer.

## 11. References

- OpenTelemetry Transformation Language (OTTL) specification:
  https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/main/pkg/ottl
  (pin a commit at RFC freeze under Branch A; reference-only under
  Branch B).
- LogQL (Grafana Loki): https://grafana.com/docs/loki/latest/query/
- CloudWatch Logs Insights:
  https://docs.aws.amazon.com/AmazonCloudWatch/latest/logs/CWL_QuerySyntax.html
- OpenTelemetry log data model:
  https://opentelemetry.io/docs/specs/otel/logs/data-model/
- OpenTelemetry severity text conventions:
  https://opentelemetry.io/docs/specs/otel/logs/data-model/#field-severitytext
- Apache DataFusion logical plan docs.
- `CLAUDE.md` §4 hazard 6; §3.5 and §3.7.
- RFC 0001 (template miner), specifically §6.1, §6.3, §6.7 for the
  Ourios-specific pseudo-columns used here.
