---
rfc: 0011
title: A1 re-scope — template-mining compression is logical (query-pruning), not byte-level
status: accepted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-06-13
supersedes: —
superseded-by: —
---

# RFC 0011 — A1 re-scope

> **Status note.** **`accepted`** (2026-06-14, maintainer sign-off). A
> tuning RFC, so it advances directly to the terminal status once its §5
> criteria are enacted: RFC0011.1 (A1 is diagnostic, not gating),
> RFC0011.2 (the miner's thesis gates are C1 + C2), and RFC0011.3 (the A1
> diagnostic is still recorded) are all in force — the `docs/benchmarks.md`
> §7 gate table marks A1 diagnostic, RFC 0001's `validated` is judged on
> C1/C2, and §9.5/§9.6 record the A1 readings. Accepting ratifies the
> re-scope that RFC 0001's `validated`/`accepted` (also 2026-06-14) rests
> on.
>
> **How to read this document.** This is a **tuning RFC** spawned by the
> `docs/benchmarks.md` §7 escalation path: a thesis gate (A1) failed and
> the failure analysis is in, so the gate is reconciled with the evidence
> rather than left to block indefinitely. §§1–4 are the design contract;
> §5 is the acceptance criteria (what this RFC must enact); §6 records the
> measurements. It amends `docs/benchmarks.md` (the A1 gate's role) and
> the thesis-gate set RFC 0001's `validated` stage is judged against.

## 1. Summary

The A1 thesis gate — *"Ourios on-disk bytes ≥ 3× smaller than zstd-19
over the raw corpus"* — is **refuted by measurement on every corpus
class tested, including the maximally-templated one**, and fails *worse*
the more templated the corpus is. A1 is therefore **demoted from a
gating thesis criterion to a recorded diagnostic**. Template mining's
compression value is realised as **query pruning** (B1/B2 — row-group
skipping, RFC 0007, already `validated`), **reconstruction fidelity**
(C1), and **template-count convergence** (C2) — not as on-disk bytes
versus a byte codec. RFC 0001's (template-miner pillar) `validated` stage
is accordingly judged against **C1 + C2**, both of which pass on a
representative ≥ 1 M-line corpus (§6).

## 2. Motivation

### 2.1 The measurement

A1 had only ever been measured on the OTel-Demo corpus class
(`benchmarks.md` §9.1/§9.4), where it failed (best `0.829×` vs the `3.0×`
target). The standing analysis attributed this to two structural causes
— the demo logs are *locally repetitive* (so zstd-19 over the
concatenated stream captures the redundancy at any size) and columnar
Parquet carries a framing premium (per-column/page-index/bloom/row-group
overhead) that is *the price of queryability*. But OTel-Demo is not the
corpus where template mining should look best. The decisive test is a
**maximally-templated** corpus: a handful of templates over millions of
lines. LogHub HDFS_v1 (11.2 M lines, 1.58 GB) is exactly that.

A1 on HDFS_v1 (§6): **ourios 8.300× vs zstd-19 16.000× → delta 0.516× →
FAIL** — *worse* than OTel-Demo, not better.

### 2.2 Why the best case for template mining is the best case for zstd

The result is not a defect; it is structural and was predictable in
hindsight. The more templated (repetitive) a corpus, the more completely
a whole-stream byte codec captures its redundancy: zstd-19 over the
concatenated HDFS log hits 16×. Template mining collapses the repetitive
template *text*, but the variable bits it extracts — HDFS block IDs,
timestamps, IPs — are **high-cardinality columns** that do not compress
to the same degree, and the columnar layout adds framing the single zstd
window does not pay. Net: ourios's 8.3× cannot beat the 16× a byte codec
already extracts from the same redundancy. *The corpus that most rewards
template mining most rewards the byte codec it is measured against*, so
the `≥ 3× over zstd` framing cannot hold on any realistic log corpus.

### 2.3 What template mining actually buys

The thesis (`CLAUDE.md` §2 pillar #2) is sound; A1 measured the wrong
quantity. Template mining's "50–200×" is a **logical** reduction — each
line becomes `(template_id, params)`, so a selective query reads a
handful of row groups instead of scanning the corpus. That value is
captured by **B1** (predicate-pushdown latency, ≥ 10×) and **B2**
(template-exact queries scale with result size, not corpus size) — both
**pass authoritatively** (RFC 0007, `validated`; `benchmarks.md` §9.4,
incl. HDFS_v1 at 11.2 M rows). The miner's own correctness is **C1**
(bit-identical reconstruction or flagged-lossy) and **C2** (sub-linear
template growth) — both pass on HDFS_v1 (§6). On-disk bytes versus a byte
codec is a *diagnostic* (it tells operators the queryability premium),
not a thesis claim.

## 3. Proposed design

1. **A1 is reclassified `diagnostic`, not `gating`.** The measurement
   (ourios ratio, zstd-19 ratio, delta) is still computed and recorded in
   the `benchmarks.md` §9 series — it characterises the columnar
   queryability premium and guards against regression in the codec
   path — but a `delta < 3.0×` no longer blocks any RFC's `validated`
   stage. `benchmarks.md` §7's gate table marks A1 *diagnostic*; the §3.4
   target text is retained as the diagnostic's reference line, annotated
   that it is informational.
2. **The template-miner pillar's gating thesis criteria are C1 + C2.**
   RFC 0001 (`green`) reaches `validated` when C1 and C2 pass on a
   representative (≥ 1 M-line, `benchmarks.md` §8) corpus — which they do
   on HDFS_v1 (§6). The query-pillar gates B1/B2 remain RFC 0007's, and
   are already `validated`.
3. **No change to the codec or the writer.** The production ZSTD-3
   default stands (the codec gain is small and saturates by level 9, and
   the residual gap is structural — `benchmarks.md` §9.1). This RFC
   changes only what A1 *means* for the maturity ladder, not any byte on
   disk.
4. **`CLAUDE.md` §2 wording is flagged, not changed here.** Pillar #2's
   "50–200× compression … before any byte-level codec runs" reads as an
   on-disk-bytes claim; it is precise only as a *logical* reduction. A
   one-line clarification is recommended but `CLAUDE.md` is load-bearing
   and changes require a `meta:` RFC + maintainer approval (its own
   footer), so it is an explicit follow-up (§7), not enacted here.

## 4. Alternatives considered

- **Keep A1 as a hard ≥ 3× gate.** Rejected: it fails on every corpus
  class including the maximally-favourable one, so it would block RFC
  0001's `validated` permanently on a criterion the data shows is
  mis-framed — penalising the project for a measurement that never
  reflected the thesis.
- **Optimise ourios's on-disk size to beat zstd-19.** Rejected as
  futile and counter-productive: the ~17 %–2× gap is the columnar
  framing (page indexes, per-column chunks, bloom filters, row-group
  metadata) that *enables* row-group skipping — i.e. it is the price of
  B1/B2. Shrinking it would trade away the thesis's actual value to win a
  metric that doesn't matter.
- **Drop A1 entirely.** Rejected: the ourios-vs-zstd ratio is a useful
  operator-facing diagnostic (bytes-per-line, the queryability premium)
  and a regression guard on the codec path. Demote, don't delete.
- **Redefine A1 to measure the logical reduction (lines → template
  rows).** Considered; deferred. The logical reduction is already what
  B2 operationalises (result-size-not-corpus-size scaling) and what C2
  tracks (template plateau); a third metric restating it adds little. If
  a standalone "logical compression ratio" proves useful to operators it
  can be added later as another diagnostic.

## 5. Acceptance criteria

> **Scenario RFC0011.1 — A1 is diagnostic, not gating.**
> - **Given** the `benchmarks.md` §7 thesis-gate table and the §3.4 A1
>   definition
> - **When** this RFC is enacted
> - **Then** A1 is labelled *diagnostic (not gating)* in the §7 table
>   with a pointer to this RFC, and the §3.4 target is annotated
>   informational
> - **And** a `delta < 3.0×` no longer appears in any RFC's `validated`
>   blocking set

> **Scenario RFC0011.2 — the miner pillar's thesis gates are C1 + C2,
> and they pass on a representative corpus.**
> - **Given** RFC 0001 (`green`) and a representative ≥ 1 M-line corpus
>   (`benchmarks.md` §8)
> - **When** C1 (reconstruction) and C2 (convergence) are measured on it
> - **Then** both pass — C1 = 1.000000 bit-identical on non-lossy rows,
>   C2 sub-linear with the formal gate *applying* (not abstaining) at
>   ≥ 1 M lines — recorded in the §9 series
> - **And** RFC 0001's `validated` stage is judged against C1 + C2 (with
>   B1/B2 the query pillar's, RFC 0007); A1 does not gate it

> **Scenario RFC0011.3 — the diagnostic is still recorded.**
> - **Given** a bench run with the A1 gate selected
> - **When** the harness finalises
> - **Then** the ourios ratio, zstd-19 ratio, and delta are still
>   computed and written to the §9 results, flagged diagnostic — so the
>   queryability premium stays visible and codec regressions surface

## 6. Measurements (2026-06-13, local — `hardware_kind = "unknown"`)

Run via `ourios-bench --gates … --parquet-zstd-level 19 --allow-unknown-hardware`
on LogHub HDFS_v1 (Zenodo record 8196385,
md5 `76a24b4d…`; 11,175,629 lines, 1,577,982,906 raw bytes; fetched at
bench time, never redistributed — `query-bench.yml`). Local hardware, so
these are **diagnostic**, not the authoritative `baseline-8vcpu-32gib`
numbers; A1's verdict is corpus-structural and hardware-independent
(compressed bytes are deterministic), and C1/C2 are ratios, so the
finding stands regardless of the runner. The authoritative
representative-corpus rerun for the actual RFC 0001 `validated` flip is a
maintainer-gated GH Actions / baseline step.

| gate | result | verdict |
|---|---|---|
| A1 | ourios 8.300× vs zstd-19 16.000× → **delta 0.516×** (raw 1.578 GB → ourios 189.98 MB, zstd-19 98.21 MB) | FAIL (now **diagnostic**) |
| C1 | **1.000000** reconstruction — 11,175,578 / 11,175,578 non-lossy rows bit-identical; lossy ratio 4.6e-06 (51 rows) | PASS |
| C2 | end template count **40** at 11.2 M lines (33 at 1 M); ratio 0.825 — sub-linear, formal gate **applies** (≥ 1 M) | PASS |

For comparison, A1 on the OTel-Demo class (`benchmarks.md` §9.1/§9.4) was
`0.829×` best — so the maximally-templated corpus fails A1 *harder*,
confirming §2.2.

## 7. Open questions

- **`CLAUDE.md` §2 pillar #2 wording.** "50–200× compression … before any
  byte-level codec runs" should be clarified to "a 50–200× *logical*
  reduction (lines → `(template_id, params)`), realised as query pruning
  — not an on-disk-bytes win over a byte codec." Requires a `meta:` RFC
  per `CLAUDE.md`'s footer; recommended follow-up.
- **Authoritative representative rerun.** C1/C2 here are on local
  hardware. The `validated` flip for RFC 0001 should cite a
  `baseline-8vcpu-32gib` (or equivalent) representative run; the verdicts
  are not expected to change (deterministic ratios), but the record
  should be authoritative.

## 8. References

- `docs/benchmarks.md` §3.4 (A1 definition), §7 (gate table + escalation),
  §9.1/§9.4 (prior A1), §8 (representative-corpus minimum).
- RFC 0001 §5 (C1/C2 among the miner's acceptance criteria), `CLAUDE.md`
  §2 pillar #2, §3.3 (reconstruction).
- RFC 0007 (`validated`) — B1/B2, the query pillar.
