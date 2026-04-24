---
rfc: 0001
title: Template miner (Drain-derived online log parsing)
status: draft
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-04-24
supersedes: —
superseded-by: —
---

# RFC 0001 — Template miner

> **How to use this document:** it is a scaffold. Each section lists the
> prompts you should answer *after* reading the Drain paper (He et al.,
> ICWS 2017) and the Drain3 source. Do not fill in a section before you
> have read the material it references — the point of the RFC is to
> prove you understood the paper well enough to commit Ourios to a
> specific extension of it.
>
> Cross-references to `CLAUDE.md` sections are in square brackets,
> e.g. `[§3.1]`, and name the invariant the section must preserve.

## 1. Summary

*One paragraph.* We implement a Drain-derived online template miner for
Ourios. It extends the paper with the invariants required by `[§3.1]`,
`[§3.2]`, `[§3.3]`, and `[§3.5]`. State your commitment here in 3–5
sentences and nothing more.

## 2. Motivation

*Why this is a pillar `[§2.2]` and not a dependency choice.*
- Why template mining at all (50–200× compression claim — justify the
  number or revise it).
- Why online vs. offline (latency budget; no batch window that would
  allow offline clustering).
- Why this is the right layer for the compression (before Parquet's
  byte-level codecs, not after).

## 3. Background: Drain as published

*Restate the paper in your own words and notation.* If you cannot
restate it, you have not yet understood it well enough to modify it.

- 3.1 Tree structure (root → length group → token-prefix nodes at
  depth `d` → leaf log group). Include a redrawn Fig. 2 in ASCII or
  Mermaid.
- 3.2 Similarity function: how two lines are compared inside a leaf
  log group.
- 3.3 Threshold `st`: what value means what.
- 3.4 New-log-group creation: the condition under which a new group is
  opened rather than an existing one updated.
- 3.5 Worked example: take one line from `testdata/corpus/` (once it
  exists) and walk it through the tree. If the corpus does not exist
  yet, use a fabricated example and mark it as such.

*Citations should name paper section/figure, not page number.*

## 4. Background: Drain3 extensions (not in the paper)

*What IBM's maintained fork adds, from reading the Drain3 source
(`drain3/template_miner.py`) and README.*

- 4.1 Persistent state (Redis / file / Kafka backends).
- 4.2 Masking rules (pre-parse regex masks: IPs, UUIDs, numbers).
- 4.3 Variable-length wildcards.
- 4.4 Dynamic / adaptive threshold.
- 4.5 Anything else the README calls out that is not in the 2017 paper.

For each: *adopt / modify / reject*, with one sentence of rationale.
This is the shortest section with the highest downstream impact —
every decision here echoes through the miner's test matrix.

## 5. What the paper and Drain3 do not address

*The gap list. Each gap is the reason an Ourios invariant exists.*

| Gap in published Drain | Ourios invariant that fills it |
|---|---|
| No confidence score on a match | `[§3.1]` body retention below threshold |
| No audit trail on group merges | `[§3.1]` merge audit events |
| No inter-token whitespace preservation | `[§3.3]` bit-identical reconstruction |
| No per-parameter byte bound | `[§3.2]` param length limit, overflow to `body` |
| No multi-tenant scoping of the tree | `[§3.7]` per-tenant template trees |
| No template versioning / drift story | `[§3.5]`, hazard 5 |

*If you find additional gaps while reading, add rows.*

## 6. Proposed design

*The Ourios miner in detail. This is the section that the `ourios-miner`
crate is implemented against. Keep it precise; do not hand-wave.*

### 6.1 Data model

- Record shape: `(tenant_id, template_id, template_version, params,
  body?, confidence, lossy_flag)`.
- Template identity: hash of the canonical template string, or a
  monotonic integer per tenant — justify the choice.
- Relationship between `template_id` and `template_version` `[§3.5]`.

### 6.2 Algorithm

*Describe the tree walk precisely enough that two engineers would
produce the same Rust code.* Include:
- Preprocessing (masking rules applied, whitespace capture).
- Tree-walk pseudocode.
- Branching rules at each node type.
- When a leaf is split vs. extended vs. merged.
- Reference §3 of this RFC rather than restating it.

### 6.3 Confidence scoring `[§3.1]`

- Definition of the confidence metric (similarity / threshold ratio,
  or a dedicated score — pick one and defend it).
- The three zones: `≥ threshold` (accept), `floor ≤ x < threshold`
  (accept but retain body), `< floor` (parse failure, retain body,
  count `parse_failures_total`).
