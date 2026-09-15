---
rfc: 0054
title: Publish unwind safety
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-09-15
supersedes: —
superseded-by: —
---

# RFC 0054 — Publish unwind safety

> **Status note.** `drafted`. Split out of RFC 0053 at the maintainer's
> direction. The reviewed prose lives in
> [`#802`](https://github.com/jensholdgaard/ourios/pull/802) @ `30a21f80`,
> §2.2 and the ownership / drain / worker / publisher parts of §3.2.
> Extract that wording. Do not invent a new protocol.
>
> Depends on RFC 0052 (barrier / sweep) and RFC 0053 (the node still
> accepts or refuses; this RFC does not decide the bound).

## 1. Summary

A panic inside publish can drop batches the age sweep has already taken
out of the sink. The snapshot barrier then reads empty buffers as
"fully drained" and can stamp a WAL mark over frames that never reached
the store (#796). Today the sweep **stops** after a panic (#795) so that
does not repeat.

This RFC chooses **duplicates over loss**. Ownership of a drained batch
stays recoverable across unwind. Only then may the sweep keep running.

It does **not** specify the `PUBLISHED` sidecar, settlement rebuild, or
audit-durability policy. Those are RFC 0055 and RFC 0056.

## 2. Motivation

See current RFC 0053 §2.2. The non-panic paths already requeue. Only
unwind does not. `CLAUDE.md` §3.4 forbids losing acknowledged data and
says nothing against delivering it twice.

## 3. Proposed design

**Invariant.** No acknowledged batch becomes unreachable because a
consumer panicked. After unwind the records (and any *unsettled* audit
events) are back in their buffers. Settled audit events stay out.

**Budget.** At-least-once. Duplicates are tolerated. The bound is
"bounded by snapshot age / in-flight partitions," not "exactly one
extra object per panic." The stronger claim in the 0053 draft fought
the snapshot-rebuild path; this RFC keeps the weaker honest one.

**Mechanism (shape, not types).** Each consuming call takes the batch
in a form whose `Drop` requeues what it has not settled. A caller-side
guard does not cover panic points *inside* the writes. Named types
(`RecoverableBatch`, pool supervisor, publisher respawn) are
implementing-PR details that must satisfy the invariant; they are not
normative API.

**Sweep.** Once the invariant holds, a recovered panic does not
permanently stop the cadence. Cancellation still terminates it.

**Out of scope.** `PUBLISHED` watermarks, tenant settlement, encode-pool
channel policy except as needed to not drop a mined batch on worker
death.

## 4. Alternatives considered

- Keep stopping the sweep (#795). Rejected: the node then requires a
  restart after every publish panic.
- Idempotent object keys. Open in RFC 0055 §7; not required to state
  the invariant.

## 5. Acceptance criteria

> **RFC0054.1 — Recoverable on unwind**
> - **Given** a drained batch and a publish double that panics at the
>   consumer boundary (before audit write, inside it, inside record
>   publish)
> - **When** the panic is recovered
> - **Then** records and unsettled audit events are back in buffers;
>   settled audit events are absent; no WAL mark crosses unrequeued data

> **RFC0054.2 — Sweep survives**
> - **Given** RFC0054.1
> - **When** the next cadence tick runs
> - **Then** the sweep continues; a later healthy barrier publishes

> **RFC0054.3 — Duplicate budget**
> - **Given** a panic after the store accepted at least one partition
> - **When** the batch is published again
> - **Then** every record id is present at least once; extras are
>   allowed and counted; absence is a failure

## 6. Testing strategy

Fault injection at the consumer boundary, parameterised per consumer
(record vs audit). Exact handle types are the implementing PR's.

## 7. Open questions

- Idempotent drain-time object keys (cost: a requeued batch must stay a
  unit). Owned with RFC 0055 if the sidecar needs them.
- Publisher-thread respawn vs disconnected-queue fallback. Implementation.

## 8. References

- RFC 0052, RFC 0053, `#795`, `#796`, `CLAUDE.md` §3.4
- Source quarry: RFC 0053 draft on #802 @ `30a21f80`

## 9. Extracted wording (RFC 0053 draft, `30a21f80`)

Moved here unedited from the RFC 0053 quarry so the reviewed sentences
survive the split rather than being rewritten from memory. §3 above is
the shape of this RFC and §5 above governs acceptance; the section
numbers inside these paragraphs ("§3.1", "§3.2") and the `RFC0053.n`
ids are the draft's own.

### 9.1 The unwind defect (draft §2.2)

**#796 — a publish unwind loses drained batches.** `drain_aged` removes
batches from the sink buffers and `write_ordered` consumes them, so a panic
inside the publish drops them: no longer buffered, never written.
`flush_then_snapshot` then reads the empty buffers as "fully drained" and can
stamp a WAL high-water mark over frames that never reached the store, after
which recovery suppresses them. The non-panic paths requeue correctly; only
the unwind does not. #795 counts the panic and **stops** the sweep, because
continuing would repeat that loss every tick; this RFC is what makes
continuing safe.

### 9.2 Duplicates over loss (draft §3.2)

`write_ordered` requeues on every error arm already. It must do the same on
an unwind, which means the handling lives inside `write_ordered` rather than
at the call site, because the call site sees a partially moved `Drained`.

The decision the unwind forces is duplicate-versus-lose: a panic part-way
through the audit write may have made some rows durable, so requeueing risks a
duplicate and dropping risks the loss in §2.2. **This RFC chooses
duplicates.** `CLAUDE.md` §3.4 forbids losing acknowledged data and says
nothing against delivering it twice, and the audit-ordering barrier is
preserved either way because the record flush is skipped whenever the audit
sink has not drained.

### 9.3 Two settlements: ownership and publication

**Two settlements are meant by that word, and this RFC keeps them apart.**
*Ownership* settlement answers "is anything stranded?" — the recoverable
batch has released this partition, so no destructor will resurrect it and
no panic can lose it without a defined state. *Publication* settlement
answers "are the rows durable?" — the put succeeded and the publication
watermark may advance. Every path records both, and they are not always
the same: a put that succeeded is settled on both counts; a requeue that
completed is ownership-settled and publication-unsettled, the buffer
holding it; a record the sink quarantined (RFC 0025 §3.3) is
ownership-settled and publication-unsettled until its
`record_quarantined` event is durable, per §3.2's watermark rule; and a
**permanently failed audit write** is ownership-settled — the batch is
not stranded, its records are back in the buffer — and emphatically
*not* publication-settled, since §3.2 requeues those records and makes
the tenant terminal. So each consuming call removes a partition from the
recoverable batch when its put has **succeeded**, its requeue has
**completed**, or the sink has **permanently dropped** it under its own
policy (the audit sink's derivation-failure path and the record sink's
RFC 0025 §3.3 quarantine) — that is the *ownership* boundary and nothing
more; what advances the watermark is publication settlement alone.

### 9.4 The per-put ownership boundary

The record sink's
quarantine has a second boundary of its own: `publish_owned` can quarantine
some records and then issue a second put for the remainder, so the
quarantined records are settled the moment they are quarantined and the
remainder is settled when *its* put returns; a panic in that put requeues
the remainder alone, and the one-duplicate bound holds per put — a
partition whose
put failed transiently is requeued at once, under the sink lock, before the
next put starts, never parked in a local vector to be requeued after the
loop as the record and audit paths do today, since a panic in a later
partition would drop that vector. An unwind then requeues only the
partition in flight and the ones not yet started — one ambiguous put, hence
one possible duplicate.

### 9.5 Mechanism: ownership, drain, encode workers, publisher

**Choosing the policy is not the same as making it hold, so the mechanism is
specified too.** `write_ordered` moves `drained.audit` into `write_owned`
before it can touch `drained.records`, so a panic inside that call drops
whatever is still owned as `Drained` unwinds — the policy alone changes
nothing. An outer guard alone is not sufficient either, and this is the
subtlety that makes the fix structural rather than local: `write_owned` and
`publish_owned` take their `Vec`s **by value**, so the moment either is called
the guard no longer owns the batch and cannot requeue it if that callee
panics. A guard around `write_ordered` covers the panic points *between* the
writes and none of the ones inside them — which are the likely ones, since
that is where the store I/O and encoding live.

So the requirement lands on the consuming calls, not on their caller, and
the ownership shape is **one** state machine rather than an either/or: a
`RecoverableBatch` handle holding each partition in one of `Pending`,
`InFlight`, `Settled { durable | dropped }` or `Requeued`. The consumer that
holds the handle moves a partition to `InFlight` when its put starts, to
`Settled` when the call returns `Ok` or the sink drops it permanently, and
takes it out of the handle only in `Settled`; the handle's `Drop` requeues
every `Pending` and `InFlight` partition and marks it `Requeued`, so after
any panic the batch is still reachable and requeued, and nothing settled is
requeued twice. A barrier can observe a sink only after the handle has been
dropped or fully settled. The points
*outside* the consuming calls — before the audit call, and between it and the
record publish — need an owner too, and today's `Drained` has none:
its `_guard` is a `PublishGuard` that only tracks the in-flight count and
holds neither batch. So `Drained` itself gains the destructor, and it exists **before either
take**: today `drain_aged` and `drain_all` take the audit buffer, then the
record partitions, and only then construct `Drained`, so a panic inside
the second take unwinds with the audit batch owned by nobody, and a panic
part-way through the record drain leaves the partitions already removed
from the buffer on the drain's own stack — a window RFC 0052's
epoch latch covered by failing the cut, and this RFC covers by ownership. So the coordinator builds
an empty `Drained` (guard and sink handles only) first, and each take
moves what it removes *into* it as it goes: the record sink's drain takes
`&mut` the `Drained`'s partition slot and pushes each partition the moment
it leaves the buffer, and the audit take moves its buffer in on return, so
at no instant does a removed batch live only on a stack frame. Its two
batches are `Option`s the consuming calls take partition by partition as
they settle — and every normal requeue arm (the audit-failure arm
that requeues the records today included) `take()`s what it requeues, so
`Drop` handles remaining ownership only and nothing is requeued twice — it carries clones of the two shared sink handles
(`SharedParquetSink` and `SharedParquetAuditSink` are `Arc`-shared already)
so that `Drop` can call the record sink's `requeue` and a new public
`SharedParquetAuditSink::requeue_ahead` — prepending the recovered events
ahead of concurrent emits and restoring the buffered gauge, which today only
the private `BufferingAuditSink::requeue_ahead` does — on whatever is still
present, **audit events first and records second**: the two buffers are
locked independently, the audit barrier treats an empty audit buffer as
"nothing pending", and a concurrent inline or size publish that observed
the record buffer restored before the audit buffer would make the
recovered rows visible ahead of their template events; and that `Drop`
path is non-panicking — it recovers a poisoned sink lock rather than
unwinding inside an unwind. A pre-call or
inter-call unwind then requeues everything through the destructor, and a
panic inside a call requeues the unsettled remainder through the call's own
handle. What the design requires is that **no** panic arm anywhere in the
sequence can drop an un-requeued batch; neither mechanism delivers that
alone, and neither can a `catch_unwind` at one call site.

**The encode workers and the publisher are in the contract too.** Under
RFC 0052 §3.1 a worker performs no store I/O: `emit_concurrent` appends
each record to the buffers, and a size- or ceiling-detached partition is
enqueued to the **publisher** — the one dedicated thread the
`PublishCoordinator` owns behind a bounded queue, which runs
`write_ordered` on each batch and settles its guard as durable, requeued,
quarantined or, on an unwind, latched. The guard is **not** the worker's
to create: RFC 0052 §3.1 has `submit` create it under the exclusion with
the batch's own epoch. Nor is it moved into one item, since a batch can
detach several partitions into several queue slots; it is a **shared
completion**, an `Arc` holding the guard and a count incremented per
detach and decremented per completion (durable, requeued, parked or
dropped), settling the guard when the count reaches zero and the batch's
encode phase has ended. Each queued item
(`PublishItem::{ Drained(Drained), Detached { records, guard,
audit_watermark } }`) carries a handle to that completion — so no
partition is ever outside both the buffers and the in-flight set, and the worker moves
to its next record with `quiesce` waiting on encodes alone. Everything this section says about `write_ordered` — the `Drained`
destructor, the `RecoverableBatch` handle, the unwind arm — therefore
attaches to the publisher thread for detached batches exactly as it does
to the age sweep's step, since both run the same function on the same
thread; nothing attaches to a worker-side PUT, because there is none. What a worker panic can still drop is
the *unappended remainder* of its mined `Vec`, and RFC 0052 has
`BatchGuard` lower `failed_epoch` for exactly that; the latch stays, and
the worker's batch takes the same recoverable shape so a cut can clear it —
held in a `RecoverableBatch` whose `Drop` requeues the remainder to the
record sink **before** `BatchGuard` decrements `pending`, so `quiesce` can
never observe idle while records sit in a panicking worker's dropped
iterator. The inner emit path has two transfer boundaries, both explicit:
the worker's guard owns a record only until `append_off_lock` **inserts**
it — the handoff happens inside that call, before its post-append trigger
step, so a panic in the trigger cannot requeue a record the sink already
holds; a permanent failure in `PartitionKey::derive`, which returns before any
insertion, comes back as an explicit `Settled::Dropped` result of the
handoff rather than something the guard has to guess at — but *dropped*
here means quarantined, never discarded: RFC 0025 §3.3 requires every
permanent encode rejection, a timestamp that will not derive included, to
emit a `record_quarantined` audit event carrying the tenant, the partition
key and the error text, with the WAL retaining the record and the
flush-error counter taking the `BatchError` variant as `error.type`. So
this handoff routes through that same path rather than beside it, and the
partition is settled only **after** the quarantine emission returns; a
failed emission leaves it unsettled and requeued, so no record leaves the
system without its operator-facing pointer. RFC0053.2 asserts it — and a detached
partition passes from the worker to the publisher's own `RecoverableBatch`
at the in-flight registration, before the enqueue and before the worker
continues, so a panic in the worker after that point cannot touch it and a
panic inside the publisher settles it per partition exactly as the sweep's
batches are. The one-duplicate bound is claimed only under those two
transfers. RFC0053.2 asserts it with a panic inside a worker's encode loop,
and with a panic inside the publisher on a batch a worker detached. Requeueing
the batch is worthless if the pool then swallows the next one, so both
halves of worker recovery are stated: a worker whose `emit_concurrent`
panics exits its OS thread today, and a **pool-owned supervisor**
respawns it: the supervisor, not the worker closures, holds the receiver,
so when a worker dies the batches still queued in the `sync_channel` are
drained and requeued and their `Pending` accounting settled rather than
dropped with the last worker's receiver; it respawns only when the join
result reports a panic — never on the normal exit a closed `tx` produces —
and it stops before teardown, so `Drop` can join; the panic is counted.
**The publisher thread gets the same treatment**, because a publisher
that died would leave `quiesce_publishes` and shutdown waiting on guards
nothing will ever settle: the `PublishCoordinator` runs each batch under
`catch_unwind`, settles a panicking batch through its `RecoverableBatch`
(requeued, latched per §3.1's guard rule), counts it with `error.type =
publisher_panic`, and continues. The publisher enters the cloned Tokio `Handle` RFC 0052 §3.1 gives it, so
a requeue taken on that thread — RFC 0047's graph update included — is not
silently skipped for running off-runtime; this RFC's requeue-on-unwind
inherits that entry rather than adding one of its own. RFC 0052 §3.1
already defines what a dying thread does — it drains every batch still queued back into the
buffers as `ready` partitions, releasing each guard as its records land,
and the restart is a **claim behind the coordinator's publisher-slot
mutex** rather than a spawn by whoever noticed: whichever worker still
sees the closed sender takes that mutex and replaces channel and thread,
the others retry against the new sender with their batches parked
meanwhile. This RFC's requeue-on-unwind goes through that same gate and
adds no spawn of its own; it changes only the failing batch's arm, from latched-and-settled to requeued
through its `RecoverableBatch`; and a worker
whose enqueue finds the queue **disconnected** (the publisher gone for
shutdown, or between death and respawn) follows RFC 0052 §3.1's park
rather than a settle: the failed send returns the item to the worker,
which puts the batch back into the buffers as a **`ready` partition**
under the sink lock *before* releasing its guard — **recording the current
`barrier_epoch` on the parked partition exactly as RFC 0052 §3.1 has a
requeue record it** — and parking is **all-or-nothing per capture**, as
that RFC settles it: a capture that would carry the sink past its ceiling
parks everything and advances nothing, never a partial take, which the
capture a forced rotation triggers inherits unchanged — so a cut captured before the park fails its
`all_ok(cut.epoch)` and cannot stamp across records it never saw; keeping
its `audit_watermark` in the buffer entry, since a park that stripped the
dependency would let a later drain publish the records ahead of their
template events, and a `Drained` that takes it carries the maximum
watermark over the partitions it took — and signals the
coordinator to respawn — a park, not a requeue-and-settle, because a guard
released before the records are back in the buffers is a window in which a
cut would read them as neither buffered nor in flight. Parked partitions
are covered by every drain, so no guard is left registered with no owner
and no wait is unbounded.
And `EncodePool::submit` on a **disconnected** pool is not a failure the
client sees at all: the mined batch goes into the record sink's buffer
through an **append-only** handoff (`SharedParquetSink::buffer_only`: no
trigger and no detached publish, since this runs on the async ingest path
and must neither register a publish nor wait on one) — the durability handoff the
pool would have reached, published by the next barrier or sweep — the turn
acknowledges normally, since the frame is WAL-durable and the batch will be
published by the next barrier, and the event is counted on a new registry
counter, `ourios.ingest.encode_fallback` (added in the same semconv bump),
since no existing instrument fits an accepted batch on a degraded path. A
*full* channel is not a fallback case: `submit` keeps blocking on the
bounded `sync_channel` as today, because that bound is what caps in-flight
memory during exactly the outage §4 says the sink cannot yet bound, and
the respawn keeps the pool from staying disconnected; a submit after the
pool has closed for shutdown is the disconnected case and takes the sink
path. An
earlier draft refused the turn before the ack; that would have told a
client to re-send a frame that was already durable and already on its way
to Parquet, and it still needed the requeue, because `mine_batch_ordered`
has already mutated the miner and a later successful append can carry
`last_durable` past the frame. The `catch_unwind` arm of
`mine_batch_ordered`, which submits its partially mined `out` and then
resumes the panic, takes the same path for `out` — but the prefix is not
enough, because the frame's *unmined suffix* still exists only in the WAL
and a later successful append would carry the mark past it. Today that
arm exists only on the pool branch: the no-pool branch calls
`miner.ingest` inline with no catch, so a panic there leaves the same
suffix with nothing recording it. This RFC collapses the two into **one
loop**: both branches drive `ingest_mined` under the same catch arm, the
pool branch collects the returned records for `submit` and the no-pool
branch emits each returned record inline as it is mined, so there is one
unwind arm and one entry, whichever branch is configured.

### 9.6 What is requeued; the sweep and the barrier task

**What is requeued depends on where the panic lands.** The audit events are
requeued only while the audit write has not completed. Once `write_owned`
has returned **fully durable** — the only one of its three outcomes that
lets the record phase proceed, per the amendment above — every event it
covered is *settled*, durable in the audit store. The other two are not
settlement: `retained` requeues the events and refuses the record publish
as it always has, and `permanently failed` refuses it too and makes the
tenant terminal, so no path treats a lost event as final while publishing
the records that depended on it. A later panic inside the record publish therefore requeues the records
alone. Requeueing settled audit events would manufacture a duplicate the
policy tolerates but does not seek, and the ordering barrier does not need it:
the next flush skips the record publish only when the audit sink has
*undrained* events, which after a completed audit write it does not. So the
recoverable audit handle need only live until that call returns; the
recoverable record handle must live until the record publish returns. The
boolean is enough for that boundary; the permanent-drop count the sink already
exports is what distinguishes the two settled outcomes for an operator.

With that settled, the age sweep can survive a panic and keep sweeping. #795
deliberately stops, because without this it would repeat the loss every tick,
and RFC 0052 §3.1 adds the epoch latch (`failed_epoch`) that keeps its
timer from stamping past a dropped batch. The stop is retired here and the
latch is kept, and the transition is explicit: with requeue-on-unwind a
recovered panic no longer strands records, so
the latch stops being a restart-only fault and becomes a signal a cut can
clear, and RFC 0052's contract is amended by this RFC in exactly that
respect. The epoch pair stays as it is — `barrier_epoch`, `failed_epoch`,
the guards' `Release`-before-decrement stores and the barrier's `Acquire`
loads — because it is the only thing that says *which* cut a dropped
batch could have been stamped past. What changes is the consequence. RFC
0052's rule is that a cut with epoch `E` fails when `failed_epoch ≤ E`
and that only a restart clears it, because in stage 1 the dropped records
exist only in the WAL. Here the panicking guard's `RecoverableBatch` has
requeued its records into the buffers *before* the guard decrements, so
they are drainable, and **the clear is defined**: a cut with `failed_epoch
≤ E` is not failed by the latch — it proceeds and stamps iff `cut_ok` and
`prior_ok`, as every cut does. A `BatchGuard` drop completes its requeue
before `quiesce_encodes` returns, so cut `E`'s own drain took those
records; a publish guard that settles after the capture leaves its
requeued records in the buffers for cut `E + 1`, and
`quiesce_publishes().all_ok(cut.epoch)` — RFC 0052 §3.2's epoch-scoped
verdict — counts the panicked publish as not-ok for `E`, so `E` does not
stamp and its pending slot is invalidated as that RFC defines. Whichever cut
stamps having drained them clears the latch, and the clear cannot erase a
failure it never drained. A CAS on `failed_epoch` alone would: a cut that
captured `E` and observed `failed_epoch = E` can be overtaken by a guard
reporting `E + 1`, which leaves `failed_epoch` at `E` — the minimum — so
the CAS would succeed and the newer failure would vanish. So the epoch latch
carries a **generation** beside it, `failure_generation: AtomicU64`, and
the publication order is stated once and holds everywhere. **At report**, a
guard increments `failure_generation` **first**, then lowers
`failed_epoch` and the generation together. **Two atomics cannot carry
this**, whatever the ordering between them: a cut that reads the epoch and
then the generation can be overtaken by a report at the *same* epoch,
which bumps the generation and leaves the epoch unchanged — the cut's
second load picks up the new generation, its captured pair equals the
current pair, and its CAS erases a failure it never drained. Ordering the
stores the other way only moves the window. So the latch is **one**
atomic, not two: a single `AtomicU64` packing **the epoch in the high 32
bits and the generation in the low 32**, and report, capture and clear are
each a single-word operation on it. A report is a CAS loop that lowers the
epoch half to the minimum of its own and what it read *and* increments the
generation half in the same word, with `Release`, before it decrements its
count; a capture is one `Acquire` load of the word, which is by
construction a coherent pair; the clear after a stamp is a
`compare_exchange` against **the exact word the cut captured**, so any
report in between — a lower epoch, the same epoch, or neither — changes
the word, the CAS fails, and the failure survives for the next cut to
drain and clear. The clear is written as RFC 0052 §3.1 encodes it — **`u32::MAX` in the
epoch half with the generation half zero**, never a zero word — and epoch
`u32::MAX` is reserved, so no real cut's epoch can collide with the clear
value and a CAS to it is unambiguous. Overflow is given a defined path rather than argued away: a wrap *is*
reachable in principle — four billion reports is a large number, not an
impossible one — and equality on a wrapped value would let a clear erase
a failure it never drained. So the generation half is **sticky at its
maximum**: a report that would carry it past `u32::MAX - 1` leaves it at
`u32::MAX` instead, and **no clear may CAS from a word whose generation
half is `u32::MAX`** — the latch holds, the state is exported, and a
restart is what resets it, which is the same fail-closed posture every
other unclearable state here takes. The epoch half needs no such rule: it is the barrier's own counter, but "one cut per `barrier_secs`" is not
the whole story — a rotation capture takes an epoch too, and a busy node
rotates far more often than it ticks — so the epoch is given a reset
rather than an argument. When `barrier_epoch` reaches `u32::MAX - 1` the
next cut performs an **epoch reset** instead of an increment: under the
barrier exclusion, after `quiesce_encodes` and `quiesce_publishes` have
returned, **no guard is outstanding** — that is exactly what those two
calls establish — so no epoch is held by anything, and the counter may be
set back to zero and the latch word cleared without any comparison losing
meaning. The reset is therefore an ordinary cut with one extra step, not
a special mode, and it keeps `u32::MAX` reserved as the clear sentinel
that no live epoch can collide with. An implementation that needs either
half wider moves to a 128-bit word or a short mutex rather than splitting
the pair again. RFC0053.3's leg drives a
same-epoch report between a capture and its clear. So the timer's pre-cut guard — RFC 0052 §3.2's pseudocode opens every
tick with

```text
    if failed_epoch <= barrier_epoch: skip // §3.1's latch; housekeeping below still runs
```

— is **replaced** by this RFC with "proceed: the cut's drain takes the
requeued records first, and the `ok` verdict and the epoch CAS decide";
the "only a restart clears it" clause, RFC0052.1's restart-only wording
and RFC0052.7's assertion of it are amended with it, and a latch set by a pre-RFC 0053 process clears on the restart
that deploys this — while the `cadence_panic` counter #795 added stays,
now meaning "a step panicked and was retried" rather than "the cadence is
dead". Only a *panicking* `JoinError` continues the sweep; a cancelled one is
the runtime going away and still terminates it, exactly as today, so an
implementation that merely deleted the `break` — and let shutdown spin — would
not satisfy this section. **The barrier task gets the same semantics**, and RFC 0052 §3.2 already
runs the barrier and housekeeping ticks under `catch_unwind`, so what this
RFC adds is the consequence rather than the mechanism: in each tick's body
— the capture and the `run_cut` — a panicking cut is dropped, and
dropping it is what requeues it, since the cut's batches are held in a
`Drained` whose destructor requeues and the snapshot bytes are a
rebuildable cache; the task continues on the next tick, and the panic is
counted on the flush-error counter with `error.type = cadence_panic` like a
sweep panic. A rotation-captured cut handed to the task is the same shape
and takes the same path. RFC0053.3 drives the barrier task as well as the
sweep.

### 9.7 Draft acceptance legs (source for §5)

> **Scenario RFC0053.2 — A publish unwind keeps the records**
> - **Given** a cadence step whose publish panics after the batches have been
>   drained out of the sink
> - **When** the unwind completes
> - **Then** the records of the in-flight and not-yet-started partitions are
>   back in their buffer, and a partition whose put had already returned
>   `Ok` stays out — a put the store had accepted but that had not returned
>   is the ambiguous case and is requeued
> - **And** the audit groups not yet settled are back in theirs; a group
>   that had settled — durable, or dropped under the sink's own
>   permanent-failure policy — is **not** requeued, so a later panic neither
>   duplicates nor resurrects it
> - **And** a panic in the third of several partition puts requeues only the
>   third and any not yet started; the two accepted objects are not written
>   again
> - **And** a transient failure on the first put followed by a panic on the
>   third leaves the first partition requeued too: a failed partition is
>   never outside the recoverable batch while it awaits requeue
> - **And** a panic inside an `EncodePool` worker's encode loop, after the WAL
>   ack, requeues the unappended remainder of that worker's mined batch, and
>   the barrier does not read the pool as idle across it; a panic inside the
>   publisher thread on a batch that worker detached settles it per
>   partition, and the worker's own guard never touches it
> - **And** a submit after that panic is either published by a respawned
>   worker or placed in the record sink's buffer and acknowledged; it is
>   never dropped behind a success, and a later successful append never
>   carries the mark past it
> - **And** the partially mined batch a panicking `mine_batch_ordered`
>   submits from its unwind arm reaches the sink before the panic resumes,
>   on the pool branch and the no-pool branch alike
> - **And** this holds for a panic raised **at each** point the publish can
>   reach it — during the drain itself, while taking the second of the two
>   batches, before the audit write, inside it, and inside the record
>   publish — since the partial-move shape means only the last of those is
>   caught by a naive guard
> - **And** the publication barrier does not read the buffers as fully
>   drained, so no high-water mark is stamped across them
> - **And** a subsequent barrier with a healthy store publishes them
> - **And** a panic raised after the store accepted an object but before the
>   publish returned leaves at most one duplicate object per panic, the rows
>   are present in every case, and the panic is counted

> **Scenario RFC0053.3 — The cadence survives a panic once unwinds are safe**
> - **Given** RFC0053.2 holding
> - **When** a cadence step panics
> - **Then** the sweep counts the panic and continues on the next tick
> - **And** a repeating panic loses no records, however many ticks it spans
> - **And** the same holds for the barrier task: a panic inside a cut —
>   timer-triggered or rotation-captured — requeues the cut's batches,
>   advances no checkpoint, is counted, and the task captures the next
>   tick's cut on schedule; it installs no snapshot when it precedes the
>   first install, and a panic *after* one leaves that tenant's snapshot
>   ahead of the checkpoint, which is the `S > X` case recovery already
>   handles by replaying from `max(X, S)` per tenant — nothing is lost and
>   nothing is republished, so the criterion asserts the pre-install points
>   rather than an undo that cannot exist
> - **And** the same holds for the publisher thread: a panic inside a
>   batch requeues it and the thread continues; a dead publisher is
>   respawned and settles the batches still queued; a worker enqueuing on
>   a disconnected queue buffers its batch instead, and `quiesce_publishes`
>   and shutdown return
> - **And** the epoch latch clears without a restart: a worker panic bumps
>   the packed word in one CAS — the epoch half lowered to the minimum and
>   the generation half incremented together — the cut that drains the
>   requeued records stamps and clears by a `compare_exchange` against the
>   exact word it loaded at its capture, and a panic raised during that cut
>   — at the same epoch or a later one — changes that word so the clear
>   fails and the failure survives for the next cut; a **same-epoch**
>   report between a capture and its clear is caught, which two separate
>   atomics would miss

### 9.8 Draft testing legs (source for §6)

- **Unwind safety (RFC0053.2, RFC0053.3)** — a publish double that panics on
  demand at **each** reachable point (before the audit write, inside it,
  inside the record publish), asserting the records and any unsettled audit
  events are back in their buffers, settled audit events are absent, the
  no mark crosses unrequeued data (a healthy barrier may publish the
  restored batches in the same call, so an unconditional refusal is not the
  contract), and a later healthy barrier publishes.
  Parameterising the panic point is the whole test: the partial-move shape
  means a guard that only covers the last point passes a single-point test.
  The point is also parameterised **across partitions, for each consumer
  separately** — record partitions and audit groups each get a drained batch
  of several with the panic in the first, a middle and the last put —
  asserting the accepted objects are not written again, the store holds at
  most one duplicate, the corresponding buffer is restored and the other
  consumer's settled items stay out; each consumer also gets a leg that
  fails the first put transiently and panics on a later one, asserting the
  failed partition or group is back in its buffer. RFC0053.3 drives many consecutive panicking ticks
  and asserts logical no-loss by unique record ids plus at most one extra
  object per panic — not an exact row count, which the accepted duplicate
  would fail.
