---
rfc: 0034
title: D1 re-scope — ingest throughput is a per-node capacity on baseline hardware, not a per-core rate
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-20
supersedes: —
superseded-by: —
---

# RFC 0034 — D1 re-scope

> **Status note.** **`specified`**, with all three §5 criteria now in
> force (2026-07-21); the terminal flip to `accepted` is the
> maintainer's sign-off, not this note. RFC0034.1 is **enacted** by
> this change: the `docs/benchmarks.md` § D1 block is recast per-node
> (≥ 100 000 lines/s on `baseline-8vcpu-32gib`, multi-tenant, shared
> commit stream, p99 ack ≤ 200 ms at the sustained rate) with the old
> per-core target and the per-tenant single-stream ceiling retained as
> recorded diagnostics, the RFC 0011 A1 pattern; the §7 gate table
> lists only the five `[THESIS]` goals, so the annotation lives at the
> § D1 block. RFC0034.2 is **satisfied** by §9.23 — RFC 0035 reached
> `green` and the one-hour `--tenants 8` asserting soak achieved
> 99.92% of the offered 100k lines/s (0 failed batches, p99 ack
> 153.63 ms, D2 PASS). RFC0034.3 was **already satisfied** by the soak
> harness's report format (aggregate + per-tenant + per-core fields,
> #567/#570). The §7 one-run-two-records question resolved as one run,
> one record (§9.23), cited by both this RFC and RFC 0035.
>
> **How to read this document.** A tuning RFC in the RFC 0011 mold: the
> D1 gate, as written, measures a dimension the architecture deliberately
> does not scale on, so the gate is reconciled with its own stated intent
> and with measurement — *after* the measurement was made honest. The
> sequencing matters and is part of the record: an earlier draft of this
> RFC (never PR'd) proposed recasting D1 as *per-node multi-tenant*
> capacity on the premise that node throughput scales with tenant
> parallelism; the in-process measurement (`benchmarks.md` §9.21)
> **refuted that premise** (the ceiling was flat across tenant counts —
> a global serialization), which spawned RFC 0035 (ingest concurrency)
> first. This RFC now recalibrates D1 against RFC 0035 Design A's
> measured capacity (§9.22). It amends only the `benchmarks.md` § D1
> block and §7 gate table; no code, no on-disk byte. Per
> `docs/rfcs/README.md`, **`specified`** means the §5 acceptance
> criteria are written and review has confirmed them testable in
> principle — the #575 review round did exactly that (tightening
> RFC0034.2 to an observable below-saturation condition and a
> deterministic N); RFC0034.2's assertion additionally waits on
> RFC 0035 reaching `green`.

## 1. Summary

