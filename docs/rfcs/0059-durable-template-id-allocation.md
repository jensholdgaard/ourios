---
rfc: 0059
title: Durable template-id allocation (RFC 0001 amendment)
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-10-05
supersedes: —
superseded-by: —
---

# RFC 0059 — Durable template-id allocation (RFC 0001 amendment)

> **Status: `specified`.** §5 lists the acceptance criteria. This RFC
> amends RFC 0001 §6.1 (template identity), §6.9 (persistence and
> recovery) and scenario §3.5.3, and RFC 0023 §3.4 (the parse-failure
> reason enum). RFC 0001 keeps the old text with a dated pointer here,
> the way RFC 0023 amended it.

## 1. Summary

`template_id` is allocated by one in-memory counter per process. A
restart rebuilds that counter from what recovery restores and replays,
so a discarded snapshot, reclaimed WAL frames, or a replaced local root
can restart it below ids that Parquet rows and audit events already
carry, and the next new template takes one of them (#898). This RFC makes
uniqueness durable:
- one small object in the store records the highest id ever reserved;
- the allocator draws only from blocks it has reserved there, before
  using them;
- every start allocates above it.

The first start of an existing deployment computes the object from
every data and audit file footer, so the floor is provable and needs no
margin.

## 2. Motivation

**The bug.** #898's scenario tests, run against `main` at `09e493ed`,
show both outcomes:
- a new shape after a discarded snapshot took id 1, which the audit
  stream already binds to `user alice logged in`;
- re-minting that old shape took id 2, which belongs to another
  template.

The querier's registry fold is last-wins per `(template_id, version)`,
so existing rows then render against the wrong text. That breaks
`CLAUDE.md` §3.1 (no silent merges) and §3.3 (bit-identical
reconstruction).

**Why the audit stream cannot be the floor.** #908 first seeded the
counter from the audit stream's highest id. Review showed three holes:
- structured templates emit no audit event (RFC 0001 §6.2 step 0);
- an audit write that fails permanently is dropped while its records
  still publish (RFC 0056);
- a replaced local root looks like a fresh node, so nothing triggered
  the read at all.

**Why it is reachable today.** The 0.10.0 → 0.11.0 → 0.11.1 upgrade
discarded every version-1 and version-2 snapshot (RFC 0052 §3.1, RFC
0001 §6.9's 2026-09-30 amendment). It did so on a node whose WAL
reclamation had already removed the frames below those snapshots.

**Why at this layer.** Object storage is the source of truth (`CLAUDE.md`
§3.6). A counter that must outlive any local state belongs there, not in
a local snapshot that is, by RFC 0001 §6.9's own definition, a
rebuildable cache.

## 3. Proposed design

### 3.1 The high-water object

- **Key.** One object per store, at **`miner/template_ids.v1.json`**.
  - `miner/` is a new top-level prefix beside `data/`, `audit/`,
    `erasure/` and `backfill/`.
  - No audit or data scan lists it, since both scope their listings to
    their own prefixes.
- **Format.** The version is in the filename, following RFC 0033's
  `template_map.v2.json.zst`. The body is JSON, following the RFC 0009
  `manifest.json` and the RFC 0047 erasure markers:

  ```json
  {"reserved_through": 4021000}
  ```

  `reserved_through` (`N`) is a `u64` written as a JSON integer. It is
  the highest id any allocator has been allowed to issue. Readers
  ignore unknown fields within version 1.
- **Read failures fail closed.** A reader fails when:
  - the key exists but cannot be fetched or parsed;
  - `reserved_through` is absent or is not a `u64`;
  - `miner/` holds any `template_ids.v<k>.json` with `k` above the
    version the reader knows, **whether or not** its own version's key
    also exists.
- **Why every later version is refused.** RFC 0033's map is a cache: a
  reader that skips an unknown version just rebuilds. This object is
  the reverse, the one record that keeps ids unique. A later version may
  have moved the authoritative high-water to its own key and left the v1
  object stale, so an older binary that trusted v1 could re-issue ids
  the newer one already handed out. The only safe reading of an unknown
  version is to refuse to start (RFC0059.11). A future format change
  therefore keeps writing v1 alongside, or ships with a migration that
  every binary still running can read.
- **Absent** means no `template_ids.v<k>.json` of any version exists.
  That alone triggers the bootstrap (§3.5).

### 3.2 Reservation: write before allocate

An allocator holds a **current block** `(a, b]` and allocates ids from
it in increasing order. It never allocates an id above `b`.

**Reserving a block**, given `f`, the highest id this allocator has
restored or issued:

1. Read `N` and its `ETag`.
2. Compute `a = max(N, f)` and `b = a + BLOCK`. Fail if `b` would exceed
   `u64::MAX - 1`, so `u64::MAX` is never issued.
3. Write `N' = b` as a compare-and-swap against the `ETag` (§3.6). On a
   precondition failure, re-read and retry from step 1.
4. Only once the write is durable does `(a, b]` become usable.

**Crash semantics.** A crash between the write and the use leaves ids
in `(a, b]` unissued, and they are skipped. Ids are `u64`, so these
gaps cost nothing. Nothing in Ourios reads id density (§3.7).

**`BLOCK = 1000`, a constant.** New templates are rare next to lines:
- RFC 0023 bounds a tenant at 20,000 tree templates;
- the C2 gate corpora plateau in the tens of templates (RFC 0001
  `benchmarks.md` §9.5).

So a block lasts hours to days, and reservation runs at a negligible
rate. A smaller block raises the refill rate and the chance of an empty
range under a burst (§3.3). A larger one only wastes more ids per crash,
which is free.

### 3.3 No object-store I/O under the miner lock

The miner allocates under its lock (RFC 0035 §3.1's ordered phase), so
no reservation ever runs there.
- **Holding a block ahead.** The miner holds its current block and at
  most one **ready block**. When the current block is used up, the
  ready block becomes current. That is an in-memory swap.
- **Background refill.** Each time a block becomes current, the
  ingester's background refiller reserves the next ready block off the
  lock. If the store fails, it retries with capped exponential backoff
  (100 ms doubling to 30 s).
- **Exhausted range.** If both blocks are used up before a refill
  lands, every **fresh** allocation fails immediately: a new tree leaf,
  an adoption-interned template, or a first-seen structured key.
  - Each such line is emitted as a parse failure: `template_id = 0`, its
    body retained (`CLAUDE.md` §3.3 holds through the body), and
    `lossy_flag = true` for string bodies.
  - It is counted on the existing `ourios.miner.parse_failures` counter
    under `ourios.miner.parse_failure.reason = id_reservation_failed`.
  - Lines that match an existing template keep attaching to it.
  - An adoption that would intern is not attempted. The line is mined
    instead, and either attaches or counts as above.
  - Allocation resumes as soon as the refiller hands over a block.
- **Precedence.** RFC 0023 §3.1's per-tenant ceiling is checked before
  id availability. A line at the ceiling reports `template_ceiling`,
  not `id_reservation_failed`.

### 3.4 Every start

Recovery reads `N` **before** replay, after restoring the snapshots
(RFC 0001 §6.9):
- **Object present.** Recovery seats the allocator above
  `max(N, highest restored id)`. It reserves the first current block
  and the first ready block synchronously, before the listeners open,
  so startup is the one place a reservation blocks. Replay then mints
  from that block.
- **Object absent** (no version of it, §3.1). Recovery bootstraps it
  (§3.5), then proceeds as above.
- **Read fails.** Startup fails closed. This is the same trade-off as
  RFC 0052's fail-closed checks and #791 (recovery during an
  object-store outage): a restart then needs the store reachable, but a
  guessed floor could silently bind published rows to another template.

**The guarantee.** Write-before-allocate makes every id ever issued at
most `N`. Every start allocates only above `N`. So nothing replay or
later ingest mints can equal an id issued before the restart, whatever
recovery restored, discarded, or never found. A replaced local root is
covered too: it has no snapshots and no WAL, but `N` is in the store.

**Restore equivalence** (amends RFC 0001 scenario §3.5.3, approved by
the maintainer on 2026-10-05):
- A template **first allocated during tail replay** now takes an id
  from the fresh block, not the one the uninterrupted process would
  have issued.
- Restore plus tail replay therefore equals a full rebuild only up to
  an **injective renaming of the ids first minted in tail replay**.
- Restored ids, versions, slot types, routes, the structured map, and
  every other field remain exactly equal.
- No renamed id equals an id issued before the restart.

**When the renaming is observable.** In the steady state a tenant's
frames above its horizon `S` also lie above the checkpoint `X` (`S ≥ X`,
RFC 0001 §6.9's 2026-06-12 amendment). They were never published, so the
renaming is invisible. Only a lagging snapshot (`S < X`) re-mints
templates whose rows in `(S, X]` were already published. That is
drift, surfaced by RFC 0010, never a collision.

### 3.5 Bootstrap: a provable floor

**Trigger.** The object is absent. This happens on the first start of a
deployment upgrading to this RFC, or on a new store. The first start
computes:

```text
floor = max(data_max, audit_max, restored_max)
```

**`data_max`.** The highest `template_id` in the statistics of every
data file under `data/`.
- This covers every published row, structured or not, whatever the
  audit stream holds. The data writer enables page-level statistics on
  every column (RFC 0005 §3.6, `writer.rs`), and those include chunk
  min/max for `template_id`.
- The read goes footer only, one file at a time. One suffix-ranged
  GET of the last 64 KiB returns the footer and the object's size, and a
  second, larger one follows only when the footer is bigger than that.
- A file with a row group lacking usable statistics comes down whole,
  and only its id columns are decoded. The writers record the
  statistics, so a file that needs this is a pre-statistics or foreign
  file.
- No row is ever materialised.

**`audit_max`.** The same footer read over every `audit/` file. It
covers the `template_id` column, plus `alias_representative_id` and the
`alias_member_ids` leaf, since alias events carry ids too.

**`restored_max`.** The highest id in the snapshots this start restored.
Those can hold ids whose rows were not yet published.

**Why no margin is needed.**
- An id that no surviving data row, audit event or restored snapshot
  carries binds nothing durable. If it is re-issued, no existing
  artefact mis-renders. Quarantined records (RFC 0025) persist no id.
- Rows whose ids were erased (RFC 0047) or aged out are gone, so their
  ids bind nothing either.
- `reserved_through = floor`, and the allocator starts at `floor + 1`
  by construction.

**Listing cost.** The store's listing seam returns one `/`-delimited
level per call (`Store::list_delimited_blocking`: the objects directly
under a prefix plus its immediate child prefixes). Data partitions nest
`data/tenant_id=…/year=…/month=…/day=…/hour=…/`, so the walk issues one
delimited `LIST` at every directory of every level:

```text
LISTs = 1 (data/) + tenants + tenant-years + tenant-months
      + tenant-days + tenant-hours
```

That is dominated by the hour partitions. A backend pages a large
directory, at 1,000 keys per page on S3, and each page is one more
request. `audit/` adds the same walk over its
`tenant_id=…/year=…/month=…/day=…/` levels.

On top of the listings, every file costs one suffix GET (two when its
footer exceeds 64 KiB). On the reporter's node that is about 620,000
footer reads and, for two tenants with a year of hourly partitions,
about 17,500 hour-level `LIST`s.

This is the whole-prefix walk #853 warns about. It is paid once per
store, at its first start under this RFC, and never again.

**Memory.**
- **Listings.** The walk is depth-first. It holds one directory's
  listing at a time, plus the prefixes still to visit. Those are at
  most the unvisited siblings at each level of the current path, so a
  few hundred names, never the prefix's keys.
- **Files.** Each file's footer, or a whole pre-statistics file, is
  dropped before the next one is fetched.
- **Peak.** The scan's peak heap is one directory listing plus one
  file, independent of the store's size (RFC0059.7).

**Progress.** Every 10,000 files, the bootstrap logs
`ourios.receiver.template_ids.bootstrap.progress` (§3.9).

**Crash mid-bootstrap.** Nothing is written until the scan completes. A
crash or restart during the scan leaves no object, so the next start
redoes the scan from the beginning. The write is create-if-absent
(`put_if_absent`). A start that loses that race, to a concurrent replica
(§3.6), reads the winner's object instead. It then proceeds as in §3.4:
its first reservation still uses `f ≥ restored_max`.

**Multi-replica upgrade.** Before this RFC, two receivers sharing a
store each started counting at 1, so a multi-replica deployment already
has colliding ids.
- **Detecting them.** #908's read-only DuckDB procedure finds them.
- **Ordering the upgrade.** The upgrade bootstraps from what the store
  holds, plus the restoring replica's own snapshots. A replica that
  starts later may restore snapshot ids above the bootstrap floor that
  it minted pre-upgrade and never published, and the first replica may
  have reserved those ids already.
- **The rule.** Upgrade a multi-replica deployment to this RFC with
  `receiver.replicas = 1`, then scale out.
- **Ready-gating does not close the window.** The chart's StatefulSet
  uses the default `OrderedReady` policy (no `podManagementPolicy` in
  `receiver-statefulset.yaml`), so replica 0 bootstraps before replica 1
  starts. That ordering still leaves the window above, which is why the
  rule exists.

### 3.6 Several receivers on one store

**What the chart allows.**
- The Helm chart runs the receiver as a StatefulSet whose
  `receiver.replicas` an operator may raise (`values.yaml`, default 1).
- Each replica has its own WAL PVC, and all of them write one shared
  store.
- The receiver Service load-balances, and nothing routes a tenant to
  one replica.

**S3-compatible stores.** Reservation is therefore a compare-and-swap on
the one object. This is RFC 0013's manifest-swap primitive,
`Store::put_if_match`, including its unquoted-`ETag` retry:
- it writes with `If-Match: <etag>`;
- it creates with `If-None-Match: *` (`put_if_absent`) when the object
  is absent;
- on a precondition failure it re-reads and retries, up to 16 attempts
  per reservation;
- a reservation that still loses is a failed reservation (§3.3).

The store linearises conditional writes, so every replica's blocks are
disjoint, and no per-node key is needed.

**The local backend.**
- It has no `If-Match` (RFC0019.7: it commits by atomic overwrite, last
  writer wins). The reservation writes `N'` by overwrite there.
- **One receiver per local store** is a stated constraint. It is the
  same constraint under which the chart's shared local PVC is coherent
  at all (`data-pvc.yaml`: "coherent only on one node or with a
  ReadWriteMany class").

**Out of scope.** The same template reaching two replicas gets two ids.
That is drift across replicas (hazard #5), not a collision.

### 3.7 Monotonicity

RFC 0001 §6.1 said `template_id` is "a cluster-wide unique monotonic
`u64`". That is amended as follows:
- ids are **unique cluster-wide**;
- ids are **strictly increasing per allocator**, across that
  allocator's restarts, because every block lies above every earlier
  one;
- ids are **not dense**;
- ids are **not monotonic across replicas**: two replicas interleave
  disjoint blocks.

Nothing relies on cross-replica order. Each place that orders or compares
ids:

| Where | What it does with id order | Effect |
|---|---|---|
| Querier registry fold `template_registry.rs:110`, `RegistryFold::push` | Keys by `(template_id, version)` and orders by `(timestamp, file path, row)` (RFC 0005 §3.7.1) | No id order involved |
| Template map `template_map.rs:605` | Sorts entries by `(template_id, version)` | Serialisation determinism only |
| Drift query `drift.rs:155–161` | `ORDER BY widening_count DESC, template_id ASC` | Display tiebreak |
| Alias store `alias.rs:14` and `:350` | Canonical representative is `min(members)` | Documented there as a display convenience that is not part of the contract |
| Compaction `writer.rs:744–790` | Sorts by time and promoted service columns | Never `template_id` |
| Miner `tree.rs:281` | Convergence ties go to the lowest id | Within one tenant tree, which only one allocator ever mints into |
| Miner `persist.rs:156`, `:418` | Snapshots sort records by id, and restore re-inserts leaves in id order | Within one tenant tree, which only one allocator ever mints into |
| C2 gate counter `ourios-bench/src/c2.rs:156` | Counts a template as created when its id exceeds the running maximum | Correct per allocator: the bench drives one in-process miner |
| DSL comparisons on `template_id` (`plan/predicate.rs:354`) | Answer correctly | "Newer templates have higher ids" holds only within one allocator; the query cookbook will say so when the RFC reaches `green` |

The miner rows follow from the per-allocator guarantee: within one
allocator, ids increase with creation order across restarts, which is
all the restore-order equivalence of RFC 0001's 2026-09-30 amendment
needs.

### 3.8 Amendments to earlier RFCs

- **RFC 0001 §6.1** (*Template identity*): uniqueness is a durable
  guarantee (§3.4), and monotonicity is per allocator (§3.7).
- **RFC 0001 §6.9**: every start reads the high-water and allocates
  above it (§3.4), and the first start bootstraps it (§3.5).
- **RFC 0001 scenario §3.5.3**: restore equivalence holds up to the
  injective renaming of tail-minted ids (§3.4), and RFC 0001 §8's
  restore-equivalence test asserts exactly that.
- **RFC 0023 §3.4**: `ourios.miner.parse_failure.reason` gains the
  member `id_reservation_failed`, checked after `template_ceiling`
  (§3.3).
- **RFC 0001** gets only a dated pointer here, in §6.1 and §6.9.

### 3.9 Telemetry

All names go through the shared semconv registry (ourios-semconv). They
are listed exactly so that registry PR can be finalised:

| Name | Kind | Attributes / members |
|---|---|---|
| `ourios.receiver.template_ids.bootstrapped` | event, once per store | `ourios.receiver.template_ids.floor` (int, required); `ourios.receiver.template_ids.data_max` (int, conditionally required when any data file carries an id); `ourios.receiver.template_ids.audit_max` (int, conditionally required when any audit file carries an id); `ourios.receiver.template_ids.files_scanned` (int, required) |
| `ourios.receiver.template_ids.bootstrap.progress` | event, every 10,000 files | `ourios.receiver.template_ids.files_scanned` (int, required); the progress-event shape of `ourios.graph.backfill.progress` |
| `ourios.miner.parse_failure.reason` | existing enum attribute | new member `id_reservation_failed` |

Reservation failures in the background refiller log through the
existing `tracing` warn path, with `error.type` set to the store error
class. No new metric is added: a run of failures shows on the
parse-failure counter as soon as it costs a template.

### 3.10 What this RFC does not change

- The snapshot payload (format 3, including `wildcard_routed`).
- The WAL format.
- The Parquet and audit schemas.
- The RFC 0052 recovery horizons.

Restore still seats the allocator past the restored ids; the high-water
only adds a floor above that.

## 4. Alternatives considered

**Floor from the audit stream only (#908).** It cannot see structured
templates, dropped audit writes, or a lost root (§2). That makes it
unsound, not just imprecise.

**Bootstrap with a fixed margin instead of the data scan.** Cheaper at
the one-time bootstrap, but the margin is a guess: nothing bounds
structured keys or dropped audit batches above the audit maximum. The
maintainer chose the provable floor (2026-10-05).

**Journal reservations as WAL frames.** This would keep exact id
continuity across restarts, so §3.5.3 would need no renaming. But it is
an RFC 0008 format change, and it still needs the store object for a
lost root. The renaming it avoids is unobservable in the steady state
(§3.4).

**A per-node key with static disjoint ranges.** For example,
`node k` issues ids `≡ k (mod R)`. It needs a stable node identity and a
fixed `R`, and it breaks on scale-out. The compare-and-swap needs
neither.

**Synchronous reservation under the miner lock as a fallback.** It puts
store latency, and store outages, on the ingest path for every line in
the batch. Failing only fresh allocations (§3.3) bounds the damage to
lines that need a new template.

**A content hash as the id.** RFC 0001 §6.1 already rejects this: it
leaks identity across tenants and makes versioning collapse into
aliasing.

## 5. Acceptance criteria

The ids are referenced from test code.

> **Scenario RFC0059.1 — A discarded snapshot never re-issues a published id**
> - **Given** a receiver that minted string and structured templates for
>   two tenants, published them through a barrier cut, and reclaimed
>   their WAL frames
> - **When** one tenant's snapshot is discarded (each of
>   `unknown_version`, `corrupt`, `empty`, `no_horizon`,
>   `restore_failed`) and the receiver restarts and mints new shapes and
>   the old ones
> - **Then** no newly allocated `template_id` equals any `template_id` a
>   data row or audit event in the store already carries
> - **And** no `(template_id, version)` in the audit stream is bound to two
>   template texts
> - **And** an old shape first seen in a reclaimed frame re-mints under a
>   fresh id (hazard #5 drift), never another template's

> **Scenario RFC0059.2 — A replaced local root never re-issues a published id**
> - **Given** RFC0059.1's published store
> - **When** the receiver restarts on an empty WAL root (no snapshots, no
>   frames) over the same store
> - **Then** its first allocation is above the high-water `N`
> - **And** no newly allocated id equals any id the store carries

> **Scenario RFC0059.3 — Ids are reserved before they are used; a crash only skips**
> - **Given** a receiver whose high-water reads `N`
> - **When** it allocates its first id
> - **Then** the object already reads at least that id when the allocation
>   happens
> - **And** after a SIGKILL that follows a reservation but precedes any
>   use of its block, the restarted receiver's first allocation is above
>   that block, and every id below it stays unissued

> **Scenario RFC0059.4 — An exhausted range fails fresh mints without blocking ingest**
> - **Given** a miner whose current and ready blocks are both used up, and
>   a reserver that is down
> - **When** lines arrive, some needing a fresh template and some matching
>   an existing one
> - **Then** each fresh-needing line is emitted with `template_id = 0`,
>   its body retained, and counted on `ourios.miner.parse_failures` with
>   `ourios.miner.parse_failure.reason = id_reservation_failed`
> - **And** each matching line attaches to its existing template as
>   before
> - **And** no store call is made while the miner lock is held
> - **And** once the reserver recovers, the next fresh line is allocated
>   from the new block

> **Scenario RFC0059.5 — An unreadable high-water fails startup closed**
> - **Given** a store whose `miner/template_ids.v1.json` does not parse,
>   lacks `reserved_through`, or holds a non-`u64` value
> - **When** the receiver starts
> - **Then** startup fails with an error naming the object, before any
>   listener opens
> - **And** the object is not rewritten

> **Scenario RFC0059.6 — The bootstrap floor is provable and written once**
> - **Given** a store with published data and audit files but no
>   high-water object, including a structured template that has no audit
>   event and a data row whose audit event was dropped
> - **When** the receiver starts
> - **Then** the object is created with `reserved_through =
>   max(data_max, audit_max, restored_max)`, with no margin
> - **And** every id allocated afterwards is above it
> - **And** `ourios.receiver.template_ids.bootstrapped` is logged exactly
>   once, with the floor and the maxima
> - **And** a restart killed mid-scan leaves no object, and the next start
>   redoes the scan and writes once

> **Scenario RFC0059.7 — The bootstrap reads footers in bounded memory**
> - **Given** histories of 40 and 160 data and audit files with large
>   bodies and templates
> - **When** the bootstrap scan runs
> - **Then** it decodes no row and reads no data page of a file whose
>   `template_id` statistics are present
> - **And** its peak heap is below one eighth of the history's body and
>   template bytes, and grows less than 1.5× when the history grows 4×

> **Scenario RFC0059.8 — Concurrent reservers on one store get disjoint blocks**
> - **Given** a store with `If-Match` support and two reservers on it
> - **When** both reserve blocks concurrently, many times
> - **Then** every block either reserver received is disjoint from every
>   other
> - **And** the high-water reads the highest block end

> **Scenario RFC0059.9 — Restore equivalence holds up to renaming tail-minted ids**
> - **Given** a tenant snapshotted at `S`, frames above `S` that mint
>   new templates, and a high-water above every id issued
> - **When** recovery restores the snapshot and replays the tail
> - **Then** the recovered state equals a from-scratch control under an
>   injective renaming that touches only ids first minted in the tail
> - **And** every restored id and every other field is exactly equal
> - **And** no renamed id equals an id issued before the restart

> **Scenario RFC0059.10 — Ids increase per allocator across restarts**
> - **Given** an allocator that issued ids, restarted, and issued more
> - **When** its ids are listed in issue order
> - **Then** they strictly increase
> - **And** `u64::MAX` is never issued: seating past it, or a block
>   reaching it, is a controlled error

> **Scenario RFC0059.11 — Any later-version high-water fails startup closed, even beside a v1**
> - **Given** a store holding `miner/template_ids.v2.json`, both alone
>   and beside a readable `miner/template_ids.v1.json`
> - **When** a binary that knows only version 1 starts
> - **Then** startup fails with an error naming the later-version key,
>   before any listener opens
> - **And** it neither bootstraps nor writes any high-water object

## 6. Testing strategy

**Integration tests (`ourios-ingester` `tests/it`).** These run on #898's
scenario harness: the production barrier, housekeeping and recovery
path, with the store's data rows and audit events compared against
every newly minted id.
- RFC0059.1, .2, .3 (the SIGKILL arm reuses the RFC 0052 crash
  fixture), .5, .6, .9 and .11.

**Miner unit tests**, with a scripted `IdReserver`:
- RFC0059.4: the reserver records every call; the test asserts none
  happens while the miner holds its lock;
- RFC0059.10.

**Property test (`proptest`).** Over any reachable miner state, any
subset of discarded tenants, surviving replay and new traffic: a miner
seated past the issued ids only hands out an issued id for a template a
kept snapshot restored under that id. This covers RFC0059.1 and .9.

**Concurrency test (RFC0059.8).** Two reservers on an in-memory
`If-Match` store, plus an `#[ignore]`d LocalStack arm run by the
`s3-integration` job.

**Heap test (RFC0059.7).** `dhat` in its own binary, in the style of
#896's `rfc0033_bounded_fold.rs`.

**Red gate.** One `#[ignore]`d `todo!` stub per scenario, registered in
`tests/it/main.rs` (#819's convention), flips this RFC to `red`.

## 7. Open questions

- [ ] **Semconv names.** §3.9's names are final once the shared registry
      PR (ourios-semconv#7) lands with them. The code pins that tag.
- [ ] **Template-map artefacts.** RFC 0033's map is derived from the audit
      stream, so it inherits the audit fold's last-wins on historical
      collisions. Repairing already-collided history is out of scope; the
      DuckDB procedure from #908 detects it.
- [ ] **Scale-out without the upgrade rule.** The §3.5 rule (upgrade at
      one replica) could become a checked precondition: refuse to
      bootstrap while another replica's lease is live. That waits on a
      lease, as RFC 0013 §7 does.

## 8. References

- Issue #898 and PR #908 (the investigation, the scenario tests, the
  DuckDB collision check); #909 (the review this RFC answers).
- RFC 0001 §6.1, §6.2 step 0, §6.9, scenario §3.5.3.
- RFC 0005 §3.6 (data-file statistics) and §3.7.1 (audit fold order).
- RFC 0010 (drift).
- RFC 0013 (`put_if_match` and the manifest compare-and-swap).
- RFC 0019 (RFC0019.7, the local backend).
- RFC 0023 §3.1 and §3.4.
- RFC 0025 (quarantine).
- RFC 0033 (the map artefact precedent).
- RFC 0035 §3.1 (the ordered phase).
- RFC 0047 (erasure).
- RFC 0052 (reclamation and recovery horizons).
- RFC 0056 (audit durability).
- #791 (object-store outage recovery); #853 (listing cost); #896 (the
  bounded audit fold).
- `CLAUDE.md` §3.1, §3.3, §3.6, §3.7; hazards #1 and #5.
