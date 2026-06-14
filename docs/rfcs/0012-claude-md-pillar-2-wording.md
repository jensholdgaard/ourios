---
rfc: 0012
title: "meta: CLAUDE.md §2 pillar-#2 wording — template mining's 50–200× is a logical reduction, not on-disk bytes"
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-06-14
supersedes: —
superseded-by: —
---

# RFC 0012 — meta: CLAUDE.md §2 pillar-#2 wording

> **This is a `meta:` RFC.** It proposes a change to `CLAUDE.md`, which
> its own footer declares load-bearing: *"This document is load-bearing;
> further changes require a `meta:` RFC and majority maintainer
> approval."* Per `CLAUDE.md` §8.5 (cache discipline) the edit is **not**
> made in the drafting session — this RFC specifies the exact change; a
> maintainer enacts it after approval. Precedent: the §6.2
> "tests are specifications" bullet, added via an informal `meta:` RFC
> waiver (commit `b50067d`, 2026-05-13). This RFC follows the same path,
> written out in full rather than as an informal waiver.

## 1. Summary

`CLAUDE.md` §2 pillar #2 currently reads: *"Log lines collapse to
`(template_id, params)` at ingest time. This is where the 50–200×
compression comes from — before any byte-level codec runs."* That phrasing
reads as an **on-disk-bytes** claim: that template mining alone yields
50–200× smaller files than the raw corpus, ahead of (and independent of) a
byte codec. RFC 0011 (`accepted`) established by measurement that this is
false — a whole-stream byte codec (zstd) captures the same redundancy, so
on disk Ourios does not beat zstd (the A1 gate is refuted and demoted to a
diagnostic). Template mining's 50–200× is a **logical** reduction (each
line becomes one row keyed by a small, stable `template_id`), and its
*value* is realised as **query pruning** — the benchmark gates B1/B2 — not
as fewer on-disk bytes than a codec. This RFC amends pillar #2 to say so,
so the project's canonical thesis statement matches its measured reality,
and reconciles the coupled echoes of the same framing elsewhere
(`benchmarks.md` §2, `README.md`, and RFC 0001's summary).

## 2. Motivation

### 2.1 The pillar statement is now contradicted by an accepted RFC

`CLAUDE.md` §2 is the project's load-bearing thesis: changing a pillar "is
an RFC-level decision." Pillar #2's "this is where the 50–200×
compression comes from — before any byte-level codec runs" asserts that
the byte-savings come from template mining, ahead of the codec. RFC 0011
(`accepted` 2026-06-14) measured the opposite on every corpus class,
including the maximally-templated LogHub HDFS_v1 (ourios 8.3× vs zstd-19
16× → A1 delta 0.516×, `benchmarks.md` §9.5/§9.6). The headline number in
the most load-bearing document in the repo is therefore inaccurate as
written. `benchmarks.md` opens by calling itself "an honesty contract with
ourselves"; the same standard applies to the pillar it tests against.

### 2.2 The number is not wrong — its *referent* is

The 50–200× figure is real and worth keeping: it is the **logical**
collapse of *N* near-identical log lines to a handful of `(template_id,
params)` rows. That reduction is exactly what lets a selective query read a
few row groups instead of scanning the corpus (pillar #1's footer-skip),
which the thesis gates **B1** (predicate-pushdown latency, PASS — 34.2× /
25.4×, `benchmarks.md` §9.4) and **B2** (result-size-not-corpus-size
scaling, PASS) measure and confirm. RFC 0011 §2.3 spells this out. So the
fix is a *referent* correction — "logical reduction → query pruning," not
"on-disk bytes → before the codec" — not a retraction of the claim.

### 2.3 Why fix the wording at all

An inaccurate load-bearing claim quietly licenses bad decisions: someone
could "optimise" Ourios's on-disk size to chase the 50–200×-vs-codec
framing, trading away the columnar framing (page indexes, bloom filters,
row-group metadata) that *is* the value (it enables the row-group skipping
B1/B2 measure) — exactly the alternative RFC 0011 §4 rejected as
counter-productive. Pinning the pillar to the logical-reduction framing
forecloses that.

## 3. Proposed design

### 3.1 The CLAUDE.md §2 pillar-#2 change

Replace the current pillar #2 (`CLAUDE.md` §2, the "Drain-derived online
template mining" item):

> 2. **Drain-derived online template mining.** Log lines collapse to
>    `(template_id, params)` at ingest time. This is where the 50–200×
>    compression comes from — before any byte-level codec runs. Correctness
>    of this layer is the single biggest engineering risk in the project.

with:

> 2. **Drain-derived online template mining.** Log lines collapse to
>    `(template_id, params)` at ingest time — a **logical** 50–200×
>    reduction (many near-identical lines become rows keyed by one small,
>    stable `template_id`). That reduction is what lets a selective query
>    read a handful of row groups instead of scanning the corpus, so the
>    payoff is **query pruning** (pillar #1's footer-skip; benchmark gates
>    B1/B2), **not** fewer on-disk bytes than a byte codec — RFC 0011
>    showed a whole-stream codec captures the same redundancy, so the
>    on-disk-compression-vs-zstd ratio (A1) is a recorded diagnostic, not a
>    gate. Correctness of this layer is the single biggest engineering risk
>    in the project.

The final sentence (the "single biggest engineering risk" line) is
preserved verbatim — it is load-bearing in its own right and unaffected.

### 3.2 The coupled documentation reconciliations

The same on-disk/byte-level framing echoes in three other docs; all are
reconciled in the same enactment so the docs stay consistent (none is
load-bearing in the `CLAUDE.md` sense, so they ride normal doc PRs). The
authoritative list is whatever the RFC0012.2 framing-grep (§5) surfaces —
as of drafting, the phrase "before any byte-level codec" / "over a
competent byte codec" appears in exactly these (plus RFC 0011 and this RFC,
which quote it to describe the change):

1. **`benchmarks.md` §2** — the A1 "Why this bar" bullet paraphrases the
   pillar as *the project's headline claim (§2, CLAUDE.md) is "50–200× over
   raw, ≥ 5× over a competent byte codec."* That paraphrase (a) attaches a
   "≥ 5× over a competent byte codec" multiplier the pillar never literally
   stated and (b) is the byte-vs-codec framing RFC 0011 demoted. Reword to
   the logical-reduction / diagnostic framing.
2. **`README.md`** — the "Drain-derived online template miner" bullet says
   lines collapse to `(template_id, params)` *"before any byte-level codec
   runs."* Same fix: it is the logical reduction, before the codec in the
   *pipeline* but not a bytes-vs-codec claim.
3. **`docs/rfcs/0001-template-miner.md` §1** — its summary states *"The
   compression target is 50–200× over raw bytes before any byte-level codec
   runs."* Same framing. RFC 0001 is `accepted`, but this is a **factual**
   thesis-statement correction (not a change to its design or §5 acceptance
   criteria), so reconcile it to the logical-reduction framing with a
   one-line note pointing at RFC 0011. (If the maintainer prefers to leave
   an accepted RFC's prose untouched, the alternative is a dated editorial
   note rather than a reword — maintainer's call at enactment.)

Only the **framing** is reconciled; bare mentions of the *50–200×*
**figure** as a logical reduction (e.g. `docs/roadmap.md`, other RFCs) are
correct and are left alone.

### 3.3 What does **not** change

- No code, schema, or on-disk format. This is a documentation-wording RFC.
- The production codec default (ZSTD-3) and the A1 *diagnostic* itself
  (RFC 0011) are untouched.
- `CLAUDE.md` §1's thesis sentence ("collapses the inverted index, the
  compression layer, the storage tier, and the query engine into one
  stack") is left as-is — it describes the *stack* collapsing layers, not
  template mining as the byte-compressor; see §7 for the open question on
  whether it also wants a touch.

## 4. Alternatives considered

- **Leave the wording.** Rejected: an accepted RFC (0011) contradicts a
  load-bearing pillar; leaving it is the silent-inaccuracy failure mode the
  project's honesty contract exists to prevent.
- **Delete the 50–200× number.** Rejected: the logical reduction is real,
  is the thesis's actual mechanism, and is worth stating — only its
  referent (logical, not on-disk-bytes) needs fixing.
- **Reword more aggressively** (drop the figure, restate the whole pillar
  around query pruning). Rejected as over-reach for a wording fix: the
  minimal precise change keeps the pillar recognisable and the diff
  reviewable.
- **Fold this into RFC 0011.** Rejected: RFC 0011 is `accepted` and
  explicitly deferred the `CLAUDE.md` edit to a `meta:` RFC (its §3 item 4
  / §7), because `CLAUDE.md` changes need the footer's majority-approval
  gate that a thesis-gate tuning RFC does not.

## 5. Acceptance criteria

> **Scenario RFC0012.1 — pillar #2 states the logical-reduction framing.**
> - **Given** `CLAUDE.md` §2 pillar #2
> - **When** this RFC is enacted (post-approval)
> - **Then** pillar #2 reads per §3.1: the 50–200× is described as a
>   **logical** reduction whose payoff is query pruning (B1/B2), and the
>   on-disk-vs-zstd ratio is named a diagnostic (A1, RFC 0011), not a gate
> - **And** the "single biggest engineering risk" sentence is preserved
>   verbatim

> **Scenario RFC0012.2 — no on-disk-bytes framing of the 50–200× remains.**
> - **Given** the repo docs (`CLAUDE.md`, `README.md`, `docs/benchmarks.md`,
>   `docs/rfcs/0001-template-miner.md`)
> - **When** this RFC is enacted
> - **Then** no passage frames template mining's 50–200× as **on-disk
>   bytes** beaten "before any byte-level codec runs" or as "≥ N× over a
>   byte codec" — all coupled echoes (§3.2: `benchmarks.md` §2, `README.md`,
>   RFC 0001 §1) are reconciled
> - **And** a repo-wide grep for the **framing phrases** —
>   `before any byte-level codec` and `over a competent byte codec` —
>   returns only RFC 0011 / this RFC (which quote the old wording to
>   describe the change). The check is on the **framing**, not on the
>   *50–200× figure* itself: mentions of that figure as a logical reduction
>   (e.g. `docs/roadmap.md`, `docs/rfcs/0005-parquet-storage.md`) are
>   correct and expected to remain.

> **Scenario RFC0012.3 — consistency with the accepted A1 re-scope.**
> - **Given** RFC 0011 (`accepted`), `benchmarks.md` §7's gate table
>   (A1 = diagnostic), and the amended pillar #2
> - **When** a reader cross-checks the thesis statement against the
>   benchmark gates
> - **Then** the three agree: template mining's value is logical /
>   query-pruning (B1/B2 gate it), A1 is a diagnostic, and C1/C2 are the
>   miner pillar's gates (RFC 0001 `accepted`, RFC 0011)

## 6. Testing strategy

There is no code test: this RFC changes prose in two living documents. The
acceptance criteria (§5) are **doc-state assertions**, verified by review +
the grep in Scenario RFC0012.2, exactly as RFC 0011's RFC0011.1–.3 were.
Two notes:

- Unlike new OTel names (semconv `weaver registry generate` no-diff CI) or
  RFC acceptance scenarios (greppable test ids), `CLAUDE.md` carries **no
  automated consistency gate** — the gate is the footer's *majority
  maintainer approval* on the enacting PR. That human gate is this RFC's
  "test."
- The enacting PR's diff is the artefact: reviewers confirm the §3.1 text
  landed verbatim and §3.2's `benchmarks.md` reconciliation rode along.

## 7. Open questions

- [ ] **Maintainer approval (majority).** `CLAUDE.md`'s footer requires it
      for any change; this RFC cannot be enacted without it.
- [ ] **Does `CLAUDE.md` §1's thesis sentence want a parallel touch?** It
      reads "collapses … the compression layer … into one stack." That is
      defensible as written (the *stack* unifies layers; Parquet/zstd is
      the byte-compression layer, template mining the logical one), so this
      RFC leaves it alone — but if the maintainer reads "the compression
      layer" as implying template mining *is* the compressor, a one-clause
      clarification there is in scope for the same enactment.
- [ ] **Footer changelog line.** `CLAUDE.md`'s footer records each meta
      change with its commit range and rationale; the enacting PR should
      add the 2026-06-14 line (and bump "Last updated") in the same diff.

## 8. References

- **RFC 0011** — A1 re-scope (`accepted`): the measurement and the
  diagnostic-not-gating decision this RFC propagates to the pillar wording.
  Its §3 item 4 / §7 explicitly deferred this `CLAUDE.md` edit to a
  `meta:` RFC.
- **`docs/benchmarks.md`** §2 (A1 + "Why this bar"), §7 (gate table:
  A1 diagnostic), §9.4/§9.5/§9.6 (the A1/B1/B2/C1/C2 readings).
- **`CLAUDE.md`** §2 (the pillars), §8.5 (cache discipline — why the edit
  is not made in-session), and the footer (the `meta:` RFC + majority-
  approval rule; precedent `b50067d`).
- **RFC 0001** (`accepted`) and **RFC 0007** (`validated`) — the miner and
  querier pillars whose gates (C1/C2 and B1/B2) carry the value the amended
  wording points at.