D1 — *"OTLP → WAL throughput ≥ 100 000 lines/s **/core**, p99
ingest-ack ≤ 200 ms"* — has a metric (`lines/second/core`) that
contradicts its own falsifier (*"a meaningful share of production
traffic **per node**"*). Measurement showed per-core is the wrong axis
twice over: single-tenant ingest is bounded by the per-tenant sequential
miner (`CLAUDE.md` §3.7's deliberate isolation), and node ingest as a
whole was bounded by a global commit-gate serialization — a software
artifact, since removed by RFC 0035 Design A (132,289 lines/s measured
on the baseline class, §9.22, vs ~86k before). D1 is recast: the
**must-win becomes per-node sustained throughput ≥ 100 000 lines/s on
`baseline-8vcpu-32gib`** (multi-tenant load, one shared WAL/commit
stream — Design A clears it with ~32% margin), the **per-core and
per-tenant rates become recorded diagnostics**, and the **p99 ≤ 200 ms
ack bar is unchanged**, clarified to apply at the sustained rate (below
saturation). D2 is untouched — it passed at every measured load.

## 2. Motivation

### 2.1 The metric contradicts the falsifier

D1's target is per-core; its falsifier is per-node. Per-core reads as a
portability normalization bolted onto a node-level operational claim.
The tension stayed latent until D1 was first measured (§9.19 — unrun
before 2026-07-19).

### 2.2 Per-core is the wrong axis, for two measured reasons

**(a) Single-tenant load cannot scale on cores by design.** The
per-tenant template miner is sequential — `CLAUDE.md` §3.7's per-tenant
trees, the least-common-mechanism isolation choice. §9.20's capacity
ladder found a flat single-tenant service-rate ceiling regardless of
offered rate; more cores cannot help one tenant's in-order stream.

**(b) The node-level ceiling was a software artifact, now removed.**
§9.21: node capacity was flat (~86k lines/s) across 1/8/16 tenants —
a global commit-gate + miner-lock serialization with the machine ~85%
idle (1.2 of 8 cores). RFC 0035 (`specified`) took the order-insensitive
Parquet emit off the gate; its Design A prototype measured **132,289
lines/s** on the baseline class (§9.22, D2 PASS, ack latencies down
~40% at saturation). The residual serial fraction (~0.62 — ordered
mining + WAL group commit) is what a per-core bar would demand scale
that the WAL's single durable append stream (§3.4) intentionally does
not offer.

### 2.3 What the falsifier actually asks

"A meaningful share of production traffic per node." At 100k lines/s a
node ingests ≈ 8.6 B lines/day; horizontal scaling multiplies nodes
(§9.20's independent-lane result showed near-linear multi-node
extrapolation). The per-node bar measures the operational claim
directly, on the axis the architecture scales on after RFC 0035.

## 3. Proposed design

1. **D1's must-win**: sustained **≥ 100 000 lines/s per node** on
   `baseline-8vcpu-32gib`, multi-tenant load (`soak --tenants N` with
   **N = cores**, i.e. 8 on the baseline class) through one shared
   WAL/commit stream, WAL fsync batched at 100 ms, **with p99
   ingest-ack ≤ 200 ms at that sustained rate**. "Below saturation" is
   observable, not asserted: the run offers exactly the bar rate
   (100 000 lines/s) and must **achieve ≥ 99% of offered** — a
   saturated pipeline cannot keep pace with the paced load, so
   achieved ≈ offered *is* the below-saturation proof, and the p99 is
   measured over that same run (queue-bound latencies at over-offered
   load are a different regime, per §9.20's reading, and do not
   count). Judged on the §9 series;
   first evidence: §9.22's 132,289 lines/s (RFC 0035 Design A
   prototype). The bar **asserts only once RFC 0035's production
   implementation is `green`** — the prototype number is the calibration
   input, not the verdict.
2. **Diagnostics (informational, still recorded)**: per-core rate
   (the old bar's axis) and the per-tenant single-stream ceiling (the
   most one service can push into one tenant — a real operational
   number that guards the mining path against regression). Neither
   gates any RFC's `validated` (the RFC 0011 A1 pattern).
3. **D2 unchanged** — already node-level, passed at every measured load
   including saturation.
4. **Falsifier retained verbatim** — it was right all along; the metric
   moves to match it.

## 4. Alternatives considered

- **Keep the per-core bar.** Rejected: it demands scaling on an axis the
  architecture deliberately serializes twice (per-tenant miner order,
  §3.7; single durable WAL stream, §3.4). A permanently-red gate whose
  redness is unrelated to quality trains readers to ignore gates.
- **Recast as per-node *multi-tenant scaling* (the refuted first
  draft).** Rejected by measurement: §9.21 showed node capacity flat
  across tenant counts pre-RFC-0035. Tenancy is not the scaling axis
  in-process; concurrency within the node (RFC 0035) is.
- **Set the bar at the measured 132k.** Rejected: gates are floors the
  architecture clears with margin (the RFC 0031 must-win convention),
  not peaks that fail on noise. 100k keeps ~32% margin, matches the
  falsifier's round operational claim, and gives the old per-core
  number a per-node home.
- **Wait for RFC 0035 `green` before writing this RFC.** Rejected:
  the recalibration design is measurement-independent (the 132k only
  sets the margin); reviewing it in parallel shortens the path, and §3.1
  explicitly defers assertion until RFC 0035 is `green`.

## 5. Acceptance criteria

> **Scenario RFC0034.1 — the gate table is recast.**
> - **Given** the `benchmarks.md` § D1 block and §7 gate table
> - **When** this RFC is enacted
> - **Then** D1's must-win reads *per-node ≥ 100 000 lines/s on
>   `baseline-8vcpu-32gib` (multi-tenant, shared commit stream), p99
>   ack ≤ 200 ms at the sustained rate*, with a pointer to this RFC
> - **And** per-core and per-tenant rates are labelled *diagnostic
>   (informational)* and appear in no `validated` blocking set.

> **Scenario RFC0034.2 — the bar asserts on baseline hardware once
> RFC 0035 is green.**
> - **Given** RFC 0035's production implementation at `green` and a
>   `soak --tenants 8` run on `baseline-8vcpu-32gib` (N = cores,
>   deterministic)
> - **When** the one-hour soak offers exactly 100 000 lines/s
> - **Then** achieved ≥ 99% of offered (the observable
>   below-saturation condition) with 0 failed batches, p99 ack
>   ≤ 200 ms over that run, D2 PASS — recorded in the §9 series as
>   the asserting run.

> **Scenario RFC0034.3 — the diagnostics stay visible.**
> - **Given** any soak run
> - **When** the harness finalises
> - **Then** per-core and per-tenant rates are computed and recorded,
>   flagged diagnostic.

## 6. Testing strategy

Mapped to `CLAUDE.md` §6.2. This RFC changes documentation and gate
semantics, not code, so its tests are the measurement harness's:
RFC0034.1 is enacted by editing `benchmarks.md` (reviewed, greppable
pointer to this RFC); RFC0034.2 rides the existing `soak --tenants N`
harness (#567) on the baseline class — the same instrument §9.21/§9.22
used — recorded by hand in §9 per the series discipline; RFC0034.3 is
already satisfied by the soak report format (aggregate + per-tenant +
per-core fields, #567/#570) and pinned by its existing report tests.

## 7. Open questions

- [x] Whether the asserting run (RFC0034.2) doubles as RFC 0035's
  RFC0035.4 measurement (same instrument, same hardware) — resolved
  as **one run, one record** (`benchmarks.md` §9.23), cited by both.
- [x] N for the asserting run: settled at **N = cores** (8 on the
  baseline class, the §9.22 shape) for determinism; a tenant-scaling
  curve remains a worthwhile *diagnostic* exploration but never moves
  the gate's N.
- [x] `benchmarks.md` § D1's prose retains the old per-core target as
  the diagnostic's reference line (RFC 0011 did this for A1) —
  confirmed at enactment: the § D1 diagnostics bullet keeps the
  ≥ 100 000 lines/s/core line, labelled informational.

## 8. References

- `docs/benchmarks.md` § D1/§ D2 (amended), §7 (scope/bar vocabulary),
  §9.19 (first D1 run — paced, latency bar pass), §9.20 (single-tenant
  ceiling ladder + tenant-parallel approximation), §9.21 (in-process
  flat ceiling + serialization profile), §9.22 (RFC 0035 Design A A/B:
  82.1k → 132.3k, the calibration input), §1 (`baseline-8vcpu-32gib`).
- **RFC 0035** (`specified`) — ingest concurrency; this RFC's must-win
  asserts only at its `green`.
- **RFC 0011** (`accepted`) — the must-win/diagnostic recalibration
  precedent.
- **Issue #571** — the serialization profile.
- `CLAUDE.md` §3.4 (WAL-before-ack — why the commit stream is one),
  §3.7 (per-tenant trees — why single-tenant load doesn't scale on
  cores).
