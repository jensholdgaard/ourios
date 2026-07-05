---
rfc: 0024
title: OTLP-envelope property testing and corpus-calibrated generation (RFC 0006 amendment)
status: green
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-05
supersedes: —
superseded-by: —
---

# RFC 0024 — OTLP-envelope property testing and corpus-calibrated generation (RFC 0006 amendment)

## 1. Summary

The verification surface has two layers today, and a gap between
them. Frozen corpora (RFC 0006: the seed corpus, the OTel-Demo
captures, the `LogHub` stress family) exercise *reality* — but only
the exact records that happened to be captured. Property suites
(RFC 0003's wire-decode equivalence over the proto value space, the
miner's hazard and RFC 0023 fanout properties) exercise *arbitrary*
inputs — but each at one unit's boundary, never through the full
ingest → store → query pipeline.

The gap: nothing generates **realistic-but-arbitrary OTLP** and
asserts end-to-end invariants over it — and the strongest such
invariant, *query-result correctness against an independent oracle*,
does not exist anywhere in the suite today.

This RFC amends RFC 0006 with:

1. **Calibration manifests** — small, committed distribution summaries
   extracted from a real capture (attribute-count and body-length
   histograms, severity mix, `AnyValue` shape frequencies), so
   generators are shaped by measured reality rather than guesses.
2. **OTLP-envelope generators** — `proptest` strategies over
   `OtlpLogRecord`, with a *calibrated* mode (the realistic centre)
   and an *adversarial* mode (the envelope's legal extremes).
3. **Four end-to-end properties** — bit-faithful round-trip, no
   silent merge, RFC 0023 bounds, and the **query oracle**: for
   generated data and generated predicates, the querier's answer
   must equal an independent linear-scan evaluator's.

Scope is OTLP only, per the standing product decision: legacy log
formats are Collector concerns; generators target the OTLP envelope
space exclusively.

## 2. Motivation

- **There is no at-scale OTLP corpus to test against.** OTLP logging
  is the least-adopted OTel signal; public corpora at the §8 sizes
  are legacy text. The demo captures are real OTLP but *friendly* —
  a dozen well-behaved services will never emit deeply nested
  `AnyValue` bodies, thousand-entry attribute maps, zero timestamps,
  or adversarial attribute keys. Production feeds are the only truly
  representative corpus and arrive only after deployment. Generation
  is the pre-production instrument that covers the space *around*
  the captures.
- **The §9.11 lesson generalises.** The 16 GiB run surfaced an
  input-shape-driven failure (unbounded template minting) that no
  existing corpus had triggered. RFC 0023 bounded it; this RFC makes
  "an input shape we didn't anticipate" a generated, repeatable test
  class instead of a paid-infrastructure discovery.
- **Query correctness has no oracle.** C1 pins reconstruction;
  RFC0022.3 pins old-file parity against the prior compile; but no
  test asserts that a DSL query returns *the right rows* against an
  independent evaluator over data the test didn't hand-shape. For a
  query backend, that is the invariant users actually rely on.

## 3. Design

### 3.1 Calibration manifests

A `calibration.json` per corpus release (committed under
`testdata/calibration/<corpus-tag>.json`, single-digit KiB),
extracted by a new `ourios-bench --calibrate <corpus-dir>` pass:

- attribute-count histogram (per-record resource + log attributes),
- body length histogram and `body_kind` mix,
- severity number/text distribution,
- `AnyValue` shape frequencies (string / int / double / bool / bytes
  / array / kvlist, and nesting depth),
- distinct-key counts for attribute keys (cardinality signal).

The manifest is a *measurement*, versioned with the corpus it
summarises; regenerating it is deterministic for a given corpus.

### 3.2 Generators

`proptest` strategies over [`OtlpLogRecord`] (the RFC 0003 §6.6
in-memory shape — generation happens past wire decode, which the
RFC 0003 equivalence suites already cover):

- **Calibrated mode** — field distributions weighted by a
  calibration manifest. Statistical, not exact: the §5 sanity
  criterion checks gross moments, not equality.
- **Adversarial mode** — uniform-ish over the envelope's legal
  extremes, bounded only by documented product limits: `AnyValue`
  nesting to the canonical-JSON depth bound, attribute maps to a
  few thousand entries, empty/absent everything, zero and `u64::MAX`
  timestamps, non-ASCII and confusable keys, text-heavy bodies past
  `max_line_tokens` (the Collector-fronted-legacy shape).

Both modes will live in **`crates/ourios-testgen`**, a dev-only
crate the calibration green slice introduces (no production crate
grows a proptest dependency; `ourios-bench` cannot host them because
it already depends on `ourios-querier`, and the querier's P4 suite
consuming generators from it would create a dev-dependency cycle).
The crate is test infrastructure; naming it in this RFC satisfies
`CLAUDE.md` §7, which treats any new crate as an architectural
commitment requiring an RFC. It will never be published, and nothing
in the workspace's production graph will depend on it.

### 3.3 The four properties

Over generated batches (both modes), through the real pipeline
(`MinerCluster` → RFC 0005 writer → reader / querier):

- **P1 — round-trip fidelity.** Every generated record's stored form
  round-trips per the RFC 0017/0018 fidelity contract; string bodies
  bit-identical, structured bodies canonical-JSON equal.
- **P2 — no silent merge.** A generated record's row carries either
  a template its line actually attached to under §6.3's zones, or
  `NO_TEMPLATE` with the body retained — never another line's
  template. (The §3.1 invariant, now under arbitrary input.)
- **P3 — bounds hold.** RFC 0023's three bounds are never exceeded
  mid-stream: template count ≤ ceiling, node fan-out ≤ cap,
  over-long lines always divert. (Generalises the tree-level fanout
  property to the full pipeline.)
- **P4 — the query oracle.** For a generated batch written to a
  store and a generated predicate from the supported DSL surface
  (severity / time-window / template-id / promoted- and
  non-promoted-attribute equality), the querier's row count equals
  an independent in-memory evaluator's over the same
  `MinedRecord`s. The reference evaluator is deliberately naive
  (linear scan, no DataFusion) — its correctness must be reviewable
  by eye.

Case counts: CI runs `proptest` defaults (fast, deterministic
regressions via committed failure persistence); a scheduled deep run
may crank `PROPTEST_CASES` (§7).

### 3.4 What this does not change

No production code paths, no schema, no telemetry. This is test
infrastructure; RFC 0006's corpus methodology and every recorded §9
number are untouched. The `LogHub` family keeps its role as the
Collector-output stress corpus.

## 4. Alternatives considered

- **More frozen corpora only.** Necessary (the v7 capture is
  happening) but not sufficient: a corpus can only contain what its
  emitters emitted; §9.11-class findings live in the combinations.
- **Fuzzing the full pipeline** (`cargo-fuzz`). The existing fuzz
  targets cover wire decode, where coverage-guided byte mutation
  shines. Pipeline invariants need *structured* inputs and
  cross-checked outputs — property testing's home ground.
- **Differential testing against another backend** (e.g. DuckDB over
  the same Parquet). Powerful but heavyweight; P4's naive evaluator
  buys most of the assurance at a fraction of the machinery, and the
  Parquet files remain externally checkable by hand when wanted.

## 5. Acceptance criteria

Scenario ids `RFC0024.<m>`.

> **Scenario RFC0024.1 — calibration extraction.**
> Given a corpus release, When `--calibrate` runs, Then a
> deterministic manifest is produced (byte-identical on rerun) and
> committed alongside the corpus tag it summarises.

> **Scenario RFC0024.2 — calibrated generators are shaped by the
> manifest.** Given a calibration manifest, When N records are
> generated, Then gross distribution moments (mean attribute count,
> body-length quartiles, severity mix) fall within a documented
> tolerance of the manifest's.

> **Scenario RFC0024.3 — P1 holds.** Round-trip fidelity over
> generated batches, both modes.

> **Scenario RFC0024.4 — P2 holds.** No silent merge over generated
> batches, both modes.

> **Scenario RFC0024.5 — P3 holds.** RFC 0023 bounds over generated
> streams with deliberately tiny configured bounds.

> **Scenario RFC0024.6 — P4 holds.** Query-oracle equality over
> generated batches and generated predicates, covering every
> operator class the DSL supports on each field kind — including at
> least one promoted-attribute predicate (RFC 0022's two-arm
> compile) and one non-promoted one.

> **Scenario RFC0024.7 — adversarial mode finds nothing today.**
> The full property set passes at an elevated case count on the
> adversarial generators. (This scenario is the regression tripwire:
> any future failure here is a minimal reproducer by construction.)

## 6. Testing strategy

The RFC *is* testing strategy; the §5 scenarios are the suites
themselves. Failure persistence files are committed so any generated
counterexample becomes a permanent regression case. The properties
run in the crates that own the invariant (miner: P2/P3; parquet: P1;
querier: P4) so a failure lands at the responsible boundary.

## 7. Open questions

1. **Deep-run cadence.** A scheduled high-case-count run (nightly?
   weekly?) vs CI-only defaults — decide once the suite's wall-clock
   is known.
2. **Trace/metric envelopes.** Out of scope (logs backend), noted
   only because the demo capture contains correlated trace ids that
   generators should populate realistically.

## 8. References

- RFC 0006 (bench corpus methodology — amended), RFC 0003 §6.6 (the
  generated shape) and its wire-decode property suites, RFC 0017/0018
  (fidelity contracts P1 pins), RFC 0022 §3.3 (the two-arm compile P4
  must cover), RFC 0023 (the bounds P3 pins; §9.11 for why generated
  shapes matter).
- Standing scope decision (2026-07-05): OTLP only; legacy formats are
  Collector concerns.
