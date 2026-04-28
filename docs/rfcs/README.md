# RFCs

> Referenced from `CLAUDE.md` §5.1. This document is the minimum
> viable RFC process for Ourios. It will grow as the project does.

## When an RFC is required

Per `CLAUDE.md` §5.1, an RFC precedes implementation for any change
that touches:

- An architectural pillar (`CLAUDE.md` §2).
- An invariant (`CLAUDE.md` §3).
- A hazard (`CLAUDE.md` §4 / `docs/hazards.md`).
- The on-disk Parquet schema (`CLAUDE.md` §3.5).
- A new crate (`CLAUDE.md` §7).

Bug fixes, dependency bumps, and internal refactors do not need RFCs.
When in doubt, assume RFC.

## File layout

- Filename: `NNNN-short-kebab-title.md`, e.g. `0001-template-miner.md`.
- Numbers are assigned in merge order. Draft PRs may use the next free
  number provisionally; if two drafts collide, the later-merged one
  renumbers.
- One file per RFC. Supersessions are recorded in the frontmatter of
  both the old and new RFC.

## Required frontmatter

```yaml
---
rfc: NNNN
title: Short descriptive title
status: drafted | specified | red | green | validated | accepted | rejected | superseded
author: Name <email>
drafting-assistance: Claude   # omit if no LLM drafted
created: YYYY-MM-DD
supersedes: —                 # or RFC NNNN
superseded-by: —              # or RFC NNNN
---
```

The maturity stages (`drafted` through `validated`) are gates an RFC
moves through before it becomes binding; `accepted` is the terminal
post-maintainer-signoff state; `rejected` and `superseded` are the
off-ramps. See `docs/verification.md` §3.

## Required sections

Every RFC has at least:

1. **Summary** — 3–5 sentences. The commitment, not the rationale.
2. **Motivation** — why this change now, and why at this layer.
3. **Proposed design** — precise enough that two engineers would
   produce the same implementation.
4. **Alternatives considered** — one paragraph each. "I have not
   heard of it" is not acceptable.
5. **Acceptance criteria** — normative scenarios, one per invariant
   or hazard the RFC touches. Format: structured prose with
   `Given / When / Then / And` leading clauses; each scenario carries
   an id of the form `H1.1`, `§3.4.2`, or `RFC<NNNN>.<m>`, referenced
   from the test code so the mapping is greppable. See
   `docs/verification.md` §2.
6. **Testing strategy** — mapped to `CLAUDE.md` §6.2; references the
   §5 scenario ids and names the technique (`proptest`, corpus,
   `criterion`) for each.
7. **Open questions** — everything unresolved, as a checklist.
8. **References** — paper citations, related RFCs, `CLAUDE.md`
   sections constrained.

Additional sections are welcome when they clarify. Do not pad for the
sake of the template.

## Lifecycle

The five-stage maturity model. An RFC moves through these stages
before becoming binding; the `status:` frontmatter field tracks the
current stage so reviewers and tooling see it without reading the
body.

1. **Drafted** — PR opened with status `drafted`. Sections §§1–4 and
   §§7–8 are filled. Discussion happens in PR review.
2. **Specified** — §5 acceptance criteria are written, every
   invariant and hazard the RFC touches has at least one scenario,
   and review has confirmed the criteria are testable in principle.
3. **Red** — test stubs exist and fail. Implementation may begin.
4. **Green** — all acceptance criteria pass; unit + property + corpus
   tests green.
5. **Validated** — thesis-gates in `docs/benchmarks.md` §7 pass on
   representative corpora. Maintainer flips status to `accepted`.

A regression detected after `Validated` either reopens the RFC (if a
criterion is invalidated) or spawns a tuning RFC per `benchmarks.md`
§7 (if a thesis-gate degrades). See `docs/verification.md` §3.

Two terminals reachable from any stage:

- **Superseded** — a later RFC replaces part or all of this one.
  Both frontmatters are updated. The superseded RFC is not deleted.
- **Rejected** — closed PR or status flipped to `rejected`. The
  file is kept for the record.

## Diagrams

When an RFC needs a diagram (state machine, sequence flow, schema
relationship, decision tree), it is authored in **Mermaid**, embedded
as a fenced ` ```mermaid ` block in the markdown. Mermaid is chosen
for the same reasons we chose markdown over a binary doc format:
text-based source is reviewable in PR diffs, version-controllable,
and lets the RFC itself remain a single self-contained file.

Lectures (`docs/talks/`) use a different convention: hand-drawn
SVGs (Excalidraw export, or hand-authored to match) committed under
`docs/talks/img/`. Lectures benefit from a "manuscript / blackboard"
aesthetic that Mermaid does not provide; RFCs benefit from the
diff-ability that Excalidraw does not provide. Do not mix the two
conventions.

The mdBook build does not yet have the `mdbook-mermaid` preprocessor
enabled — it will be added the first time an RFC actually needs a
diagram, at which point local builds and the CI workflow both gain
the preprocessor in the same change.

## Relationship to architecture docs

An accepted RFC is a contract for how something will be built. Once
the subsystem is stable, the RFC graduates to
`docs/architecture/<subsystem>.md` — a living document describing the
system as it actually is. The RFC stays in place as the historical
decision record; the architecture doc is what a new contributor reads
first.
