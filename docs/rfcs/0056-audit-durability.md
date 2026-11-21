---
rfc: 0056
title: Audit-sink durability on permanent write failure
status: red
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-09-15
supersedes: —
superseded-by: —
---

# RFC 0056 — Audit-sink durability on permanent write failure

> **Status note.** `red`. Every §5 scenario has an `#[ignore]`d `todo!`
> stub in `ourios-server` (`tests/it/rfc0056_held_tenant.rs`) naming the
> slice that discharges it. Specified 2026-11-20 (maintainer-approved,
> with the §7 answers). Tracks #1294. 2026-11-20: the maintainer
> chose this RFC's per-tenant hold over the alternative of publishing and
> holding the node's checkpoint (§4.1). §3 below is the design; §9 keeps
> the RFC 0053 draft's wording it grew from, and §3.7 says where it
> departs. This RFC **amends accepted RFC 0005 §3.7** (no audit event is
> lost) by naming what a permanent audit-write failure does. It
> coordinates with RFC 0052 (`green`), whose `CHECKPOINT` sidecar and
> recovery routing it extends. It covers the **local** WAL backend; the
> replicated backend (RFC 0065, `red`) needs its own protocol and is
> tracked by #1392 (§3.6). **Blocked from `validated` by [#809](https://github.com/jensholdgaard/ourios/issues/809)**,
> RFC 0026's denial-audit durability, which §7 explains this RFC cannot
> close.

## 1. Summary

When the audit sink cannot write a tenant's template events for good,
that tenant's records stop publishing instead of publishing without
their events. The tenant is **held**: its records stay unpublished,
kept only in the WAL, its new appends are refused with a retryable error, and
every other tenant carries on. The node's checkpoint keeps advancing and
records, per held tenant, the offset its records are durable through, so
a restart re-mines that tenant's frames and publishes its records under
events that exist. A restart after the fault is repaired is the only
thing that clears a hold.

## 2. Motivation

