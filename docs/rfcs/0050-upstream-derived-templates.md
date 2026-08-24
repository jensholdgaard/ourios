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
3. **Align the vocabulary** — where Ourios exposes a template *string*
   as an attribute, the name is `log.record.template`, tracking the
   convention rather than inventing one (§3.6).

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

### 2.2 Two clusterings are worse than either

If the Collector templates and Ourios re-mines, a deployment gets two
independent trees over the same corpus with different thresholds and
different masking rules. Filtering rules written against
`log.record.template` upstream do not select the same rows as
`template_id == N` downstream. Nobody can tell which is "the" template
for a line, and the drift query answers a question about only one of
them.

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

### 3.1 The attribute Ourios reads

`log.record.template` (string) on the log record, as written by the
drainprocessor or any producer following the same convention. When
present and adoption is enabled, it is the record's template. The
companion attributes are read only as described in §3.4; neither is
required.

### 3.2 Adoption is opt-in

```yaml
miner:
  upstream_templates: ignore   # default; `adopt` opts in
```

- **`ignore`** (default) — today's behaviour exactly: the attribute is
  an ordinary attribute, the miner mines the body, and the Parquet
  bytes for a given corpus are unchanged. The default must stay this
  way: adopting silently would change every `template_id` in a live
  store and move the corpus gates, which is a migration, not a default.
- **`adopt`** — a record carrying a usable `log.record.template`
  (§3.4) skips the Drain tree; its template is the upstream string.
  A record without the attribute is mined as before, so a mixed stream
  works and no producer is forced to change.

### 3.3 An adopted template is a first-class template

It is interned in the tenant's registry exactly like a mined one and
receives a `template_id` from the same space; the Parquet schema does
not change (RFC 0005 §3.5 — no migration). The registry entry records
**provenance** (`mined` | `upstream`), which is what lets everything
downstream stay honest:

- **RFC 0023 budget** — adopted templates count against
  `max_templates` like any other. A tenant at its ceiling stops
  interning new upstream templates and falls back to mining (or
  `NO_TEMPLATE`), so a producer emitting a unique template per record
  cannot grow memory without bound. §3.2 of `CLAUDE.md` in spirit: an
  untrusted-shaped input must not become unbounded cardinality.
- **RFC 0010 drift** — the drift query reports provenance, so an
  operator can see a shape whose template changed *because the
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

So adoption is conditional on reconstruction, checked per record:

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

Parameter values remain subject to the §3.2 byte limit and spill to
`body` exactly as mined parameters do.

### 3.5 What stays local

`template_id` remains Ourios-derived and tenant-local. It is the
Parquet column, the pruning key and the DSL field, and nothing in this
RFC makes it portable — the portable identity is the string. The
glossary and `docs/architecture/otlp-log-format.md` say so explicitly so
the flat namespace stops implying otherwise.

### 3.6 Vocabulary alignment

Where Ourios exposes a template string *as an attribute* — today: none;
tomorrow: any span, metric or log attribute naming a template — the name
is `log.record.template`, not an `ourios.*` invention, because the
convention exists and a store should speak it. Two consequences:

- The name is registered in `semconv/registry/` as a reference to the
  upstream attribute (the project's standing rule: query the OTel MCP
  and check for collisions before adding any name), and the weaver
  live-check gate exercises it.
- If semantic-conventions #1283 / #2064 lands with a different final
  name, this RFC's §7 tracks the rename; the registry entry is the
  single place it changes.

API surfaces that return a template string as *data* (the template
registry, `list_templates`, the drift query) keep their field names —
they are not OTel attributes, and renaming them is a query-contract
break with no upside.

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
- **Emit `ourios.template.string` instead of the conventional name.**
  Inventing a vendor name for a concept the ecosystem is standardising
  is exactly what `CLAUDE.md`'s OTel-alignment rule exists to prevent.

## 5. Acceptance criteria

Scenario ids `RFC0050.<n>`. Seven criteria (RFC0050.1–.7).

