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
status: draft | accepted | rejected | superseded
author: Name <email>
drafting-assistance: Claude   # omit if no LLM drafted
created: YYYY-MM-DD
supersedes: —                 # or RFC NNNN
superseded-by: —              # or RFC NNNN
---
```

## Required sections

Every RFC has at least:

1. **Summary** — 3–5 sentences. The commitment, not the rationale.
2. **Motivation** — why this change now, and why at this layer.
3. **Proposed design** — precise enough that two engineers would
   produce the same implementation.
4. **Alternatives considered** — one paragraph each. "I have not
   heard of it" is not acceptable.
5. **Testing strategy** — mapped to `CLAUDE.md` §6.2.
6. **Open questions** — everything unresolved, as a checklist.
7. **References** — paper citations, related RFCs, `CLAUDE.md`
   sections constrained.

Additional sections are welcome when they clarify. Do not pad for the
sake of the template.

## Lifecycle

1. **Draft** — PR opened with status `draft`. Discussion happens in PR
   review. The RFC is not yet binding.
2. **Accepted** — maintainer approval, status flipped to `accepted` in
   the same or a follow-up PR. Implementation may begin.
3. **Superseded** — a later RFC replaces part or all of this one. Both
   frontmatters are updated. The superseded RFC is not deleted.
4. **Rejected** — closed PR or status flipped to `rejected`. The file
   is kept for the record.

## Relationship to architecture docs

An accepted RFC is a contract for how something will be built. Once
the subsystem is stable, the RFC graduates to
`docs/architecture/<subsystem>.md` — a living document describing the
system as it actually is. The RFC stays in place as the historical
decision record; the architecture doc is what a new contributor reads
first.
