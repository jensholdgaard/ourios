---
rfc: 0035
title: Ingest concurrency — relax the global miner serialization to per-tenant
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-20
supersedes: —
superseded-by: —
---

# RFC 0035 — Ingest concurrency

> **How to read this document.** A profile (`docs/benchmarks.md` §9.20/§9.21;
> issue #571) showed the ingest hot path saturates ≈ 86k lines/s while
> using only ~1.2 of 8 cores — an ~85%-idle machine held back by a
> **global** serialization, not by compute. This RFC relaxes that
> serialization. It touches the project's highest-risk invariants — WAL
> durability (§3.4), miner determinism (§3.5.3), tenancy (§3.7) — so it
> is design-first and change-nothing until `specified`→`red`→`green`.
> §§1–4 are the contract; §5 the acceptance criteria; §7 the open
> questions that gate `specified`. It changes **no on-disk byte** in its
> recommended form (Design A); the full-scaling alternative (Design B)
> would, and is deferred for that reason.

## 1. Summary

Every ingested batch, **for all tenants**, is serialized through one
global WAL-sequence gate (`CommitCoordinator::await_ingest_turn`) and one
global miner mutex (`Mutex<MinerCluster>`), and the expensive per-record
work — Drain match, template-id assignment, **and Parquet encoding** —
runs *inside* that single-file section (`pipeline.rs:314–354`). Because
the correctness constraint the gate exists for is only **per-tenant**
WAL-order (each tenant owns its Drain tree and template-id slice, §3.7),
the gate over-serializes: it makes tenant B wait behind tenant A's
encode.

This RFC keeps the **WAL append + fsync globally ordered** (durability is
non-negotiable — §3.4) and relaxes only the **miner hand-off**. It
recommends **Design A**: keep template-id assignment globally ordered and
cheap under the gate, but move the expensive, order-*insensitive* Parquet
encoding **off** the critical section onto a concurrent pool — capturing
the bulk of the idle headroom with **no on-disk format change and no
change to how template-ids are assigned**. It documents, and defers,
**Design B** (a genuinely per-tenant template-id space), which reaches the
full per-core ceiling but requires an on-disk `template_id` migration
(§3.5) and a query-surface change.

## 2. Motivation

### 2.1 The measurement

At the 86k ceiling the machine is ~85% idle (1.2 / 8 cores; §9.21
pidstat). Of the CPU that runs, ~52% is `MinerCluster::ingest` and ~16%
is Parquet encoding. Throughput is flat across 1 / 8 / 16 tenants and
across offered rate — the signature of one serialized lane. The bound is
the **serial fraction**, not total work: freeing the order-insensitive
majority of per-batch work onto idle cores is what unlocks throughput
(Amdahl — the achievable multiple is `1 / serial_fraction`, measured in
§6, not assumed here).

### 2.2 The two orderings, only one of which must stay global

- **(a) WAL append order — global, MUST stay.** The WAL is a single
  writer (`Wal::append(&mut self)`) behind the coordinator's journal
  mutex; seq assignment is atomic with the append (`commit.rs:190–204`).
  WAL-before-ack (§3.4) and recovery's strict-order replay
  (`recovery.rs:259–282`, ids re-derived in WAL order) depend on this.
  **Untouched.** Group commit still folds all tenants into one fsync.
- **(b) Miner hand-off order — currently global, relaxable to
  per-tenant.** The per-tenant Drain tree and structured-template map
  only require *their own* tenant's records in WAL-order. The global
  `ingest_gate` (`commit.rs:239–256`) and single `Mutex<MinerCluster>`
  make it global — the over-serialization this RFC targets.

### 2.3 The constraint that shapes the design

`MinerCluster::next_template_id` is a **single cluster-wide counter**
(`cluster.rs:116–126`): a template's concrete id depends on the *global
interleaving* of first-sightings across tenants. RFC 0001 §6.1 + §3.7.2
reconcile this as "the id *space* is cluster-wide, each tenant's slice
monotonic in that tenant's allocation order," with the hard rule *no
template-id shared across tenants* (pinned by
`invariant_3_7_2_same_template_two_tenants_distinct_template_ids`). This
is the pivot: relaxing hand-off order to per-tenant would let records
reach that shared counter in a different interleaving than global
WAL-order replay, changing assigned id *values* → §3.5.3 divergence.
**So per-tenant *mining* is unsafe as long as the id-space is a shared
counter touched in-line.** The two designs differ precisely in how they
resolve this.

## 3. Proposed design

### 3.1 Design A (recommended) — ordered id-assignment, concurrent encode

Split `MinerCluster::ingest` into an **ordered, cheap** phase and a
**concurrent, expensive** phase:

1. **Ordered phase (under the existing global gate, per batch):** for
   each record, Drain-match and **assign/look-up the template-id** —
   exactly today's ordering, so the cluster-wide counter is still
   advanced in strict WAL-append order and every id keeps its current
   value. Produce a `MinedRecord` carrying the assigned `template_id`,
   template version, and slot values. This phase does **no Parquet
   work**. It stays under the `ingest_gate` + a (now much shorter)
   critical section.
2. **Concurrent phase (off the gate):** hand each `MinedRecord` to a
   bounded worker pool that performs the Parquet encoding
   (`RecordSink::emit` / `encode_records_to_parquet_with_promoted`).
   Encoding is a pure function of the already-assigned id and the
   record's values — order-insensitive — so it parallelises across
   cores and tenants freely.

**Why determinism is preserved (unchanged, not re-argued):** template-id
assignment — the *only* order-sensitive step — stays globally ordered
under the gate. Ids keep their exact values; the live tree still equals a
WAL-order replay; snapshots (which capture the **trees**, updated in the
ordered phase) stay coherent, so the rotation barrier the global gate
provides for free is preserved (`pipeline.rs:343–354`). No `template_id`
representation changes → **no Parquet schema change, no §3.5 migration,
no query-surface change.**

**What must be verified (Design A's real risks, §7):**
- **Parquet row-order independence.** Concurrent encode may buffer a
  tenant/partition's rows in a different order. Queries filter by
  predicate, not position, and C1 reconstruction is per-record, so this
  should be semantically inert — but the RFC must confirm no test or
  invariant depends on intra-file row order, and that per-partition
  `RecordSink` buffering is concurrency-safe (today it is written under
  the single miner lock).
- **Audit-sink ordering.** The shared `audit_sink` (`cluster.rs:127–135`)
  has an RFC 0001 §6.4 "ordering-plus-durability-barrier" contract.
  Template-created/widened audit events are produced in the *ordered*
  phase (they are id-assignment events), so they stay ordered; the RFC
  must confirm no audit emission moves into the concurrent phase.
- **Backpressure.** The concurrent pool's queue must bound memory and
  apply backpressure to the gate so an encode-bound burst can't grow an
  unbounded in-flight backlog (ties into D2 / hazard #4).

### 3.2 What stays exactly as-is

The WAL, the coordinator's global seq + group-commit fsync, WAL-before-ack
ack timing, the cluster-wide template-id space and its on-disk
representation, the query DSL, and the snapshot/restore format. Design A
is an *internal* re-partitioning of `MinerCluster::ingest` into
ordered-vs-concurrent phases behind the same public contract.

## 4. Alternatives considered

- **Design B — genuinely per-tenant template-id space (full scaling,
  deferred).** Make each tenant's mining fully independent: per-tenant
  ordering gate, per-tenant miner lock, and a per-tenant id-space that
  still satisfies §3.7.2 cross-tenant uniqueness (a compound/namespaced
  id, e.g. `(tenant_ordinal, per_tenant_seq)`, **not** a naive per-tenant
  `u64` — which the code explicitly warns collides, `cluster.rs:120–122`).
  This reaches the full per-core ceiling (the ~341k independent-lane
  approximation, §9.20). **Deferred because** changing `template_id`'s
  representation is an on-disk Parquet **schema change** (§3.5 migration
  plan required — historical files, reader forward-compat) that ripples
  into the query surface (`template_id == N`), the audit stream, and the
  snapshot format. That is a large, separate commitment; Design A should
  ship and be measured first, and B revisited only if A's measured
  ceiling (§6) leaves the D1 must-win unmet. Per-tenant snapshot
  high-water marks (or a rotation drain-barrier) would also be required
  (`recovery.rs:199–203` stamps one global mark on every tenant today).
- **Per-tenant miner lock only, keep the global gate.** Insufficient —
  the global `ingest_gate` still serialises; and unsafe — per-tenant
  mining into the shared counter reorders id assignment (§2.3).
- **Do nothing; recast D1 down to ~86k (RFC 0034 option B).** Rejected as
  the primary path (this RFC exists because ~86k is a software artifact,
  not a ceiling — recasting would enshrine it, the §6.2 "don't weaken the
  spec to match the code" trap). RFC 0034 remains, sequenced *after* this
  RFC: recalibrate D1 against Design A's measured number.

## 5. Acceptance criteria

> **Scenario RFC0035.1 — determinism is preserved (the load-bearing
> guard).**
> - **Given** Design A implemented and a multi-tenant workload
> - **When** N tenants ingest concurrently and the live miner state is
>   compared to a control that replayed the same WAL frames in strict
>   global order
> - **Then** every tenant's `snapshot_state` (leaves, template ids,
>   versions) is **equal** to the control — i.e. concurrent encode does
>   not perturb any assigned id or tree.
> - **And** the existing `rfc0008_8_concurrent_ingest_preserves_wal_order_at_the_miner`
>   passes unchanged, extended by a **new multi-tenant variant** (the
>   current one is single-tenant and would not catch cross-tenant
>   reordering).

> **Scenario RFC0035.2 — snapshot/restore + rotation stay coherent.**
> - **Given** Design A and a WAL rotation under concurrent ingest
> - **When** the rotation snapshot fires and the process later restores
>   from it plus tail replay
> - **Then** restore + tail == full rebuild (per tenant), and the rotation
>   hook still observes the pre-rotation high-water with no batch above
>   the mark applied — the full `rfc0001_3_5_*` and `rfc0008_10_*` suites
>   pass unchanged.

> **Scenario RFC0035.3 — WAL-before-ack and durability are untouched.**
> - **Given** Design A
> - **When** N concurrent exports are acked
> - **Then** each is durable before its ack, exactly N frames land, and
>   group commit still folds them into shared fsyncs —
>   `rfc0003_15_*` and `rfc0008_8_batched_fsync` pass unchanged.

> **Scenario RFC0035.4 — the serialization is actually relaxed
> (the point).**
> - **Given** Design A on `baseline-8vcpu-32gib`, `soak --tenants 8`
> - **When** the saturating soak runs
> - **Then** node throughput exceeds the pre-RFC ~86k by the §6 measured
>   multiple with core utilisation materially above 1.2/8, p99 ack ≤
>   200 ms at the sustained rate, and D2 still PASS — recorded in the §9
>   series and feeding RFC 0034's D1 recalibration.

> **Scenario RFC0035.5 — no on-disk or query change (Design A scope
> guard).**
> - **Given** Design A
> - **When** files written before and after the change are read, and
>   `template_id == N` queries run
> - **Then** the Parquet schema, `template_id` values, and query results
>   are identical — Design A introduces no migration.

## 6. Measurements

> **To be filled from a prototype + baseline run.** Design A's achievable
> multiple is `1 / serial_fraction` after moving encode off the gate; the
> serial fraction is measured by prototyping the ordered/concurrent split
> and re-running the §9.21 profile (per-thread CPU + throughput) on
> `baseline-8vcpu-32gib`. This number sets both RFC0035.4's target and
> RFC 0034's recalibrated D1 bar. The pre-RFC baseline is ~86k at 1.2/8
> cores (§9.21).

## 7. Open questions

- [ ] **Prototype the split and measure the serial fraction** (the §6
  number) before `specified` — it decides whether Design A alone clears
  the D1 must-win or whether Design B is eventually needed.
- [ ] Confirm no test/invariant depends on intra-file Parquet **row
  order**, and make per-partition `RecordSink` buffering concurrency-safe
  (or shard it per tenant/partition).
- [ ] Confirm all **audit** emissions stay in the ordered phase (RFC 0001
  §6.4 barrier); if any are in the record-emit path, keep them ordered.
- [ ] Backpressure design for the concurrent encode pool (bound memory;
  propagate to the gate; interaction with D2 backlog / hazard #4).
- [ ] Worker-pool sizing and whether encode runs on the tokio blocking
  pool or a dedicated rayon-style pool (CPU-bound work off the async
  runtime).
- [ ] `CLAUDE.md` §3.4/§3.5.3/§3.7 are *preserved* by Design A, so no
  `meta:` RFC — confirm at sign-off. (Design B *would* touch §3.5 —
  a separate RFC if pursued.)

## 8. References

- **Issue #571** — the profile finding this RFC resolves.
- `docs/benchmarks.md` §9.20 / §9.21 — the ~86k ceiling, 85%-idle
  profile, and the ~341k independent-lane approximation (Design B's
  ceiling); §1 `baseline-8vcpu-32gib`.
- **RFC 0034** (`drafted`) — D1 re-scope, sequenced *after* this RFC:
  recalibrate the D1 bar against Design A's measured number.
- **RFC 0001** (`accepted`) — the template miner: §6.1 template-id
  semantics, §3.7.2 cross-tenant uniqueness, §3.5.3 snapshot-restore,
  §6.4 audit-sink barrier — the invariants this RFC must preserve.
- **RFC 0008** (`accepted`) — WAL: single-writer append order, group
  commit, WAL-before-ack (§3.4), rotation cadence.
- Code: `pipeline.rs:314–354` (the serialized region), `commit.rs:239–256`
  (`ingest_gate`), `cluster.rs:116–126` (the shared `next_template_id`),
  `recovery.rs:199–205 / 259–282` (global high-water; strict-order
  replay).
- `CLAUDE.md` §3.4 (WAL-before-ack), §3.5 (schema-change migration — why
  Design B is deferred), §3.5.3 (snapshot determinism), §3.7 (per-tenant
  trees — why the constraint is per-tenant, not global).
