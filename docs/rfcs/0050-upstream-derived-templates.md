---
rfc: 0050
title: Upstream-derived templates — accepting `log.record.template` and reconciling with semconv
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-08-24
supersedes: —
superseded-by: —
---

# RFC 0050 — Upstream-derived templates

> **Status: `specified` (2026-08-24).** §5 criteria written and testable.
> Prerequisites: RFC 0001 (the miner, `accepted`), RFC 0023 (bounded
> template memory, `green`), RFC 0010 (drift), RFC 0007 (aliases).
> Touches **pillar 2** (§2 of `CLAUDE.md`) and invariants §3.1 (no silent
> merges), §3.2 (parameter cardinality) and §3.3 (bit-identical
> reconstruction), so it is an RFC rather than a patch.

## 1. Summary

OpenTelemetry now templates logs *before* they reach a store. The
collector-contrib **`drainprocessor`** (alpha, logs) runs the same Drain
algorithm Ourios's pillar-2 miner runs and annotates each record with
`log.record.template` — plus optional
`log.record.template.parameter.<name>` and
`log.record.template.wildcards`. Its README states the attribute
"aligns with the proposed OTel attribute in
[semantic-conventions#1283][sc1283] and [#2064][sc2064]".

Today Ourios ignores that entirely: the attribute is stored as an
ordinary log attribute and the miner re-derives its own template from
the body. A deployment that templates upstream therefore pays for
clustering twice and ends with **two clusterings that can disagree** —
and the one Ourios keeps is the *less* portable of the two, because
`template_id` is tenant-local while the string converges across
instances.

This RFC makes the upstream template usable and reconciles the two
worlds:

1. **Accept it** — opt-in, per deployment: when a record carries
   `log.record.template`, adopt that string as the record's template
   instead of mining the body (§3.2).
2. **Reconcile it** — an adopted template is interned in the same
   registry, gets the same `template_id` key, and records its
   **provenance**, so drift (RFC 0010), aliases (RFC 0007) and the
   bounded budget (RFC 0023) all keep working across both origins
   (§3.3, §3.5).
3. **Align the vocabulary, and carry both identities** — the portable
   string under the convention's name (`log.record.template`) and the
   local key under ours (`ourios.template.id`), because they answer
   different questions and a consumer that has only one is stuck
   (§3.6).

Nothing about `template_id` itself changes: it stays the Ourios-derived,
tenant-local key the Parquet layout and the pruning path are built on.

## 2. Motivation

### 2.1 The interface our thesis rests on is being standardised

Pillar 2 says log lines collapse to `(template_id, params)` at ingest.
That was a bet that *someone* must do the collapsing and the store is
the natural place. The bet is half-won: the ecosystem agrees templates
matter, and has started producing them one hop earlier. A store that
cannot consume an upstream template is, from the operator's point of
view, insisting on redoing work their pipeline already did.

### 2.2 Two *unlinked* clusterings are worse than either

If the Collector templates and Ourios re-mines, a deployment gets two
independent trees over the same corpus with different thresholds and
different masking rules. Filtering rules written against
`log.record.template` upstream do not select the same rows as
`template_id == N` downstream. Nobody can tell which is "the" template
for a line, and the drift query answers a question about only one of
them. The problem is not that two clusterings exist — it is that
nothing connects them. Linked (§3.2 `observe`), the pair is *richer*
than either alone: the upstream string gives our template a portable
identity, and disagreement between the two trees is itself a signal.

### 2.3 The portable identity is the string, and we do not expose one

`template_id` is a local key: an RFC 0023 eviction, a re-mint or a
second store gives a different id for the same shape — which is exactly
why RFC 0010 makes drift a first-class query and RFC 0007 adds aliases.
The drainprocessor documents the complementary property for the string:
it "converges to the same value across instances given the same
configuration and log patterns". Adopting the convention gives Ourios
something it currently lacks: a template identity that means the same
thing outside one tenant of one store.

## 3. Proposed design

**The positioning, before the mechanics.** The miner is the
foundation and stays the foundation: it is always present, it is the
component that carries every guarantee this RFC relies on —
reconstruction (§3.4 *is* the miner's alignment machinery), confidence
scoring, the RFC 0023 budget, the corpus gates — and Ourios remains
fully functional, byte for byte, with **zero** upstream templating: no
Collector, no processor, nothing in front of it. Upstream templates are
an input Ourios can **leverage**, never a dependency, and the coupling
is exactly one attribute read at ingest. The three modes below form a
dial, not a migration: `ignore` (no leverage), `observe` (leverage the
string, keep our clustering), `adopt` (leverage the clustering too).
A deployment can sit on any of them indefinitely.

### 3.1 The attribute Ourios reads, and the grammar it must be in

`log.record.template` (string) on the log record. When present, usable
(below) and adoption is enabled, it is the record's template. The
companion attributes are read only as described in §3.4; neither is
required.

**The convention does not define a syntax, so this RFC does.** Upstream
deliberately split the question out: #2064 proposed a
`log.record.template.syntax` attribute and the SIG deferred it, which
leaves `log.record.template` a bare string that might be Drain output
(`user <*> logged in`), printf (`User %s logged in`), message-templates
(`User {user} logged in`) or an f-string. Guessing between them is how a
store silently mis-parses a line, so v1 accepts exactly one shape and
refuses the rest:

- **Wildcards.** `<*>`, and `<name>` for a named mask token (the
  drainprocessor's `masking_rules` emit these). Each matches exactly
  **one token** — a maximal run of bytes containing no delimiter — under
  the miner's own tokenisation, so alignment is deterministic and needs
  no backtracking.
- **Literals.** Every other byte is literal and must match the body
  byte for byte. Matching is over **UTF-8 bytes**: no normalisation, no
  case folding, no whitespace collapsing.
- **Rejected outright** (never adopted, mined instead): any other
  placeholder syntax (`%s`, `{}`, `{name}`, `$var`), two adjacent
  wildcards (ambiguous split), a wildcard that would match zero tokens,
  a literal segment that does not appear in the body in order, and any
  template that leaves body bytes unconsumed at the end.
- **Repeats are positional.** The same mask name appearing twice yields
  two parameters in template order; the drainprocessor's own
  first-match-wins collapsing of `parameter.<name>` is exactly why its
  parameter attributes are a cross-check and not the source (§3.4).

When the upstream syntax attribute lands, accepting a second syntax is
an additive change to this section.

### 3.2 Three modes: ignore, observe, adopt

```yaml
miner:
  upstream_templates: ignore   # default; `observe` and `adopt` opt in
```

- **`ignore`** (default) — today's behaviour exactly: the attribute is
  an ordinary attribute, the miner mines the body, and the Parquet
  bytes for a given corpus are unchanged. The default must stay this
  way: adopting silently would change every `template_id` in a live
  store and move the corpus gates, which is a migration, not a default.
- **`observe`** — **coexistence: leverage the string, keep our
  clustering.** The miner mines every record exactly as under `ignore`
  — same `template_id`s, same Parquet bytes for the data columns, same
  corpus-gate numbers — and when a record also carries a
  `log.record.template` that passes the byte cap and the §3.1 grammar,
  that string is recorded on the *mined* template's registry entry as
  an **upstream association**. The registry gains, per template, a
  bounded set of associated upstream strings (default 4; overflow is
  counted, not stored — a template attracting many distinct upstream
  strings is a cardinality signal, not data worth keeping). Nothing is
  adopted, no reconstruction gate runs, and the clustering decision
  stays Ourios's. What it buys: every mined template acquires a
  portable identity for §3.6's surfaces, and *disagreement between the
  two trees becomes queryable* — two upstream strings mapping to one
  mined template (their tree is coarser here), or one upstream string
  spread over several mined templates (ours is finer), each a concrete
  place where thresholds or masking rules differ. `observe` is also
  the migration on-ramp: run it, look at the associations, then decide
  whether `adopt` is even worth it for this deployment.
- **`adopt`** — a record carrying a usable `log.record.template`
  (§3.1 grammar, §3.4 reconstruction) skips the Drain tree; its
  template is the upstream string. A record without the attribute is
  mined as before, so a mixed stream works and no producer is forced to
  change. Even here the miner is not idle standby: it is the fallback
  for every rejection in §3.1/§3.4 and the sole engine for unannotated
  records — `adopt` narrows *when* the tree is consulted, never whether
  it exists.

```yaml
miner:
  upstream_template_byte_limit: 8192   # UTF-8 bytes; 0 disables adoption
```

**The string is bounded before any work is done on it.** `max_templates`
(RFC 0023) bounds *interned* templates and `param_byte_limit`
(`CLAUDE.md` §3.2) bounds *extracted* parameters — neither bounds the
inbound attribute, so without a cap a 10 MiB "template" would be
tokenised and aligned before anything rejected it. A value longer than
`upstream_template_byte_limit` (UTF-8 bytes, default 8 KiB) is not
parsed, not aligned and not adopted: the record is mined instead, and
the rejection is counted on the miner's existing parse-outcome metric so
a misbehaving producer is visible rather than silently absorbed.

### 3.3 An adopted template is a first-class template

It is interned in the tenant's registry exactly like a mined one and
receives a `template_id` from the same space; the Parquet schema does
not change (RFC 0005 §3.5 — no migration, and no per-row provenance
column).

**Provenance is a set on the registry entry, not a single value**, over
three origins:

| Origin | Meaning | Trust |
|---|---|---|
| `mined` | Ourios's Drain tree derived it | inference; confidence scored as today |
| `upstream_derived` | a clustering processor derived it (drainprocessor) | inference made elsewhere, same failure modes |
| `producer_declared` | the emitting library's own message template | ground truth — the developer wrote it |

Two notes on that taxonomy. First, the same string can legitimately
arrive both ways: mined on Monday, adopted on Tuesday. A single-valued
field would then depend on ingest order, the later value overwriting the
earlier, so drift and audit answers would differ by replay order. A set,
unioned monotonically, is order-independent, and §5 tests both
directions. Second, **Ourios cannot always tell `upstream_derived` from
`producer_declared`**: the attribute key is the same either way, because
the convention (#1283/#2064) was drafted for producer-declared templates
and the drainprocessor reuses the name for derived ones. Until upstream
distinguishes them, both record as `upstream_derived`; §7 tracks it.

The set is what lets everything downstream stay honest:

- **RFC 0023 budget** — adopted templates count against
  `max_templates` like any other. A tenant at its ceiling stops
  interning new upstream templates and falls back to mining (or
  `NO_TEMPLATE`), so a producer emitting a unique template per record
  cannot grow memory without bound. §3.2 of `CLAUDE.md` in spirit: an
  untrusted-shaped input must not become unbounded cardinality.
- **RFC 0010 drift** — the drift query reports the provenance set, so
  an operator can see a shape whose template changed *because the
  upstream processor changed*, not because our tree moved.
- **RFC 0007 aliases** — an alias may bind a mined template to an
  adopted one, which is the migration path for a deployment that turns
  adoption on over existing history.
- **§3.1 no silent merges** — adoption is a clustering decision made
  elsewhere, so it emits the same audit event a merge does, naming the
  provenance.

### 3.4 Reconstruction decides usability (invariant §3.3)

Ourios's contract is that `render(template, params)` equals the
original line byte for byte, or the row is flagged lossy and the body
retained. An upstream template is a *claim* about the line, produced by
a different tokenizer with its own masking rules — the drainprocessor
itself documents that a mask spanning whitespace makes template and
body impossible to align position-by-position, and skips its parameter
attributes when that happens.

So adoption is conditional on reconstruction, checked per record —
after the §3.2 size cap has already rejected an oversized string, so
none of the work below is proportional to an unbounded input:

1. Align the upstream template against the body to recover the
   parameters and the inter-token separators. `log.record.template.
   wildcards`, when present, is used as a cross-check, never as the
   source of truth — the store verifies rather than trusts.
2. If the alignment reproduces the body byte for byte, adopt: the row
   carries the upstream template with its parameters and separators,
   confidence 1.0.
3. If it does not, **do not** adopt silently. The row falls back to
   mining, and if mining also cannot reconstruct it, the existing
   lossy-flag + body-retention path applies unchanged.

Parameter values remain subject to the per-parameter byte limit that
governs mined parameters — `param_byte_limit` (`CLAUDE.md` §3.2,
default 256, measured in **UTF-8 bytes** of the extracted value) — and
an overflowing value spills to the `body` column exactly as a mined one
does. Adoption changes where a parameter came from, never what bounds
it.

### 3.5 What stays local

`template_id` remains Ourios-derived and tenant-local. It is the
Parquet column, the pruning key and the DSL field, and nothing in this
RFC makes it portable — the portable identity is the string. The
glossary and `docs/architecture/otlp-log-format.md` say so explicitly so
the flat namespace stops implying otherwise.

### 3.6 Vocabulary alignment — carry **both**, and say which is which

The two identifiers answer different questions and neither substitutes
for the other: the **string** says *what shape this line has* and means
the same thing in any store; the **id** says *which row group to read*
and means nothing outside this tenant. A consumer holding only the id
must make a second call to learn the shape (which is what
`docs/guides/agent-telemetry.md` tells operators to do today); a
consumer holding only the string cannot use the pruning path. So where
Ourios names a template *as attributes*, it emits the pair:

| Attribute | Value | Why this name |
|---|---|---|
| `log.record.template` | the template string | the convention (§3.1); a vendor name for a concept the ecosystem is standardising is exactly what `CLAUDE.md`'s alignment rule forbids |
| `ourios.template.id` | the `u64` local key | vendor-namespaced because it *is* vendor-specific — no convention exists or should |
| `ourios.template.version` | the template's version | the companion the drift/alias machinery needs (RFC 0007/0010) |

Note the shape of the id's name: `ourios.template.id`, not
`ourios.template_id`. OTel's naming rules put a property under its
object with a dot (`*{object}.{property}`) and explicitly warn against
`{object}_{property}` "if this object could have other properties" — a
template has at least a version, so the dotted form is the one that
extends. It also matches the namespace the registry already uses for
`ourios.miner.template.count`.

Which surfaces carry them:

- **Telemetry Ourios emits about itself** (a query span that pinned a
  template, miner events): the pair above, registered in
  `semconv/registry/` and exercised by the weaver live-check gate. One
  registration nuance is load-bearing: `log.record.template` **cannot
  be a weaver `ref:`**, because the attribute is not in the upstream
  registry to reference — verified against the registry, whose `log`
  namespace today holds only `log.iostream`, `log.file.*`,
  `log.record.original` and `log.record.uid`. It is therefore a
  **local definition** in our registry: `development` stability, a
  `note` naming #1283/#2064 as the tracked proposal, and the explicit
  caveat that the entry pre-adopts a proposed name exactly as the
  drainprocessor (itself an OTel component) does. Writing a `log.*`
  name in a vendor registry is otherwise something OTel's naming
  guidance warns against; the tracking note is what makes it
  pre-adoption rather than squatting, and the entry collapses to a
  `ref:` the day the proposal lands.
- **Records returned by the query API**: the string is added *beside*
  the existing `template_id` / `template_version` response fields, so a
  consumer stops needing the second `list_templates` call. It is **not**
  injected into the record's `attributes` array — RFC 0018's fidelity
  rule is that the read path returns the attributes ingest stored, and
  a template Ourios derived was never one of them. Ourios-derived data
  stays in Ourios-derived fields.
- **A stored record re-exported over OTLP** (no such surface today):
  the same rule decides it. An *adopted* template may go back out as
  `log.record.template` because the producer sent it; a *mined* one may
  not be presented as though the producer had, and would need an
  explicit derived-data marker. §7 tracks that if the surface ever
  exists.

API surfaces that return a template string as *data* (the template
registry, `list_templates`, the drift query) keep their field names —
they are not OTel attributes, and renaming them is a query-contract
break with no upside. The DSL keeps `template_id` for the same reason
(RFC 0002 contract), now documented as the local key it is.

### 3.7 Named arguments may already be attributes

#2064's accepted direction is that a *named* placeholder does **not**
get a `log.record.template.parameter.` prefix: `{user.id}` is emitted as
a top-level attribute `user.id`, explicitly so that templates reuse
existing semantic conventions (`API Request by {http.request.method}
{url.full} by user {user.id}`). Positional placeholders keep
`log.record.template.parameter.<index>`.

For Ourios that means a producer-declared template can arrive with its
arguments *already stored as attributes* — possibly promoted ones
(RFC 0022) that the DSL can filter and aggregate on directly. Extracting
the same values into `params` would then store them twice: once as an
attribute column, once in the params list.

v1 does not deduplicate. The params list is what `render` consumes, and
invariant §3.3 (bit-identical reconstruction) is worth more than the
bytes saved — Parquet's dictionary encoding absorbs most of the
duplication anyway. But the interaction is worth naming now, because the
tempting optimisation (drop params that duplicate an attribute) would
quietly make reconstruction depend on the attribute set surviving
unchanged, which RFC 0018's fidelity rule guarantees for *stored*
attributes but not for a projection. §7 records it as a measurement to
take once real producer-declared traffic exists.

## 4. Alternatives considered

- **Ignore upstream templates permanently.** Today's behaviour. Cheap,
  and wrong once a deployment runs the drainprocessor: two clusterings,
  the portable one discarded.
- **Adopt by default.** Every `template_id` in a live store changes the
  moment a producer adds the attribute, corpus gates move, and the
  change arrives without an operator asking for it. Rejected — opt-in
  with an alias path (§3.3) is the migration-safe shape.
- **Trust the upstream parameters/wildcards verbatim.** Faster, and it
  breaks invariant §3.3 the first time a mask spans whitespace: the
  store would emit a template it cannot render back to the line. The
  wildcards stay a cross-check (§3.4).
- **Replace the miner with the drainprocessor.** Would make Ourios
  depend on a Collector in the ingest path and give up the corpus gates
  and confidence scoring that pillar 2's correctness rests on. The
  miner stays; upstream templates are an input, not a substitute.
- **Only `ignore` and `adopt`, no middle mode.** The first draft of
  this RFC. It forces a false choice — no leverage at all, or hand the
  clustering to a component we do not run — and gives a deployment no
  way to *evaluate* upstream quality before committing. `observe` is
  the coexistence the design is actually after: the miner stays the
  foundation, the upstream string is leveraged as identity and as a
  comparison signal, and adoption becomes a decision made on evidence.
- **Emit `ourios.template.string` instead of the conventional name.**
  Inventing a vendor name for a concept the ecosystem is standardising
  is exactly what `CLAUDE.md`'s OTel-alignment rule exists to prevent.

## 5. Acceptance criteria

Scenario ids `RFC0050.<n>`. Nine criteria (RFC0050.1–.9).

> **RFC0050.1 — the default changes nothing.** Given
> `upstream_templates` unset and a corpus whose records carry
> `log.record.template`, When it is ingested, Then the attribute is
> stored as an ordinary attribute like any other, and the result is
> byte-identical to **the same corpus ingested by the pre-RFC build**
> — the comparison is against today's behaviour on the same input, not
> against a different corpus with the attribute stripped, which would
> of course differ by that attribute's own bytes. Every `template_id`,
> every miner-derived column and the file layout are unchanged.

> **RFC0050.2 — adoption uses the upstream string.** Given
> `upstream_templates: adopt` and records carrying
> `log.record.template`, When they are ingested, Then each record's
> template is the upstream string, two records sharing a string share
> one `template_id`, the Drain tree gains no leaf for them, and the
> registry entry's provenance set contains `upstream_derived`.

> **RFC0050.3 — a mixed stream works.** Given `adopt` and a stream where
> only some records carry the attribute, Then annotated records adopt
> and unannotated ones are mined, in one tenant, with both provenances
> visible in the registry and both queryable by `template_id`.

> **RFC0050.4 — the grammar and reconstruction gate adoption
> (invariant §3.3).** Given `adopt`, When a record carries a template
> outside the §3.1 grammar — `%s`, `{}`, `{name}`, `$var`, adjacent
> wildcards, a literal segment absent from the body, trailing
> unconsumed body bytes — Then it is **not** adopted and the record is
> mined as if the attribute were absent; And when a grammatical
> template cannot be aligned byte for byte (a mask spanning
> whitespace, a template from a different line), Then likewise, and if
> mining also cannot reconstruct the row it is flagged lossy with the
> body retained. For every adopted row,
> `render(template, params, separators)` equals the original body byte
> for byte — asserted as a property test over the corpus, with
> alignment matching **UTF-8 bytes** and each wildcard consuming
> exactly one token.

> **RFC0050.5 — both bounds hold, and the string is bounded first.**
> Given `adopt`, a `max_templates` of N and a producer emitting a
> unique `log.record.template` per record, When 10·N records are
> ingested, Then the tenant's template count never exceeds N, memory
> stays bounded, the overflow path is the documented fallback (mining,
> then `NO_TEMPLATE`), and the ceiling is observable on the existing
> miner metrics; And Given a record whose `log.record.template` exceeds
> `upstream_template_byte_limit`, Then it is rejected **before**
> tokenisation or alignment — no work proportional to its length — the
> record is mined instead, and the rejection is counted.

> **RFC0050.6 — provenance is a set, and order cannot change it.**
> Given a template string that arrives **mined first, adopted second**,
> and the same string in a second tenant **adopted first, mined
> second**, Then both registry entries end with the same provenance set
> `{mined, upstream_derived}` and one `template_id` each — the answer
> does not depend on ingest order; And the RFC 0010 drift query reports
> the set, an RFC 0007 alias can bind a mined template to an adopted
> one, and adoption emits the `CLAUDE.md` §3.1 audit event naming the
> template and its origin.

> **RFC0050.7 — the vocabulary is the convention's.** Given any Ourios
> telemetry that names a template string as an attribute, Then the key
> is `log.record.template`, the name resolves in `semconv/registry/`,
> and the weaver live-check reports no violation for it; And Given the
> local key on the same signal, Then it is `ourios.template.id` (with
> `ourios.template.version`), registered as vendor attributes.

> **RFC0050.8 — the read path carries both, without inventing
> attributes.** Given a stored record whose template is known, When it
> is returned by the query API, Then the response carries the template
> **string** beside the existing `template_id` / `template_version`
> fields — so no second `list_templates` call is needed — And the
> record's `attributes` array is byte-identical to what ingest stored
> (RFC 0018 fidelity): no `log.record.template` is injected into a
> record whose producer did not send one, and one that *was* sent
> survives the round trip unchanged.

> **RFC0050.9 — observe leverages without touching the clustering.**
> Given `upstream_templates: observe` and a corpus whose records carry
> valid upstream templates, When it is ingested, Then every
> `template_id`, every miner-derived column and the corpus-gate numbers
> are identical to the same corpus under `ignore` — the clustering is
> untouched — And the registry's mined entries carry the associated
> upstream strings; And Given records mapping two upstream strings onto
> one mined template and one upstream string across two mined
> templates, Then both disagreement shapes are visible in the registry;
> And Given more distinct upstream strings for one template than the
> association bound, Then the set stays at the bound and the overflow
> is counted.

## 6. Testing strategy

Unit: the alignment routine (template ⇄ body) as a table — exact match,
mask spanning whitespace, template from a different line, a template
longer than the body, parameters over `param_byte_limit`. Property
(`proptest`): for any adopted row, render equals the original body, or
the row is lossy with the body retained (RFC0050.4). Corpus: the RFC
0024 harness run with `adopt` over a corpus pre-annotated by an actual
`drainprocessor` pass, asserting template count, reconstruction
accuracy and the RFC 0023 ceiling (RFC0050.2/.5). Integration: a mixed
stream through the served binary (RFC0050.3); the byte-identical
default asserted by ingesting the same corpus twice (RFC0050.1);
`observe` asserted by diffing its miner-derived output against
`ignore`'s on the same corpus (RFC0050.9). The weaver live-check job
covers RFC0050.7.

## 7. Open questions

- [ ] **Final attribute name.** semantic-conventions #1283 / #2064 are
      open; `log.record.template` is what collector-contrib ships today.
      If the convention lands renamed, this RFC's registry entry is the
      one place to change *for telemetry* — and OTel schema files
      formally describe attribute renames, so the rename will arrive
      with a machine-readable transformation to follow. The *stored
      data* side is decided policy, not a design gap, and stays open
      here only until the upstream name lands:
      - **Pre-production** (the current posture): a rename is a
        `!`-marked breaking change and old files are simply
        regenerated. No dual-read, no migration tooling — the standing
        rule for persisted layouts before a production deployment
        exists.
      - **Post-production**: old files keep the old column and readers
        already tolerate absent/unknown columns (§3.5 schema-evolution
        invariant), so nothing breaks on read. Query-side, the
        promoted-attribute configuration gains an alias entry (old name
        → canonical) resolved at planning time — the same shape as
        template aliases (RFC 0007, hazard #5) — with the OTel schema
        file as the authoritative mapping rather than one we invent.
        Physical convergence rides compaction re-projection (RFC 0022),
        which rewrites old files under the new column as they are
        compacted anyway; no dedicated migration tool.
      Nothing is built ahead of need: the only trigger for any of this
      is the upstream convention actually landing.
- [x] **Trust boundary — bounded, then verified.** Resolved in §3.2 and
      §3.4: `upstream_template_byte_limit` caps the string before any
      parsing (RFC 0023's budget bounds interned templates, not inbound
      bytes), the §3.1 grammar rejects everything it cannot parse
      unambiguously, and reconstruction decides adoption. The attribute
      is operator-pipeline data, but none of that trust is load-bearing.

- [ ] **Telling declared from derived.** #1283/#2064 drafted
      `log.record.template` for *producer-declared* message templates
      (`log.info("Message {}", p)`) — ground truth. The drainprocessor
      reuses the name for a *derived* one — an inference. The key is the
      same, so §3.3 records both as `upstream_derived`, and a
      producer-declared template gets less confidence than it deserves.
      The deferred `log.record.template.syntax` (#2064) would
      incidentally settle it, since Drain output and a message template
      are different syntaxes; worth raising there rather than inventing
      a marker.

- [ ] **Duplicate arguments (§3.7).** Once producer-declared traffic
      exists, measure what fraction of `params` duplicates a stored
      attribute under #2064's named-argument rule, and decide whether
      deduplication is worth making reconstruction depend on the
      attribute set.
- [ ] **Contributing upstream.** Both issues are open and quiet (last
      activity 2026-02; #1283 since 2024-07, #2064 `triage:accepted:
      needs-sig`), and neither addresses *derived* templates at all —
      the drainprocessor took the name for a concept the convention was
      not drafted for. Two things this project has that the discussion
      lacks: the **declared-versus-derived distinction** above, and the
      **reconstruction property** — that a template is only safe to
      rely on if `render(template, args)` reproduces the line byte for
      byte, which is invariant §3.3 here and is property-tested over a
      real corpus (RFC 0024). Worth contributing rather than only
      consuming; per `feedback: ai-disclosure-at-top-when-posting-
      externally`, any post is maintainer-approved first.

## 8. References

- collector-contrib **`drainprocessor`** — `log.record.template`,
  `log.record.template.parameter.<name>`,
  `log.record.template.wildcards`; masking rules; the
  whitespace-spanning-mask alignment caveat that §3.4 turns into a
  usability gate.
- [semantic-conventions#1283][sc1283], [#2064][sc2064] — the proposed
  template attribute the processor tracks.
- RFC 0001 (miner), RFC 0005 §3.5 (schema stability), RFC 0007
  (aliases), RFC 0010 (drift), RFC 0023 (bounded template memory),
  RFC 0024 (property + corpus testing).
- `CLAUDE.md` §2 pillar 2, §3.1–§3.3.

[sc1283]: https://github.com/open-telemetry/semantic-conventions/issues/1283
[sc2064]: https://github.com/open-telemetry/semantic-conventions/issues/2064