`AGENTS.md` §3.1 requires an audit event on every template change, and
RFC 0005 §3.7 says no audit event is lost. Today
`SharedParquetAuditSink::write_retaining` returns only **transient**
failures. A permanent write error (`route_partition_result`) drops the
event, logs `ourios.receiver.audit_sink.dropped` and reports success; a
failed partition derive (`derive_audit_partition`: a pre-epoch or
overflowing timestamp) drops it with nothing but the sink's
derive-error count and reports success too, so `PublishCoordinator::write` publishes the
records that depend on it (#1294). The result is a published row whose
template history the audit stream does not hold.

Every permanent error the sink can return today is deterministic:
encoding (`Parquet`, `Batch`), a partition mismatch, a poisoned lock, or
the derive failure. Retrying the same event fails the same way, so the
choice is between publishing without the event and holding the records.

## 3. Proposed design

### 3.1 A third audit outcome, per tenant

`write_retaining` reports, besides the events it retains for retry, the
**set of tenants** whose events it dropped for good, from either path:
a permanent write error on one of the tenant's audit partitions, or a
derive failure on one of its events. Audit partitions are keyed by
tenant, so the set is exact.

### 3.2 A held tenant's records do not publish

§3.1–§3.5 apply to the **local** backend; §3.6 states the replicated
backend's behaviour.

The hold lives in the **shared audit sink**, so every path that writes
audit events marks it, and every path that publishes records reads it.
A tenant becomes **held** the moment any audit write drops one of its
events for good, whoever triggered that write: the coordinator's drain,
or a record sink's inline size or ceiling flush, which writes the audit
sink first through its barrier (`audit_sink.rs`).

Every record publication skips a held tenant: `PublishCoordinator::write`
publishes the partitions of every tenant that is not held, and an inline
size or ceiling flush publishes nothing of a held tenant's.

**A held tenant's records are not kept in memory.** The record sink
drops its buffered partitions when it is held, and drops every record
for it emitted afterwards, during ingest and during recovery alike, so
they never count against the size or ceiling budget and never reach a
drain. Nothing is lost: only a restart clears a hold (§3.5), and that
restart replays every one of the tenant's frames above its floor from
the WAL, which keeps them (§3.4). Keeping them in memory as well would
buy nothing, and could not be bounded: a restart into an unrepaired
fault replays the whole tail above the floor, and several tenants can be
held at once.

**Its audit events go too.** The audit sink drops a held tenant's
retained events and every event emitted for it afterwards, and releases
the publication ticket they held (`audit_sink.rs`'s ledger), so a held
tenant's events never block another tenant's records, whether they were
retained after a transient failure or are still in the sink. A restart
regenerates them with the records (§3.5). Another tenant's transient
failure still holds that drain's records, as today.

**Recovery publishes in bounded passes.** Recovery runs the
coordinator's write whenever **either** sink reaches its bound while it
replays, the record sink its ceiling or the audit sink its buffer bound,
instead of only after the replay ends. The audit side matters on its
own: replay at or below a tenant's floor suppresses records but can
still emit the events of templates it re-mints, which would otherwise
grow with no record pass to flush them. The pass runs on recovery's own
thread, not on an encode worker. If a pass leaves a sink at its bound,
because a transient failure retained its drain, recovery **stops
reading frames** and retries the pass with backoff until one frees
capacity, as the receiver already waits out a store outage; it never
reads past a full sink.

The passes bound **buffering**, not how soon a fault is found. Replay
can publish several passes of a tenant's records whose template events
write successfully before it reaches the event whose write fails; those
records publish under events that exist, which is all §3.1 asks, and
the tenant is held from that pass on. Its floor stays where it was, so
the next restart publishes those records again (§3.4's known cost).

### 3.3 Its appends are refused

The receiver refuses a held tenant's new batches **before the WAL
append**, with the retryable class RFC 0018 §3 already maps to gRPC
`UNAVAILABLE` and HTTP `503`, naming the tenant. This keeps the held
tenant's WAL tail, which a restart must replay, from growing, and a
client that retries lands after the restart that clears the hold. Batches acknowledged before the hold are in the
WAL and lose nothing.

### 3.4 The checkpoint records where each held tenant stands

A cut (RFC 0052 §3.1) still stamps the node's checkpoint `X` when every
**other** tenant is durable through it. The `CHECKPOINT` sidecar gains a
version 3 that carries, beside `X`, a **held floor** `H_T` for each held
tenant `T`: the offset through which every one of `T`'s records and
their template events are in the store when `T` is held. That is the
later of the last checkpoint stamped and `T`'s installed snapshot
horizon `S_T`. A cut installs snapshots before it stamps (RFC 0052
§3.1), and only after its records and events are written, so a cut that
installed and then failed to stamp leaves `S_T` ahead of the checkpoint
with `T` durable through `S_T` (RFC0052.10 covers `S_T > X`). When `T`
was held before any checkpoint was stamped or snapshot installed, its
floor is **the start of the log**, encoded as an absent offset: none of
`T`'s records count as durable, so all of its frames replay.

**A floor never rises.** The formula above applies only to a tenant
with no floor in the sidecar. Recovery seeds each held floor from the
sidecar before it replays, and a tenant that is held again, as when a
restart meets the same unrepaired fault, keeps that floor: recomputing
it from a checkpoint stamped after the first hold would lift it over
records that were never published, and the next restart would suppress
them. A held tenant keeps its floor across later cuts, and a cut that
holds no tenant writes no floors.

Recovery gates a tenant's frames by `F_T` in place of `X`, where `F_T`
is `H_T` when the sidecar holds a floor for `T` (the start of the log for
an absent one) and `X` otherwise. Both of recovery's gates take it:

- **records** route by `max(S_T, F_T)` instead of `max(S_T, X)`, `S_T`
  being the tenant's restored snapshot horizon. A hold installs no
  snapshot of `T`, but after a repaired restart a cut can install one
  past `H_T` and fail to stamp; a snapshot is installed only once its
  records and events are durable, so `max(S_T, F_T)` then routes `T` by
  `S_T`, correctly, and the stale floor is dropped at the next stamp;
- **audit events** are suppressed through `F_T` instead of `X`, keeping
  the existing exception for a template a frame at or below the gate
  mints afresh (RFC 0059 §3.4).

So `T`'s records above `H_T` publish again on restart **and** the
template events replay regenerates for them are written, while every
other tenant's are suppressed through `X` exactly as today.

The held tenant's miner snapshot is not installed past `H_T` either: a
cut installs no snapshot for a held tenant, so its horizon stays at or
below `H_T`, and replay re-mints the template events those frames
carried, as RFC 0052's regeneration-only replay already does above a
horizon.

**Upgrade and the version witness.** A version-3 reader accepts
versions 1 and 2 as well, as carrying no held floors, and every sidecar
it writes is version 3; there is no dual-write and no tooling,
pre-production (RFC 0052 §3.8). For RFC 0052 §3.2's recovery witness a
version-3 sidecar counts as version 2 does: one beside a missing
`RECLAIM` is a root that lost its record and fails closed, and only a
version-1 sidecar marks a legitimate pre-RFC 0052 root.

Reclamation needs no new rule. RFC 0052 §3.2's eligibility is already
per segment and tenant-aware (`retain.rs`): a closed segment is
reclaimed only when, for every tenant with a frame in it, that frame is
at or below the tenant's snapshot horizon. A hold never installs the
tenant's snapshot past `H_T`, so exactly the segments holding its frames
above `H_T` are kept, and segments holding only other tenants' covered
frames are reclaimed as before. A tenant held before any snapshot has no
horizon, which already pins exactly its own segments.

**Known cost.** Records of `T` that published between `H_T` and the
failure publish a second time after the restart: the sink does not track
a per-tenant publication offset finer than a cut. The window is usually
one barrier interval of one tenant's data, but it is **not bounded by
one**: `H_T` is the last checkpoint that *stamped*, and cuts can fail to
stamp for many intervals in a row (another tenant's transient write
failures, a failed snapshot install) while `T`'s age flushes keep
publishing. The worst case is every record of `T` published since the
last stamped checkpoint. It arises only after a permanent audit fault
and a restart. §7 records the exact alternative.

### 3.5 Restart clears a hold, once the fault is repaired

Nothing in-process clears a hold: the dropped events are no longer in
the sink, so releasing the tenant's records would publish them under
events that were never written. A restart re-mines the frames above
`H_T`, the miner emits the events again, and the first cut that writes
both stamps without a floor for `T`. A restart into an unrepaired fault
holds the tenant again, visibly, and loses nothing: its frames stay in
the WAL under `H_T`.

What repairs a derive failure differs by backend. On the **local**
backend the miner stamps events from its own clock rather than the
frame (§9.3), so a restart on a node whose clock has recovered derives
normally. On the **replicated** backend template events take the
timestamp persisted in the batch (RFC 0065 §6.7, `BatchClock`), so
replay is deterministic and a replica's clock cannot change them; there
`BatchTime` clamps batch timestamps to the audit stream's representable
range (RFC 0065 §6.5), so a derive failure is not expected, and one that
does occur is a bug that needs a code fix before the restart.

### 3.6 The replicated backend: out of scope, #1392

This RFC covers the local backend. RFC 0065's flusher publishes through
the same coordinator and audit sink, so §3.1 and §3.2 would hold its
rows, but the rest does not carry over:

- appends are admitted by the leader, which does not mine, so the hold
  has to reach it, presumably as a log entry every replica applies, with
  rules for leader and flusher changes and for clearing it;
- the flusher's gap replay makes each slice durable before the next, so
  a held tenant's rows would stop the healthy tenants' too;
- a group checkpoint also purges the log, so a per-tenant floor below
  the cut loses the entries a restart needs.

Until #1392 designs that, the replicated backend keeps today's
behaviour: a permanently unwritable event is dropped, logged and
counted, and its rows publish.

### 3.7 Where this departs from the 0053 draft (§9)

- The node checkpoint keeps advancing (§3.4). The draft left the record
  gate node-wide, which would either republish every tenant's records
  or lose the held tenant's; the held floor resolves that.
- The draft named a `ourios.wal.tenant_unrecoverable` signal. No such
  name exists, so §7 lists the telemetry this needs instead.

## 4. Alternatives considered

### 4.1 Publish, and hold the node's checkpoint

Keep publishing every tenant's records, but stop the checkpoint
advancing while an event is lost, so a restart re-mines and re-emits it.
Simpler, with no format change. Rejected: rows are live without their
events until the restart, which is the §3.1 breach this RFC exists to
close, and every tenant pays: the node's WAL grows, and a restart
republishes every tenant's records since the checkpoint as duplicate
objects.

### 4.2 Keep the silent drop

Rejected against RFC 0005 §3.7 and `AGENTS.md` §3.1.

### 4.3 Quarantine the records

RFC 0025 §3.3's quarantine writes a `record_quarantined` event into the
same audit sink that is failing, so it cannot serve here.

### 4.4 Hold every tenant

Stop all publication on any permanent audit failure. One bad partition
would stall the node, so the hold is per tenant.

## 5. Acceptance criteria

> **Scenario RFC0056.1 — a permanent audit write failure holds the tenant `[§3.1]`**
> - **Given** a permanent audit-write failure for tenant A beside a
>   healthy tenant B
> - **When** the coordinator writes a drain holding both
> - **Then** B's records publish and A's do not, A is held, A's buffered
>   records are dropped from memory, and later drains publish B's records
> - **And** the same holds when the failing audit write comes from a
>   record sink's inline size or ceiling flush instead of a drain
> - **And** when A was held with its buffers at the ceiling, the sink's
>   byte total falls by A's buffered bytes, and B's later appends flush
>   by the size and age triggers as they would without A
> - **And** when the same drain also retains one of A's events after a
>   transient failure, A's retained events are dropped with its hold,
>   and B's records publish on the next drain

> **Scenario RFC0056.2 — a derive failure takes the same path `[§3.1]`**
> - **Given** an audit event for tenant A whose `derive_audit_partition`
>   fails (a pre-epoch or overflowing timestamp)
> - **When** the coordinator writes the drain
> - **Then** A is held exactly as in RFC0056.1, and B publishes

> **Scenario RFC0056.3 — a held tenant's appends are refused, retryably**
> - **Given** a held tenant A
> - **When** a client sends A a batch over gRPC and over HTTP
> - **Then** it is refused before the WAL append with `UNAVAILABLE` and
>   `503`, naming A, and B's batches are accepted

> **Scenario RFC0056.4 — a snapshot installed by a cut that did not stamp `[H3]`**
> - **Given** a cut that installed tenant A's snapshot at `S` and failed
>   to stamp, leaving the checkpoint at `X0 < S`, and A then held
> - **When** a later cut stamps `X1`, and the node restarts with the
>   fault repaired
> - **Then** A's floor is `S`, nothing of A's at or below `S` publishes
>   again, and A's records above `S` publish under regenerated events

> **Scenario RFC0056.5 — the checkpoint advances and records the floor `[H3]`**
> - **Given** a held tenant A, held after the checkpoint `X0`, with no
>   snapshot of A installed past `X0`
> - **When** a later cut stamps `X1 > X0`
> - **Then** the `CHECKPOINT` sidecar holds `X1` and A's floor `X0`, A's
>   snapshot is not installed past `X0`, no segment holding A's frames
>   above `X0` is reclaimed, and a closed segment holding only B's
>   frames at or below `X1` is

> **Scenario RFC0056.6 — a restart after repair publishes the held records under their events `[§3.1, H3]`**
> - **Given** RFC0056.5's node, killed, with the audit fault repaired
> - **When** it restarts and recovers
> - **Then** A's records above `X0` publish, after template events that
>   describe them, including events for frames in `(X0, X1]`, B's
>   records and events through `X1` publish nothing again, and the next
>   cut stamps with no floor for A

> **Scenario RFC0056.7 — a tenant held before the first checkpoint `[H3]`**
> - **Given** tenant A held before any checkpoint was stamped and before
>   any snapshot of A was installed, and B healthy
> - **When** a cut stamps `X1`, then the node restarts with the fault
>   repaired
> - **Then** the sidecar holds `X1` and an absent floor for A, no
>   segment holding A's frames is reclaimed, and recovery publishes all
>   of A's records and their template events and none of B's through
>   `X1`

> **Scenario RFC0056.8 — a version-2 root upgrades `[H3]`**
> - **Given** a root whose `CHECKPOINT` is version 2, with `RECLAIM`
> - **When** a version-3 node opens and recovers it, and a cut stamps
> - **Then** recovery routes by the version-2 mark with no floors, and
>   the new sidecar is version 3
> - **And** a version-3 sidecar beside a missing `RECLAIM` fails closed
>   exactly as a version-2 one does

> **Scenario RFC0056.9 — a snapshot past the floor after a repaired restart `[H3]`**
> - **Given** RFC0056.6's node after its repaired restart, where a cut
>   installs A's snapshot at `S > X0` and fails to stamp, leaving the
>   sidecar's floor `X0`
> - **When** the node restarts again
> - **Then** recovery routes A by `S`, nothing of A's at or below `S`
>   publishes again, A's records above `S` publish, and the next stamp
>   writes no floor for A

> **Scenario RFC0056.10 — a restart into an unrepaired fault holds the tenant again `[H3]`**
> - **Given** RFC0056.5's node, restarted with the fault still present
> - **When** it recovers
> - **Then** A is held again from the pass whose audit write fails, its
>   floor is still `X0` although later cuts stamped past it, nothing of
>   A's is lost, and nothing of A's publishes without its template
>   events; records of A whose events wrote before that pass may
>   publish
> - **And** neither sink buffers more than its bound at any point
>   during recovery, including a replay at or below A's floor that
>   emits only re-minted template events, and including tenant B's
>   audit writes failing transiently meanwhile, when recovery stops
>   reading frames until a pass frees capacity

## 6. Testing strategy

- RFC0056.1 and .2: coordinator tests with a fault-injected audit store
  for one tenant, and an event stamped before the epoch for the derive
  path.
- RFC0056.3: receiver tests over both transports.
- RFC0056.4–.7, .9 and .10: integration tests in `ourios-server` on the local
  backend, killing and restarting the node, beside the RFC 0052 barrier
  tests; .4 injects a stamp failure after the install.
- RFC0056.8 and the sidecar's version 3: codec round-trip and version
  tests in `ourios-wal` beside RFC 0052's, and the witness test beside
  RFC0052's missing-record tests.

## 7. Open questions

- [x] **Telemetry names.** Approved 2026-11-20 and registered in
  `ourios-semconv` v0.3.39 (2026-11-21), checked against the OTel
  conventions: `ourios.receiver.tenant.held`, an UpDownCounter in
  `{tenant}` of the tenants held, one per `ourios.tenant` (OTel's
  state-count instrument, so it sums across nodes, where the first draft
  said gauge); the WARN event `ourios.receiver.tenant.hold.started`, once
  when a tenant is held, with `ourios.tenant` and the lost write's
  `error.type`; and the `error.type` value `tenant_held` on
  `ourios.ingest.batches` for a refused batch.
- The replicated backend — **#1392** (§3.6).
- [x] **The exact floor (§3.4's known cost).** Deferred, 2026-11-20.
  Records could carry their WAL offset through the sink so `H_T` is the
  tenant's real publication frontier, removing the republish window,
  which is usually one barrier interval but unbounded in the worst case.
  It arises only after a permanent audit fault and a restart, so it is
  revisited only if it is seen to matter.
- RFC 0026 `IngestDenied` durability — **#809**, owned by RFC 0026. The
  path emits before any frame exists, so replay cannot regenerate it and
  §3 does not reach it. It is a standing contradiction with RFC 0005 §3.7
  and RFC 0026's own accepted criterion, so **this RFC cannot reach
  `validated` until #809 lands**.

## 8. References

- RFC 0005 §3.7, amended here; `AGENTS.md` §3.1 and §3.4.
- RFC 0052 (`green`) §3.1 (cuts), §3.2 (the `CHECKPOINT` sidecar and the
  per-segment, tenant-aware reclamation), and its regeneration-only replay.
- RFC 0065 (`red`) §6.5 and §6.7, whose hold is #1392.
- RFC 0018 §3 (retryable refusals), RFC 0025 §3.3 (quarantine), RFC 0026.
- #1294, which this RFC answers; #802, the RFC 0053 draft it was split
  from.

## 9. Extracted wording (RFC 0053 draft, `30a21f80`)

Moved here unedited from the RFC 0053 quarry so the reviewed sentences
survive the split rather than being rewritten from memory. §3 above is
the shape of this RFC and §5 above governs acceptance; the section
numbers inside these paragraphs ("§3.1", "§3.2") and the `RFC0053.n`
ids are the draft's own.

### 9.1 The amendment, as the draft states it (status note)

It **amends accepted RFC 0005's audit-sink durability clause** (§7, "The
writer guarantees no audit event is lost across crashes…"): a permanent
audit write failure stops being a silent drop that reports success and
becomes a third outcome that refuses the dependent record publish and
makes the tenant terminal.

### 9.2 A permanent audit failure is not a settled outcome (draft §3.2)

**"Not drained" has to include a permanent drop, and today it does not.**
`AuditSink::write_owned` reports `fully_durable` as `retained.is_empty()`,
and `route_partition_result` puts a *permanent* write error on neither
path: it counts `permanent_errors`, logs the batch, and retains nothing —
so `write_owned` returns `true`, `write_ordered` proceeds, and the records
whose template events were just dropped are published anyway. That breaks
the ordering the barrier exists to give and, worse, the `CLAUDE.md` §3.1
invariant behind it: a merge with no audit event is exactly the silent
merge the project forbids, and RFC 0005's audit contract says no audit
event is lost. So this RFC **amends the audit sink's permanent-failure
policy** (RFC 0005's audit-sink contract, the flush routing in
`audit_sink.rs`): a permanent audit write failure is *not* a settled
outcome for the dependent records. `write_owned` returns a three-way
result — fully durable, retained-for-retry, or **permanently failed** —
and `write_ordered` refuses the record publish on the third exactly as it
does on the second, requeueing the records rather than publishing them
under a missing event. **The outcome is per tenant, not per batch**: a
drained batch spans tenants, and one tenant's unwritable audit partition
must not stop every other tenant's records from publishing — that would
turn a single bad partition into a node-wide stall. So the result carries
a **failed-tenant set**, the outcome is evaluated per tenant (per audit
partition, which is keyed by tenant and day), and `write_ordered`
publishes the record partitions of every tenant whose audit events are
fully durable while refusing and requeueing only those of the tenants in
that set. Retained-for-retry is already per partition on the existing
path and stays so. **Refusing forever is not a contract, so the third
outcome is terminal for that tenant**, not a retry loop: RFC 0025 §3.3's
quarantine is for permanent `BatchError`s on *data records*, and it works
by writing a `record_quarantined` event — into the very audit sink that is
failing — so it cannot be the escape hatch here. Instead a permanently
failed audit write puts the tenant in the **server-terminal,
client-retryable** class §3.1 already defines: its records stay in the
WAL, unpublished and unacknowledged-past, its appends are refused with
the tenant named, the state is exported and alerted beside
`ourios.wal.tenant_unrecoverable`, and **a restart is the only thing that
clears it**. Not an in-process operator action: the events are gone from
the sink's buffer, so clearing the flag while the process runs would let
the tenant's requeued records publish under events that were dropped —
precisely the ordering the flag exists to protect. A restart is different
in kind, because recovery re-mines the frames from the WAL and RFC 0052's
regeneration-only replay produces those template events again, so the
records publish under events that exist. The rule is therefore stated
flatly: a permanent audit failure is cleared by restart alone, after the
operator has fixed whatever made the store reject writes.

**That argument covers the record-dependent audit stream and nothing
else, which this RFC states rather than over-claims.** The events it
reasons about are the ones the miner regenerates from frames — template
merges and widenings, which accompany published records. RFC 0026's
binding denials do not fit it: `Pipeline::enforce_binding` emits an
`IngestDenied` event to the denial sink and returns the error *before any
frame is appended*, so there is nothing in the WAL for replay to
regenerate and the restart argument simply does not apply. A permanent
drop there is a real gap against RFC 0005 §3.7's no-loss contract, and it
is **out of this RFC's scope** — its subject is the ordering between
records and the events that describe them, not the denial stream, which
has no records to order against. The gap is named here rather than papered
over, and §7 carries it as a follow-up: denial events need either their
own durable path or an explicit exemption in RFC 0005's contract, and
that is RFC 0026's ground to settle.
The per-partition settlement below reads that third outcome as *not*
settled. RFC0053.2 asserts a permanent audit failure leaving the records
unpublished and requeued and the tenant terminal.

### 9.3 Derive failure, set-aside buffers, and what a restart clears

**The audit sink's derivation failure is not a quarantine, and this RFC
gives it the same terminal treatment as a permanent write.**
`derive_audit_partition` can fail *before* a `PartitionKey` exists — a
timestamp that will not resolve to a partition — so the event never
reaches `route_partition_result` and the sink counts it and drops it.
Ownership-settled, certainly; but treating that as the end of the story
would let the dependent records publish with no template event behind
them, which is the failure §3.2 exists to close and `CLAUDE.md` §3.1
forbids. RFC 0025 §3.3's quarantine cannot serve here either, for the
reason it cannot serve a permanent write failure: it works by writing an
event into the same audit sink. So a derive failure marks that event's
**tenant permanently failed**, exactly as a permanent write does —
`write_owned` reports it in the failed-tenant set, `write_ordered`
refuses that tenant's dependent records and requeues them, and the tenant
enters the server-terminal, client-retryable class.

**A requeue alone would spin, so a terminal tenant's buffers are set
aside.** Requeued records go back into the sink, the next drain picks them
up, the same audit write fails the same way, and the node burns store
calls on a failure nothing in-process can fix. So while a tenant is
terminal its partitions are **excluded from every drain** — the age
sweep's, the barrier's and the publisher's alike — rather than retried:
they stay in the buffers, under the sink's byte accounting, doing nothing
until a restart re-mines the frames behind them. Two consequences follow
and are stated rather than left implicit. Its **WAL frames stay
unsuppressed**: the tenant's publication frontier does not advance, so
recovery replays them, which is what makes the set-aside lossless rather
than a quiet drop. And **the barrier does not advance past them**: the
tenant's own frontier holds where it is, so no checkpoint or snapshot
mark claims its records are durable — the node-wide checkpoint still
advances for every healthy tenant, per §3.2's per-tenant horizon.
RFC0053.2 asserts it: a terminal tenant's partitions are not re-drained,
its records are still in the WAL after a restart, and its frontier has
not moved.

**What a restart clears, stated precisely, because "a restart clears it"
is too strong on its own.** A restart is the *mechanism* — recovery
re-mines the frames and regeneration produces the events again — but it
cures nothing by itself: it clears the state only once the **underlying
condition is repaired**, and the two conditions differ. A permanent write
failure needs the audit store to accept writes again. A derive failure
needs the clock, because the event's timestamp is **not carried in the
frame**: `MinerCluster` stamps every audit event from its own wall-clock
source (`self.clock.now()`), so the failing timestamp is a property of
when the event was emitted, not of the record being mined. A restart
therefore re-mines the same frame and stamps a *fresh* timestamp, which
derives normally on a node whose clock has recovered — and fails again,
identically, on one whose clock is still wrong, which is the same shape as
a store that is still rejecting. So the rule is one sentence: the terminal
state clears on the first restart *after* the condition behind it is
repaired, and a restart into an unrepaired condition simply re-enters it,
visibly, on the same alert. Nothing is lost either way, because the
records stay in the WAL unpublished for as long as the state holds.
RFC0053.2 covers the derive failure beside the write failure.

### 9.4 Draft open question (source for §7)

- [ ] Where RFC 0026's binding-denial events go when the audit store
      rejects writes permanently. §3.2's three-way outcome and its terminal
      rule cover the record-dependent stream only, because a denial is
      emitted before any frame exists and replay cannot regenerate it — so
      a permanent drop there is an open gap against RFC 0005 §3.7. It needs
      either a durable path of its own or an explicit exemption in that
      contract; either is RFC 0026's to settle, not this RFC's.

### 9.5 Draft references (§8)

- RFC 0005 §3.7 (audit files and their durability clause) — **amended by §3.2
  of this RFC**: the audit sink's permanent-failure path reports a third
  outcome rather than success, `write_ordered` refuses the dependent record
  publish on it, and the tenant becomes terminal until a **restart** clears
  it — restart alone, per §3.2, because the dropped events are out of the
  sink's buffer and only recovery's re-mining regenerates them. Without the amendment a dropped event is reported as
  fully durable and its records publish anyway, which breaks both that
  clause and `CLAUDE.md` §3.1.
- RFC 0025 §3.3 (permanent-encode quarantine) — the disposition for a
  poisoned *data record*; §3.2 states why it cannot serve an audit-write
  failure, and §3.2's watermark rule states how a quarantined record is
  covered.