- Default values for threshold and floor, and the bound in `[§3.1]`
  (threshold ≥ 0.7) that must hold regardless of config.

### 6.4 Merge policy `[§3.1]`

- When two templates become candidates for merge.
- The audit event schema (old template, new template, tenant, time,
  reason) and the metric increment.
- Default: strict. Never silent. No exceptions.

### 6.5 Parameter handling `[§3.2]`

- Per-parameter byte limit (default 256, ceiling 1 KiB).
- Overflow behaviour: the original value spills to the `body` column,
  the `params` slot gets a truncation marker.
- Overflow rate metric and the 1% per-service alerting threshold.

### 6.6 Body reconstruction `[§3.3]`

- Whitespace and separator capture strategy (what state the miner
  stores alongside the template).
- Reconstruction function signature and guarantees.
- The `lossy_flag` — when it is set, and how the reader surfaces it.
- Property test pseudocode: `∀ line ∈ corpus. reconstruct(mine(line)) =
  line ∨ mine(line).lossy_flag`.

### 6.7 Template versioning and drift `[§3.5]`, hazard 5

- How a template is versioned when its internal structure changes.
- The alias mechanism (queries against `template_id=X` surface `X` and
  its aliases, or surface drift explicitly — choose and justify).
- What "drift detection as a first-class query" means in concrete
  terms. Name the query.

### 6.8 Telemetry `[§3.1]`, §6.3

*The metrics enumerated in `[§3.1]` are mandatory. List them with
their types (counter / gauge / histogram) and labels (at least
`tenant_id`, possibly `service`).*

- `template_count` (gauge, per tenant)
- `merges_total` (counter)
- `confidence_p50`, `confidence_p01` (histogram summaries)
- `body_retention_ratio` (gauge)
- `parse_failures_total` (counter)
- Plus any additional metrics the design in 6.1–6.7 implies.

### 6.9 Persistence and recovery

- Per-tenant template tree serialisation format.
- Where it lives (object storage, local cache, WAL) and the consistency
  rules tying it to the data the trees were built from.
- Recovery procedure on ingester restart.
- Migration story when 6.1 or the serialisation format changes.

## 7. Alternatives considered

*For each, one paragraph on why it was not chosen. "I have not heard
of it" is not an acceptable answer — either evaluate it or mark the
alternative as deferred and open a follow-up RFC.*

- Spell (longest-common-subsequence online parser).
- IPLoM (iterative partitioning).
- LenMa (length-based clustering).
- LogPPT / LLM-based parsers.
- Offline clustering (e.g. nightly batch with hierarchical clustering)
  with an online fallback.

## 8. Testing strategy

*Mapping to `[§6.2]`.*

- Unit tests for tree operations (insert, match, split, merge).
- `proptest` for reconstruction `[§3.3]`.
- Corpus tests: on a fixed anonymised corpus, assert bounds on
  `template_count`, `merges_total`, and reconstruction accuracy;
  regressions are build failures, not warnings.
- Confidence calibration test: on labelled lines, verify the three-zone
  classification in 6.3.
- Merge-audit assertion: no merge ever happens without an audit event
  (negative test).
- Multi-tenant isolation: a template mined under tenant A never
  appears in tenant B's tree (negative test).
- Benchmark (`criterion`): ingest throughput, per-line miner latency.

## 9. Open questions

*List the things you know you do not know yet. Each must be resolved
before the RFC is accepted. Add as you read.*

- [ ] Threshold default — the paper reports a sweet spot; does it hold
      on our target corpora?
- [ ] Does masking happen before or after the tree walk (Drain3
      choice vs. paper)?
- [ ] How do we handle log lines that do not parse into tokens at all
      (binary blobs, malformed UTF-8)?
- [ ] Template identity: hash vs. monotonic integer — implications for
      cross-tenant collisions and for schema evolution.

## 10. References

- He, P., Zhu, J., Zheng, Z., Lyu, M.R. "Drain: An Online Log Parsing
  Approach with Fixed Depth Tree." ICWS 2017.
  <!-- Add DOI / PDF link once confirmed. -->
- Drain3: https://github.com/logpai/Drain3 (commit pinned in this RFC
  once design freezes).
- LogPAI benchmark: https://github.com/logpai/logparser
- `CLAUDE.md` §§ 2, 3.1–3.3, 3.5, 3.7, 4, 6.2, 6.3.
- Future: `docs/architecture/miner.md` (this RFC graduates there on
  acceptance).
