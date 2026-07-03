---
rfc: 0021
title: Coordinated DataFusion / Arrow upgrade — phased behind upstream
status: red
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-03
supersedes: —
superseded-by: —
---

# RFC 0021 — Coordinated DataFusion / Arrow upgrade, phased behind upstream

## 1. Summary

Upgrade the storage/query dependency stack (epic #314) in **two phases,
each following what upstream has actually shipped**:

- **Phase 1 (now): DataFusion 53.1 → 54.0, one arrow.** DataFusion 54
  pins arrow/parquet `^58.3` — the same arrow the querier already pulls
  — so `ourios-parquet` moves from arrow/parquet 55.2 to 58.3 and the
  whole workspace unifies on a single arrow. That removes the RFC 0017
  row-path workaround (#276): the dual decoder and the
  `schema_force_view_types = false` override exist only because two
  arrow major versions coexist. MSRV moves 1.85 → 1.88 (DataFusion 54's
  floor).
- **Phase 2 (when upstream ships it; expected around DataFusion 55):
  object_store ≥ 0.14 and parquet 59.** parquet 59 drops the `thrift`
  dependency entirely (clears the GHSA-2f9f-gq7v-9h6m advisory, #295);
  a DataFusion release that carries object_store 0.14 unblocks #310 and
  lifts the renovate hold (#313). The quick-xml `deny.toml` ignores
  (RUSTSEC-2026-0194/0195) are removed in this phase **iff** the
  object_store release pins quick-xml ≥ 0.41 — that may trail phase 2.

The phase boundary is not a preference; it is where upstream currently
is: **no released DataFusion accepts object_store 0.14 or parquet 59**
(DataFusion 54.0.0 pins `object_store ^0.13.2`, `parquet ^58.3.0`).

## 2. Motivation

### 2.1 The security cluster

`thrift 0.17` (GHSA-2f9f-gq7v-9h6m, DoS) enters via `parquet 55.2` and
is still pinned by `parquet 58.3`; it is gone only in `parquet 59`. The
advisory is GHSA-only today, so `cargo deny` does not fail yet — but it
will the day RustSec mints an id. The quick-xml DoS advisories
(RUSTSEC-2026-0194/0195) are accepted in `deny.toml` with an explicit
removal condition that also sits behind this upgrade chain. Waiting for
a single coordinated bump keeps both windows open longer than needed;
phase 1 shrinks the eventual security-driven change to a small step.

### 2.2 The dual-decoder debt (#276)

The RFC 0017 row read path decodes DataFusion's arrow-58
`RecordBatch`es separately from `ourios-parquet`'s arrow-55 reader, and
pins `schema_force_view_types = false` to keep the two schemas
compatible. Every read-path feature pays this tax twice. Unifying on
one arrow removes the second decoder and restores the upstream
view-types default.

### 2.3 Pillar drift

Parquet-on-disk and DataFusion-as-engine are §2 pillars; the longer the
stack sits behind upstream, the larger (and riskier) the eventual jump
on exactly the code we can least afford to destabilise. Phasing keeps
each jump small.

### 2.4 Why the phases follow upstream

The version locks, as of this writing:

| Lock | Fact |
|---|---|
| DataFusion 54.0.0 → arrow/parquet | pins `^58.3.0` |
| DataFusion 54.0.0 → object_store | pins `^0.13.2` |
| parquet 58.3 → thrift | pins `^0.17` (vulnerable); dropped in parquet 59 |
| object_store 0.14.0 → quick-xml | pins `^0.40.1` (< patched 0.41) |
| ourios-querier ↔ ourios-parquet | must share one `object_store` (the querier registers `Store::object_store()` with DataFusion's `SessionContext`, RFC 0013 §2.2) |
| DataFusion 54.0.0 → rustc | MSRV 1.88.0 (workspace documented 1.85 pre-phase-1; now 1.88) |

A "single coordinated bump" resolving the whole epic is therefore not
constructible from released crates today. What *is* constructible now —
arrow unification — happens to be the riskiest part (it touches the
on-disk format pillar), and doing it in isolation means the
property/corpus/reconstruction suites validate exactly one change.

## 3. Proposed design

### 3.1 Phase 1 — DataFusion 54, arrow 58 everywhere, MSRV 1.88

One coordinated workspace bump: `datafusion = 54`, and `ourios-parquet`
moves `arrow`/`parquet` 55.2 → 58.3 so the lockfile carries a **single
arrow major**. Expected churn:

- `ourios-parquet`: writer, reader, schema declaration,
  `encode_records_to_parquet` — arrow 55 → 58 API changes. This is the
  load-bearing on-disk format (§2 pillar #1); the §3.3 and §3.5
  invariants below bound the change.
- `ourios-querier`: DataFusion 53 → 54 API changes (logical plans,
  `ListingTable` / `SessionContext`, pruning statistics); **removal of
  the RFC 0017 dual decoder** and the `schema_force_view_types = false`
  override (#276).
- `ourios-bench`: compile-level churn only.
- `rust-toolchain.toml` note + workspace `rust-version` 1.85 → 1.88
  (documented per CLAUDE.md §6.1; CI already runs a newer stable).

**What must not change (the §3 invariants this RFC touches):**

- **On-disk bytes are the contract (§3.5).** The upgrade introduces no
  schema change: field names, types, repetition, and the RFC 0005 §3.2
  column set stay identical. Files written before the upgrade MUST read
  identically after it (RFC0021.2). Any arrow-58 behaviour change that
  would alter written bytes (encodings, statistics defaults) must be
  pinned back to the current behaviour or explicitly RFC'd as a §3.5
  schema migration — not absorbed silently.
- **Bit-identical reconstruction (§3.3).** The reconstruction property
  and corpus tests run unchanged and must stay green.
- **The compactor's conditional-PUT CAS (RFC 0013 §3.3/§3.4)** is
  untouched — object_store does not move in this phase.

### 3.2 Phase 2 — object_store ≥ 0.14 + parquet 59 (upstream-gated)

Opens when a released DataFusion carries them (watch DataFusion 55).
Scope, known today:

- `object_store` 0.13 → 0.14+ API churn concentrated in
  `crates/ourios-parquet/src/store.rs` (`AmazonS3Builder`,
  `PutMode`/`PutOptions`, `S3ConditionalPut`, `list`/
  `list_with_delimiter`, `UpdateVersion`). The compactor's
  publish-CAS (RFC0013.3/.4) must be preserved and is re-proven by the
  existing localstack suite.
- `parquet` 59: `thrift` leaves the lockfile → close #295.
- Supply chain: object_store 0.14 pulls new transitives (aws-lc-rs /
  aws-lc-sys family among them) — `deny.toml` licenses/advisories
  re-cleared, `osv-scanner.toml` updated if needed.
- Renovate: lift the `<0.14.0` hold (#313); close #310.
- quick-xml: drop the RUSTSEC-2026-0194/0195 ignores **iff** the
  object_store release pins quick-xml ≥ 0.41; otherwise the ignores
  stay with their documented removal condition.

### 3.3 Non-goals

No Parquet schema change, no logs-DSL surface change, no `Store` trait
change, no query-semantics change. This RFC is a dependency migration
with pinned invariants, not a feature vehicle.

## 4. Alternatives considered

### 4.1 Wait for DataFusion 55 and do one coordinated bump
Rejected: it couples the riskiest migration (arrow 55 → 58 on the
on-disk pillar) with the object_store API churn in the same change,
leaves #276's dual decoder and the security windows open for longer,
and gambles on DataFusion 55's actual contents. Phase 1 is exactly the
de-risking slice: upstream's own arrow unification with nothing else
moving.

### 4.2 Fork/patch object_store 0.13 onto quick-xml 0.41
Rejected: a patched fork of a supply-chain-sensitive crate trades a
documented, low-exposure DoS ignore for permanent maintenance burden
and a worse provenance story.

### 4.3 Bump only `ourios-parquet` to parquet 59 (thrift fix first)
Rejected: parquet 59 means arrow 59 in `ourios-parquet` while
DataFusion 54 carries arrow 58 — reintroducing the dual-arrow split
(#276) one version higher, on the read *and* write path this time.

## 5. Acceptance criteria

Scenario ids `RFC0021.<m>`. Phase 1 = `.1`–`.6`; phase 2 = `.7`–`.9`
(**upstream-gated**: their stubs land red only when phase 2 opens).

> **Scenario RFC0021.1 — one arrow.**
> Given the phase-1 bump,
> When the workspace lockfile is inspected,
> Then exactly one arrow major (58.x) and `datafusion 54.x` are
> present, and the workspace builds with MSRV 1.88.

> **Scenario RFC0021.2 — old files read identically (§3.5).**
> Given Parquet files written by the pre-upgrade writer (committed
> fixture + freshly generated),
> When the post-upgrade reader reads them,
> Then every row and column decodes identically to the pre-upgrade
> reader's output, with no schema-mismatch errors.

> **Scenario RFC0021.3 — reconstruction stays bit-identical (§3.3).**
> Given the existing reconstruction property and corpus suites,
> When they run on the upgraded stack,
> Then they pass unchanged (no test weakened or deleted).

> **Scenario RFC0021.4 — the dual decoder is gone (#276).**
> Given the RFC 0017 row read path,
> When a query renders rows end-to-end,
> Then decoding goes through the single unified arrow path,
> `schema_force_view_types` is no longer overridden, and the RFC 0017
> suites pass.

> **Scenario RFC0021.5 — the pruning thesis holds (B1/B2).**
> Given the benchmarks.md B1/B2 gates,
> When the query benchmarks run on the upgraded stack (indicative
> ci-runner pass; authoritative baseline rerun on maintainer opt-in),
> Then selective-query row-group pruning shows no regression beyond
> run-to-run noise.

> **Scenario RFC0021.6 — the full gate is green.**
> Given the phase-1 change,
> When CI runs,
> Then the complete suite passes — including `s3 integration
> (localstack)` (the CAS paths, untouched) and `live-check (weaver)`.

> **Scenario RFC0021.7 (phase 2) — CAS survives object_store 0.14.**
> Given the object_store bump,
> When the RFC0013.3/.4 conditional-PUT localstack suites run,
> Then concurrent-sweep publish semantics are preserved.

> **Scenario RFC0021.8 (phase 2) — thrift is gone.**
> Given the parquet 59 bump,
> When the lockfile is inspected,
> Then no `thrift` crate is present (#295 closed).

> **Scenario RFC0021.9 (phase 2) — supply chain re-cleared.**
> Given the new transitive set,
> When `cargo deny check` runs,
> Then it passes with the renovate hold lifted (#313) and the
> quick-xml ignores removed iff object_store pins quick-xml ≥ 0.41.

## 6. Testing strategy

Per CLAUDE.md §6.2. The existing suites are the oracle — this RFC adds
one artefact and changes no test semantics:

- **Fixture for RFC0021.2:** before the bump, a small Parquet file
  (representative rows: structured + templated bodies, attributes,
  non-finite doubles) is generated by the current writer and committed
  under `testdata/`; a new test reads it and asserts decoded equality
  against its committed expected rows. This makes "old files still
  read" a permanent regression test, not a one-off migration check.
- Property/corpus/reconstruction suites (§3.3), the RFC 0005 §3.9
  absent-column tests, and the RFC 0017 suites run unchanged.
- RFC0013.3/.4 localstack CAS tests re-prove the store seam (phase 1:
  unchanged; phase 2: the actual subject).
- Benchmarks: B1/B2 indicative on ci-runner first, per the standing
  bench policy; the paid baseline rerun only on explicit opt-in.

## 7. Open questions

- [ ] DataFusion 55 contents and timing — does it pick up object_store
  0.14 and parquet 59 together? (Determines whether phase 2 is one
  step or two.)
- [ ] aws-lc-rs / aws-lc-sys license family under `deny.toml`'s
  allow-list when object_store 0.14 lands (ISC + Apache-2.0 variants;
  needs review, possibly an `exceptions` entry).
- [ ] Is an authoritative baseline B1/B2 rerun wanted after phase 1, or
  indicative-only until phase 2 completes the epic?
- [ ] MSRV cadence: 1.85 → 1.88 is forced here; do we want a documented
  policy (e.g. "MSRV may follow DataFusion's floor") instead of
  per-RFC decisions?

## 8. References

- Epic #314 (this RFC), #310 (object_store 0.14, blocked), #295
  (thrift GHSA-2f9f-gq7v-9h6m), #276 (RFC 0017 dual decoder), #313
  (renovate hold).
- RFC 0011 (A1 demotion — context for the bench gates), RFC 0013
  (`Store` / object_store seam, CAS), RFC 0017 (row read path).
- CLAUDE.md §2 (pillars #1, #3), §3.3 (bit-identical reconstruction),
  §3.5 (schema migration), §6.1 (MSRV), §6.2 (tests are
  specifications).
- deny.toml RUSTSEC-2026-0194/0195 ignore block (removal condition).