> **RFC0050.1 — the default is byte-identical.** Given
> `upstream_templates` unset and a corpus whose records carry
> `log.record.template`, When it is ingested, Then the attribute is an
> ordinary promoted/unpromoted attribute, every `template_id` matches
> the same corpus ingested without the attribute, and the Parquet data
> files are byte-identical to today's.

> **RFC0050.2 — adoption uses the upstream string.** Given
> `upstream_templates: adopt` and records carrying
> `log.record.template`, When they are ingested, Then each record's
> template is the upstream string, two records sharing a string share
> one `template_id`, the Drain tree gains no leaf for them, and the
> registry reports `provenance = upstream`.

> **RFC0050.3 — a mixed stream works.** Given `adopt` and a stream where
> only some records carry the attribute, Then annotated records adopt
> and unannotated ones are mined, in one tenant, with both provenances
> visible in the registry and both queryable by `template_id`.

> **RFC0050.4 — reconstruction gates adoption (invariant §3.3).** Given
> `adopt` and an upstream template that cannot be aligned to its body
> byte for byte (a mask spanning whitespace, a template from a
> different line), When the record is ingested, Then it is **not**
> adopted: it falls back to mining, and if that also cannot reconstruct
> it the row is flagged lossy with the body retained. For every adopted
> row, `render(template, params, separators)` equals the original body
> byte for byte — asserted as a property test over the corpus.

> **RFC0050.5 — the budget holds (RFC 0023).** Given `adopt`, a
> `max_templates` of N and a producer emitting a unique
> `log.record.template` per record, When 10·N records are ingested,
> Then the tenant's template count never exceeds N, memory stays
> bounded, the overflow path is the documented fallback (mining, then
> `NO_TEMPLATE`), and the ceiling is observable on the existing miner
> metrics.

> **RFC0050.6 — provenance is visible and audited.** Given an adopted
> template, Then the RFC 0010 drift query reports its provenance, an
> RFC 0007 alias can bind a mined template to it, and adoption emits
> the §3.1 audit event naming both the template and its origin.

> **RFC0050.7 — the vocabulary is the convention's.** Given any Ourios
> telemetry that names a template string as an attribute, Then the key
> is `log.record.template`, the name resolves in `semconv/registry/`,
> and the weaver live-check reports no violation for it.

## 6. Testing strategy

Unit: the alignment routine (template ⇄ body) as a table — exact match,
mask spanning whitespace, template from a different line, a template
longer than the body, parameters over the §3.2 byte limit. Property
(`proptest`): for any adopted row, render equals the original body, or
the row is lossy with the body retained (RFC0050.4). Corpus: the RFC
0024 harness run with `adopt` over a corpus pre-annotated by an actual
`drainprocessor` pass, asserting template count, reconstruction
accuracy and the RFC 0023 ceiling (RFC0050.2/.5). Integration: a mixed
stream through the served binary (RFC0050.3); the byte-identical
default asserted by ingesting the same corpus twice (RFC0050.1). The
weaver live-check job covers RFC0050.7.

## 7. Open questions

- [ ] **Final attribute name.** semantic-conventions #1283 / #2064 are
      open; `log.record.template` is what collector-contrib ships today.
      If the convention lands renamed, this RFC's registry entry is the
      one place to change — but a store that has stored the old name in
      *data* (as a promoted attribute) also needs an alias story.
- [ ] **Trust boundary.** The attribute arrives from the pipeline, which
      is operator-controlled, so §3.4's reconstruction check is the only
      validation this RFC specifies. Is a template-string byte cap
      (separate from the §3.2 parameter limit) worth adding, or does the
      RFC 0023 budget cover the abuse case adequately?
- [ ] **Contributing upstream.** Ourios has run Drain in anger over real
      corpora (RFC 0001 §5, the RFC 0024 calibration manifest). Some of
      that — the confidence scoring, the reconstruction property, the
      merge-audit discipline — is directly relevant to #1283/#2064 and
      to the drainprocessor itself. Worth a contribution rather than
      only consumption?

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
