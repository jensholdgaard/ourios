---
rfc: 0036
title: Write-side layout — compacted-partition clustering and row-group sizing
status: red
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-21
supersedes: —
superseded-by: —
---

# RFC 0036 — Write-side layout

> **Status note.** **`red`** (2026-07-21). The five §5 stubs are
> landed `#[ignore]`d, each failing on `todo!()` when force-run:
> RFC0036.1/.3/.4/.5 in
> `crates/ourios-parquet/tests/it/rfc0036_write_side_layout.rs` with
> the compaction machinery they gate, and RFC0036.2's in-repo slice in
> `crates/ourios-querier/tests/it/rfc0036_window_materialization.rs`
> beside the RFC 0016 counter assertions (its comparative arm runs
> through the `ourios-bench` RFC 0031 harness, not a CI stub). Design
> review is done — maintainer go, 2026-07-21. The §7 decisions marked
> "at `red`" (the threshold sweep, the run format, the fan-in cap F,
> the D2 band) are deferred to the green implementation, where they
> are measured rather than guessed.
>
> **How to read this document.** This is the write-side layout lever
> that `docs/benchmarks.md` §9.13 named and §9.24 left "parked on its
> own line" — hazard #4's layout fork, carried from the #498
> scoreboard. The comparative program measured *why* time-window
> browses lose the storage-bytes channel to Loki (§9.13 runs #8–#17):
> a compacted hour is one file whose row groups rotate only at
> 128 MiB uncompressed and declare no sort, so a one-service window
> query materializes essentially the whole hour. This RFC clusters
> rows at **compaction time** by (promoted `service.name`,
> `time_unix_nano`), rotates compacted row groups at a smaller
> threshold, and declares Parquet `sorting_columns` — ingest is
> untouched, no Parquet schema change, store-build determinism
> preserved. **The framing is deliberately honest:** the L6
> storage-bytes channel stays a published diagnostic (RFC 0031 §5
> declined to gate it, and the ~188 KB layout-independent
> template-registry acquisition is its floor — §2.2). The goal here is
> to collapse the *materialization* term — a window query over one
> service in one hour should fetch a few small row groups, not the
> whole hour — not to beat a floor that layout cannot touch.

## 1. Summary

Post-compaction, a (tenant, hour) partition is **one file**
(`compaction.rs` outputs a single consolidated file per partition)
whose row groups rotate only when the writer's uncompressed
in-progress buffer crosses 128 MiB (`writer.rs:60`), whose rows sit
in ingest/append order, and which declares no `sorting_columns`
(none exist anywhere in the codebase). On the v8 comparative corpus
that yields roughly **one row group per hour holding all services**
(§9.13 run #17), so neither the time min/max statistics nor the
RFC 0022 promoted `service.name` bloom can skip anything — the
measured mechanism of the L6 window-browse loss. This RFC changes
the **compacted** layout only: (1) compaction sorts the partition's
rows by (promoted `service.name`, `time_unix_nano`) via a
bounded-memory sort-run merge that preserves compaction's existing
one-input-file peak-memory property; (2) compacted row groups rotate
at a smaller uncompressed threshold (proposal ~32 MiB, tunable),
giving time/service pruning its granularity back; (3) the writer
declares Parquet `sorting_columns` so order-aware readers can
exploit the layout. No schema change, no migration, no ingest-path
change, and byte-identical store rebuilds are preserved. The
explicit non-goal: winning the L6 storage-bytes channel, whose floor
is the layout-independent registry acquisition (§2.2).

## 2. Motivation

### 2.1 The measured mechanism

`docs/benchmarks.md` §9.13 published the L6 window-browse loss
honestly and diagnosed it precisely. On a browse-k-rows query Loki
reads only the tiny chunk slice its label stream + time index point
at (16,250 B storage-side at k=100), while Ourios pays fixed
per-query costs that dwarf a k-row answer: 1,931,911 B at k=100 on
the authoritative run (§9.24). Run #17's diagnostic sharpened the
*why*: scoping the same window to the **lowest**-volume service
("ad") improved Ourios only ~22% — **no bloom collapse** — because
v8's hour partitions each hold roughly one row group containing
*all* services, so the promoted `service.name` bloom has nothing to
skip and the time min/max spans the whole hour. §9.13's closing
assessment named the fix: "the tier-changing lever is write-side
layout (service clustering / row-group sizing — hazard #4
territory, an RFC-level change), not query-side tuning." §9.24
repeated it: the write-side lever "stays parked on its own line."
This RFC is that line.

The layout facts, verified in code:

- Ingest seals a partition buffer at a 256 MiB estimate target, a
  1 GiB total ceiling, or 300 s of age
  (`ourios-server/src/receiver.rs:55–57`; these are RFC 0014 §3
  defaults, not yet RFC 0004 knobs — RFC 0014 §7).
- The writer flushes a row group when `ArrowWriter::in_progress_size`
  — an **uncompressed** estimate — crosses 128 MiB
  (`writer.rs:60`, checked at `writer.rs:627` and `writer.rs:644`).
  There is
  no `max_row_group_size` property and no `sorting_columns`
  declaration anywhere; rows are in append order.
- `PartitionKey` is (tenant, year, month, day, hour) only
  (`partition.rs:29–43`) — no service dimension in the path.
- Compaction (`compaction.rs:184–308`) streams inputs **one file at
  a time** (peak decoded memory = one input file — a deliberate,
  commented property, `compaction.rs:228–234`), appends them in
  input order into **one** output file per partition, and rotates
  row groups at the same 128 MiB threshold. §9.7's band-scale D3
  output was a single 456.7 MiB file.

### 2.2 What layout can fix, and the floor it cannot

Two distinct terms make up an Ourios window query's bytes:

1. **The registry acquisition.** Every body-rendering query pays the
   RFC 0033 template-map acquisition — the warm v2 artifact is
   **187,904 B** (RFC 0033 §9; 187,906 B on the §9.24 authoritative
   run). This term is **layout-independent**: it is paid before any
   row group is touched, and for k=100 it *alone* exceeds Loki's
   entire 16,250 B read by ~11×. No write-side layout changes it.
2. **The materialization term.** Everything else — the column chunks
   of the row groups that survive pruning. At k=100 on §9.24 this is
   roughly 1,931,911 − 187,906 ≈ 1.74 MB: the whole hour, because
   one all-services row group survives pruning by construction.

This RFC targets term 2 only, and says so plainly: **layout cannot
beat the ~188 KB registry floor and does not try.** Accordingly the
L6 *storage-bytes* channel stays exactly what RFC 0031 made it — a
published diagnostic, not a gate (RFC 0031 §5 declined to gate it;
the §7 frozen L6 gate is the latency floor, which already passes:
0.370 / 4.341 on §9.24). Illustrative arithmetic, not a promise:
with the hour clustered by (service, time) and rotated at ~32 MiB, a
one-service k=100 window's answer lives in one or two small row
groups; the materialization term drops from ~1.74 MB toward the size
of those chunks' matched columns, leaving total bytes dominated by
the registry constant. The §5 criteria gate the *mechanism* (row
groups fetched), and the before/after bytes are measured and
published in the §9 series as the diagnostic they are.

### 2.3 Why compaction is the layer

The ingest path just got its concurrency model rebuilt (RFC 0035:
streamed append, order-insensitive Parquet encode on a bounded
pool, an encode-drain-and-flush barrier keyed to WAL rotation).
Sorting at ingest would sit directly on that fresh machinery and on
the ack path (§4). Compaction, by contrast, already rewrites every
row of the partition — it is the RFC 0022 §3.4 re-projection point,
where history converges toward the *current* promoted attribute
set — so clustering rides a pass that exists, off the hot path,
with its cost bounded by the compaction cadence. Hazard #4's
mitigation already owns this layer ("background compaction job per
tenant; cadence is a tunable"); this RFC extends what that job
does, not where work happens.

## 3. Proposed design

### 3.1 The clustering key

Compacted rows are ordered by, in precedence:

1. **Promoted `service.name` value**, lexicographic byte order of
   the UTF-8 string, **absent/null first**. Lexicographic, *not*
   first-seen or dictionary-ordinal order: first-seen order depends
   on ingest interleaving and would break rebuild determinism
   (§3.5). Tenants whose promoted set does not include
   `service.name` fall back to key 2 alone — a time-only sort still
   buys row-group time-pruning granularity (§7).
2. **`time_unix_nano`**, ascending.
3. **Deterministic tie-break** — (input-file ordinal in
   sorted-basename order, row ordinal within that input). Never
   exposed as a declared sort; it exists so equal-key rows have one
   canonical order and the output is byte-identical across runs and
   listing orders (§3.5). Compaction already sorts input basenames
   for its audit event (`compaction.rs:267–268`); the tie-break
   reuses that order.

### 3.2 Bounded-memory sort: run formation + k-way merge

A plain k-way merge of the input files cannot produce this order:
the key leads with `service.name`, which append order does not
cluster at all, and RFC 0035 explicitly disclaims intra-partition
row order on ingest-side files (RFC0035.5 — concurrent encode may
reorder rows; "near-time-ordered" is an observation, not an
invariant). The design is therefore a textbook external merge sort
whose initial runs are the input files themselves:

1. **Run formation.** For each input, one at a time: decode all
   rows (exactly today's per-input `read_all` — peak decoded memory
   = one input file, the existing bound), sort them by the §3.1 key
   (a stable sort; the tie-break's row ordinal is the pre-sort
   position), and spill a **sorted run** to local scratch (local
   disk is cache, not truth — `CLAUDE.md` §3.6 clean; the run
   format, Arrow IPC vs Parquet, is §7).
2. **Merge.** Stream a k-way merge over the sorted runs, holding
   **one decoded batch per run** (the `Reader` already wraps the
   streaming `ParquetRecordBatchReader`; a batched-read entry point
   alongside `read_all` is the only reader addition). Emit into the
   existing `Writer`, rotating row groups at the §3.3 threshold.

**Why the one-input-file memory property is preserved (the
load-bearing claim).** Phase 1's peak is one fully-decoded input —
*identical* to today's bound (`compaction.rs:228–234`), since it
processes inputs strictly one at a time. Phase 2's peak is
N × one-batch, where N is the input count — bounded in practice by
the ingest seal policy (a partition accrues files at the 256 MiB
target / 300 s age cadence; §9.7's band-scale case was 32) and
bounded *unconditionally* by a **fan-in cap F**: if N > F, merge
hierarchically (F runs → one intermediate run, repeat), so phase-2
memory never exceeds F × batch_bytes regardless of backlog. With
batch sizes in the low-thousands of rows, F × batch is far below
one decoded input file. The writer's in-memory output accumulation
(`ArrowWriter<Vec<u8>>`, `writer.rs:105`) is unchanged in both
phases. Everything around the sort — manifest bootstrap, CAS
commit, GC, the RFC0009.5 per-row partition validation at input
open — is untouched.

### 3.3 Compacted row-group threshold

Compacted output rotates row groups at a **separate, smaller
uncompressed threshold** — proposal **32 MiB**, a new
`COMPACTED_ROW_GROUP_FLUSH_BYTES` alongside `ROW_GROUP_FLUSH_BYTES`
(which ingest-side files keep at 128 MiB). Illustration at §9.7's
D3 scale: the 456.7 MiB single-file hour becomes on the order of
**~14 row groups** instead of a handful (rotation is on
uncompressed bytes, so the exact count depends on the corpus's
compression ratio); on v8's ~one-row-group hours, several. Combined
with the §3.1 sort, each row group's `service.name` min/max spans
one service (or a boundary pair) and its time min/max is tight
*within* that service — so plain footer statistics prune, without
even needing the bloom.

**This amends hazard H4's row-group band.** `docs/hazards.md` H4
targets "row-group size 128 MB – 1 GB"; this RFC deliberately drops
*compacted* row groups below that band. The band's purpose is file
economics — LIST calls, footer reads, cold-cache hits — and those
are governed by the **file** band (256 MiB – 2 GiB), which is
untouched: compaction still emits one file per partition, D3 still
measures files. Within one file, a smaller row group costs a few
more footer metadata entries (~14 vs ~4 at D3 scale) and buys
pruning granularity — the trade this RFC exists to make. On
acceptance, H4's mitigation bullet is reworded to scope the
128 MB – 1 GB row-group target to ingest-side files and state the
compacted threshold as the pruning-granularity knob (a one-line
`docs/hazards.md` edit shipped with the implementation; H4.4's
detection signals are file-based and unaffected).

### 3.4 Declared `sorting_columns`

The compaction writer declares Parquet `sorting_columns` — the
§3.1 keys 1 and 2 (or key 2 alone for time-only tenants) — via
`WriterProperties`. Two honest clarifications: (a) the *pruning*
win of §3.3 comes from the physical clustering making per-row-group
statistics tight, not from this metadata — statistics prune whether
or not a sort is declared; (b) the declaration is what lets
order-aware execution (DataFusion sort-elision, merge scans, future
DSL `ORDER BY`/limit pushdown) trust the layout without a defensive
re-sort. It is pure footer metadata: **no Parquet schema change**,
old files without it read exactly as before, readers that ignore it
are unaffected — `CLAUDE.md` §3.5 is satisfied with no migration
(RFC0036.5 pins this). Ingest-side files declare nothing in this
RFC (their rows are genuinely unsorted post-RFC 0035; declaring a
sort they don't have would be a lie — a seal-time sort that would
make a time-only declaration true is §7).

### 3.5 Determinism

The comparative harness depends on byte-identical store rebuilds
(§9.13's determinism note: "for repeated measurements of the same
build and configuration, Ourios's bytes are byte-identical" — that
property is what lets the run series read as an optimisation
ledger). The §3.1 key is a **total order** over the partition's
rows: lexicographic service value + timestamp + the
(sorted-basename input ordinal, row ordinal) tie-break leave no two
rows unordered, so the merged **row sequence** is a pure function
of the input files' contents and names. Row order alone does not
imply byte identity, though — page and row-group boundaries,
dictionary state, and footer contents must also be deterministic.
They are, by the same writer-level invariants today's §9.13
property already rests on: fixed sub-batching (`SUB_BATCH_ROWS`),
a fixed row-group threshold evaluated on the same deterministic
`in_progress_size` accounting, fixed writer properties (codec
level, dictionary/statistics/bloom settings), and no
time-or-randomness-dependent metadata. Deterministic rows fed
through a deterministic serializer yield deterministic bytes —
the identical argument that makes today's unsorted builds
byte-identical, with the sort adding only a deterministic
permutation and (for spilled runs) deterministic run boundaries
from fixed spill thresholds. RFC0036.4 pins the end-to-end claim
with a byte-identity rebuild test, which subsumes the row-order
property.

### 3.6 What stays exactly as-is

The ingest path in full — streamed append, the RFC 0035 encode pool
and its drain-and-flush barrier, WAL-before-ack, the flush policy
constants. The Parquet schema and every column's encoding. The
partition path scheme (no service dimension — §4 rejects it). The
compaction manifest protocol (bootstrap, CAS, GC, orphan sweep) and
the RFC 0022 re-projection semantics — clustering rides the same
rewrite pass and re-projects under the same current promoted set.
The query DSL and querier surface. The L6 gate disposition: the
latency floor stays the frozen gate, the storage channel stays a
published diagnostic.

## 4. Alternatives considered

- **Service sub-partitioning (tenant × time × service paths).**
  Adding `service.name` to `PartitionKey` would make pruning
  trivial — and reintroduce exactly the hazard this RFC lives
  under: per-service files multiply file counts by service
  cardinality, and low-volume services (v8's "ad" at ~34 s of
  activity per window) produce precisely the small-file/LIST
  blowup H4 exists to prevent. It is also a physical **path
  layout change** — every existing partition would need
  rewriting or dual-path read logic, a migration burden §3.5
  reserves for schema-level necessity. Row-group-level clustering
  buys most of the pruning at zero path/migration cost. Rejected.
- **Ingest-time sorting.** Sort rows before the ingest-side writer
  instead of at compaction. This breaks the streamed-append model:
  a sort needs the partition's rows resident and re-orderable, so
  buffers pin their contents until seal, inflating residency
  against the 1 GiB `SINK_CEILING_BYTES` and adding latency-shaped
  work to the path RFC 0035 just relieved — and it tangles
  directly with the fresh encode pool, whose correctness argument
  (order-insensitive emit) was accepted weeks ago. Ingest-side
  files are short-lived (compaction consumes them); sorting them
  buys granularity only until the compactor runs. Rejected in
  favour of sorting where rows are already rewritten.
- **Gating the L6 storage channel.** Set a storage-bytes floor and
  drive layout work against it. RFC 0031 §5 already considered and
  declined this — and §2.2 shows why it is unwinnable as a gate:
  the registry acquisition alone exceeds Loki's entire k=100 read,
  independent of layout. Gating it would either force dishonest
  accounting (exclude the registry) or freeze a guaranteed FAIL.
  The channel stays a published diagnostic; the mechanism gets the
  gate (RFC0036.2). Rejected.
- **Do nothing.** The frozen gates all pass (§9.24) — no gate
  forces this work. But §9.13 and §9.24 both name write-side
  layout as *the* remaining storage-side lever, window
  materialization is the honest weak spot the comparative program
  documented, and hazard #4 explicitly escalates layout tuning to
  an RFC. Leaving the named lever unpulled leaves every window
  query paying whole-hour materialization for a k-row answer.
  Rejected.

## 5. Acceptance criteria

> **Scenario RFC0036.1 — compacted layout (clustering + sizing +
> declaration).**
> - **Given** a partition holding ≥ 2 input files whose rows span
>   multiple promoted `service.name` values and interleaved times
> - **When** the partition is compacted
> - **Then** footer inspection of the consolidated file shows: row
>   groups rotated at the configured compacted threshold (each
>   uncompressed size ≤ threshold + one sub-batch's bounded
>   overshoot), `sorting_columns` declared as §3.1 keys 1–2 on
>   every row group, and per-row-group `service.name` min/max
>   spanning at most a boundary pair of services
> - **And** decoding the file yields rows in §3.1 key order, with
>   the row multiset equal to the inputs' union.

> **Scenario RFC0036.2 — window-query materialization (the
> point).**
> - **Given** a compacted store built from a §9-style corpus (the
>   v8 shape: one hour, many services, promoted `service.name`)
> - **When** the L6-shape query (one service, k-row time window)
>   runs
> - **Then** the row groups scanned (the RFC 0016 scanned/pruned
>   counts) are ≤ ceil(B_sw / T) + 2, where **B_sw** is the queried
>   service's bytes within the window (measurable from the compacted
>   file's footer: the sorted layout places one service's window in
>   contiguous row groups) and **T** is the configured row-group
>   threshold — i.e. the groups that *hold the answer* plus at most
>   two boundary groups, not the whole hour
> - **And** the before/after materialization bytes (total minus
>   the registry acquisition) are measured on the same corpus and
>   published in the §9 series as the storage-channel diagnostic —
>   expected to fall by roughly the row-group-count ratio; the
>   *gate* here is the scanned-row-group bound, not a bytes ratio
>   (§2.2 — the registry floor makes bytes a diagnostic).

> **Scenario RFC0036.3 — compaction properties preserved (D2 / D3
> / memory).**
> - **Given** the §9.7-scale compaction workload (band-scale
>   partition, tens of input files)
> - **When** the sorted compaction runs
> - **Then** D3 holds unchanged (one output file per partition,
>   inside the 256 MiB – 2 GiB band, < 5% of live files below
>   128 MiB) and D2 compaction throughput stays within an agreed
>   band of the §9.7 measure (sorting is not free; the band is set
>   at `red` from a first measurement, and "keeps up" — throughput
>   ≫ per-partition seal rate — must still hold)
> - **And** a memory-bound test shows peak decoded-row residency of
>   the order of one input file (phase 1) and F × batch (phase 2)
>   — compacting an N-file partition must not regress to
>   whole-partition residency.

> **Scenario RFC0036.4 — determinism (the harness's contract).**
> - **Given** the same set of input files (same bytes, same names)
> - **When** the partition is compacted twice — including with the
>   store returning listings in different orders
> - **Then** the two consolidated outputs are **byte-identical**,
>   preserving the §9.13 determinism property the comparative
>   ledger depends on.

> **Scenario RFC0036.5 — no read-path or schema regression.**
> - **Given** stores built before and after this change, and old
>   (pre-RFC) compacted files read by the new reader
> - **When** B1/B2 and the frozen RFC 0031 comparative gates run
>   against the post-change store
> - **Then** every frozen gate still passes with the L1/L3/L4
>   pairs not degraded beyond the documented Loki-wobble band
>   (sorted, smaller row groups should help or be neutral —
>   measured, not assumed), query results are identical row-sets,
>   and old files (no `sorting_columns`, 128 MiB row groups) read
>   without error or special-casing — no migration exists because
>   none is needed (`CLAUDE.md` §3.5).

## 6. Testing strategy

Mapped to `CLAUDE.md` §6.2; techniques per §5 scenario id:

- **RFC0036.1 — footer-inspection unit tests** in `ourios-parquet`:
  compact a synthetic multi-service partition, then assert via
  `ParquetMetaData` the row-group sizes, `sorting_columns`, and
  per-group `service.name`/time statistics; decode and assert §3.1
  order. Plus a **property test (`proptest`)** for the merge
  itself: arbitrary input files with arbitrary
  service/time/duplicate-key mixes ⇒ output multiset equals input
  union, output is §3.1-sorted, and **equal-key rows land in
  tie-break order** (the stability clause RFC0036.4 leans on).
- **RFC0036.2 — the comparative dispatch + querier counters.** The
  L6-shape pair on the v8 corpus through the RFC 0031 harness;
  assert the scanned/pruned row-group counts (RFC 0016 emits them
  raw) against the ceil-bound; record before/after bytes in the §9
  series. A smaller in-repo integration test pins the
  scanned-count bound on a synthetic hour so CI catches
  granularity regressions without the full harness.
- **RFC0036.3 — `criterion` compaction bench + a memory test.** The
  existing `compaction` bench group re-run with sorting to set and
  then hold the D2 band; D3 assertions unchanged
  (`rfc0009_1_*`-style structural tests extended). Memory:
  compact an N-file partition under an allocation-tracking
  harness (or peak-RSS measurement in the bench) and assert the
  phase-1/phase-2 bounds — the test fails if the merge ever holds
  the whole partition decoded.
- **RFC0036.4 — a rebuild differential.** Compact the same inputs
  twice — second run with a shuffled listing order (store fake) —
  and assert byte equality of the outputs (a file hash **is**
  correct here, unlike RFC0035.5's decoded-row equality: byte
  identity is exactly the property claimed).
- **RFC0036.5 — existing suites + the comparative gates.** B1/B2
  and the frozen-gate dispatch on a post-change store; the
  RFC 0005 reader forward/backward tests extended with a
  pre-RFC-0036 fixture file (no `sorting_columns`) to pin
  no-migration reads.

## 7. Open questions

- [ ] **The compacted row-group threshold.** 32 MiB is a proposal
  sized off the D3-scale ~14-row-group structure; the honest
  number comes from a sweep (e.g. 16/32/64 MiB) against the
  L6-shape scanned-bytes curve *and* L1/L3 neutrality (more row
  groups = more footer entries and more per-group index overhead).
  Decide at `red` from measurement, keep it tunable (an RFC 0004
  knob eventually, like the flush-policy constants).
- [ ] **Sort-key stability definition — settled as lexicographic,
  confirm no consumer assumes otherwise.** The §3.1 key is
  lexicographic `service.name`, *not* first-seen/dictionary
  ordinal (first-seen is interleaving-dependent and would break
  RFC0036.4). Confirm nothing downstream (dictionary encoding
  efficiency, RFC 0022 bloom sizing) prefers first-seen order.
- [ ] **Unpromoted-attribute tenants.** No promoted `service.name`
  ⇒ time-only sort (§3.1). Still wins time-pruning granularity;
  confirm the `sorting_columns` declaration degrades to the
  single time key cleanly and RFC0036.1's assertions have a
  time-only variant.
- [ ] **Ingest-side time-only `sorting_columns`.** Ingest files are
  near-time-ordered but not sorted (RFC0035.5), so declaring
  order today would be false. A seal-time sort of the in-memory
  buffer by `time_unix_nano` is cheap (the buffer is already
  resident, ≤ the 256 MiB target) and would make a time-only
  declaration true — a separate small win for queries that hit
  not-yet-compacted files. Possibly in-scope at `red` if it
  falls out of the writer work; otherwise its own follow-up.
- [ ] **Interaction with RFC 0022 re-projection.** Clustering rides
  the same compaction pass that re-projects promoted columns
  (§3.6). Confirm a promoted-set *change* between builds is
  correctly out of scope for RFC0036.4 (determinism is claimed
  for same-configuration rebuilds — §9.13's phrasing — not
  across config changes), and that sorting keys read the
  *current* promoted set, matching re-projection.
- [ ] **Run format and fan-in cap F.** Sorted runs to local
  scratch as Arrow IPC (cheap encode) vs Parquet (reuses the
  reader); pick at `red`. F sizing (memory vs merge passes) and
  whether small partitions skip spilling entirely (sort fully
  in-memory when the partition fits the existing one-input
  bound anyway).
- [ ] **The H4 wording amendment** (§3.3): land the one-line
  `docs/hazards.md` H4 edit (row-group band scoped to ingest-side
  files; compacted threshold = pruning-granularity knob) with the
  implementation PR, per the hazard's own "layout tuning → RFC"
  escalation clause.

## 8. References

- `docs/benchmarks.md` **§9.13** — the L6 window-browse loss table
  (runs #8–#17), run #17's no-bloom-collapse diagnostic, the
  "write-side layout" lever naming, and the determinism note this
  RFC's RFC0036.4 preserves; **§9.24** — the authoritative run
  (k=100 = 1,931,911 B; latency floors 0.370/4.341 pass; the lever
  "parked on its own line"); **§9.7** — D2/D3 at band scale (the
  456.7 MiB consolidated file, 166.8 MiB/s).
- **RFC 0031** (comparative program) — §5's L6 disposition (latency
  floor gated, storage-bytes published-not-gated) and the frozen-
  gate set RFC0036.5 must keep green; the #498 scoreboard line this
  RFC discharges.
- **RFC 0033** (cached template map) — the 187,904 B warm
  acquisition: the layout-independent floor §2.2 is built on.
- **RFC 0009** (compaction) — the manifest/CAS machinery §3.6
  leaves untouched; RFC0009.5 input validation; the D2/D3 measures
  RFC0036.3 re-asserts. **RFC 0022** — promoted attribute columns
  and the §3.4 re-projection pass clustering rides. **RFC 0014** —
  the flush-policy defaults (`ourios-server/src/receiver.rs:55–57`)
  and their §7 knob deferral. **RFC 0035** — the ingest concurrency model §3.6
  keeps untouched, and RFC0035.5's intra-partition row-order
  disclaimer that forces §3.2's run-formation phase. **RFC 0005**
  §3.5 — the row-group and file bands this RFC re-scopes for
  compacted output. **RFC 0016** — the scanned/pruned counts
  RFC0036.2 asserts against.
- Code (paths under `crates/ourios-parquet/src/` unless noted):
  `writer.rs:60`, `writer.rs:627`, `writer.rs:644` (the
  128 MiB uncompressed rotation; no `sorting_columns`, no
  `max_row_group_size` anywhere),
  `crates/ourios-server/src/receiver.rs:55–57` (seal policy),
  `partition.rs:29–43`
  (`PartitionKey` — no service dimension),
  `compaction.rs:184–308` (one-file-at-a-time streaming, the
  §3.2-preserved memory property at `compaction.rs:228–234`, sorted
  basenames at `compaction.rs:267–268`), `reader.rs:57` (the streaming
  `ParquetRecordBatchReader` §3.2's merge builds on).
- `docs/hazards.md` **H4** — the small-file problem: the file band
  (unchanged), the row-group band (amended, §3.3), and the
  "sustained … → RFC" escalation this RFC answers. `CLAUDE.md`
  §3.5 (no schema change — §3.4 here), §3.6 (local scratch runs
  are cache, not truth), §2 pillar #1 (pruning via footer reads —
  the property this RFC restores granularity to).
