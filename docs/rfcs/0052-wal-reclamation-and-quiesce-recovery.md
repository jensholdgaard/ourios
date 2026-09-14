---
rfc: 0052
title: WAL reclamation and quiesce recovery
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-09-12
supersedes: —
superseded-by: —
---

# RFC 0052 — WAL reclamation and quiesce recovery

> **Status note.** `drafted`. **Stage 1 of two.** Motivated by a production
> incident (issue #791) and the defects found tracing it (#791, #793). Amends
> RFC 0008 §6.5 and §6.7 with the *policy* those sections left to a caller
> that was never written, **amends RFC 0001 §6.9** (per-tenant own-frame
> snapshot horizons and the tenant-aware retain rule supersede its global
> high-water wording; §8), **amends RFC 0018 §3.2** (whose transient class
> lists "post-rotation quiesce", which #791 disproved), and exports the WAL
> telemetry the incident showed is missing. Touches `CLAUDE.md` §3.4
> throughout, which is why it is an RFC and not patches.
>
> **Staging.** An earlier draft also carried explicit backpressure and
> unwind safety (#796). Six review rounds found most defects at the seams
> between the four parts, and the maintainer directed a split: this RFC is
> reclamation and rotation recovery — the pair that today guarantees every
> node eventually wedges — and **RFC 0053** is backpressure and unwind, which
> depend on this one and land after it. Section numbers are kept stable so the
> review history still resolves; §3.4 and §3.6 are pointers.

## 1. Summary

The WAL has never reclaimed a segment in any deployment: `checkpoint`
and `housekeeping` are called only from tests, so `wal_housekeeping_secs`
times nothing and the WAL grows monotonically for the life of the volume.
When the volume fills, rotation fails and sets a `quiesced` latch that no
code clears, so every subsequent write is refused until the process is
restarted — and the refusal carries no log, no metric, and (until #794)
no reason. This RFC commits to: advancing the checkpoint at the existing
publication barrier and running housekeeping on the existing timer; making a
rotation failure recoverable in place under a bounded retry rather than
permanent; and exporting the WAL state an operator needs to see either
happening. Explicit backpressure and unwind safety, which depend on both, are
RFC 0053.

## 2. Motivation

### 2.1 The incident

A transient object-store outage wedged a single node for eight hours
(#791). Ingest answered `503` with an empty body for every tenant, in
3 ms — a local refusal, not a timeout — and kept doing so for the last
eight of those hours, during which the object store was verified healthy.
`systemctl restart` cleared it in 21 seconds. The downstream collector
dropped 4,526 journal records and 177 application log records before
anyone noticed, and what noticed was an alert on the *collector's* queue
depth, one hop away. Ourios itself emitted no signal at all.

That is a direct contradiction of the promise §3.4 makes. WAL-before-ack
is supposed to mean a flaky object store degrades querying and delays
compaction while writes keep landing on local disk. Here a backend blip
took writes down and kept them down after the blip ended.

### 2.2 Two defects, one question

Tracing it turned up defects that are all the same question seen from
different sides — *when is it safe to declare WAL frames reclaimable, and what
happens when we cannot?* The two this RFC takes are the pair that guarantees
the wedge; a third, #796, is RFC 0053's.

**#793 — the WAL never truncates.** `Wal::checkpoint` (§6.7) and
`Wal::housekeeping` are sound, specified, and tested. They are also
dead: every call site is a test. Production reads `last_checkpoint()`
during recovery and never writes one, and `housekeeping` is documented
as a no-op before the first checkpoint, which never comes. The incident
node held 1,113 segments totalling 42 MB after five days — all age-rolled
(the minimum segment size is 17 MiB), none ever reclaimed. On a
long-enough timeline every node fills its volume.

**#791 — the quiesce latch is permanent.** `Wal::rotate` sets
`quiesced = true` at four sites, and no code anywhere sets it back. The
field doc says "until an operator intervenes" and §6.5 says "operator
intervention is required", but no intervention exists: there is no WAL
verb in the CLI and no API to clear the flag. `Wal::open` is the only
reset, so the intervention is, in practice, a restart. ENOSPC from #793
is the most likely way to reach it, but any of the four I/O steps
failing is sufficient.

**#796 — a publish unwind loses drained batches** is the same question from
the sink's side rather than the WAL's, and is specified in RFC 0053 §2.2. It
is mentioned here only because #795's stopping the sweep on a panic — the
behaviour this RFC's reclamation must coexist with — exists to bound it.

### 2.3 Why at this layer

Each of these could be patched locally, and two of them were (#794 makes
the rejection legible, #795 makes a dead cadence alertable). Neither
patch could touch the behaviour, because both decisions turn on the same
invariant: `CLAUDE.md` §3.4 says an acknowledged record must not be lost, and
every candidate fix either declares frames reclaimable (checkpoint,
truncation) or decides whether to keep accepting them at all (quiesce
recovery). Getting either wrong loses acknowledged data. That is the
definition of an RFC-level change.

The reclamation half is also not a new design. RFC 0008 §6.7 already
specifies the mechanism precisely — monotonic checkpoint, sidecar,
`min(checkpoint, snapshot_floor)` truncation bound, whole segments only,
never the current one. What it deliberately left out is the *policy*:
which offset, on what condition, on whose timer. This RFC supplies that,
and the answer turns out to be constrained almost to a single option by
code that already exists.

## 3. Proposed design

### 3.1 Checkpoint at the existing publication barrier

`flush_then_snapshot` already computes exactly the predicate a checkpoint
needs. It returns `true` only after: in-flight encodes are quiesced, the
coordinator's drained-in-flight publishes have settled
(`quiesce_publishes`), the audit sink has fully flushed, and the record
sink reports zero buffered records. Its own comment states the
consequence — "every acked record at or below the mark is durably
captured". That is the checkpoint precondition, already proven, already
under the miner lock.

So the checkpoint advances behind that barrier and nowhere else. **But the
barrier must also be reachable without an append.** Today it runs from the
rotation hook, which fires only after a successful append observes a segment
change, and from shutdown. If reclamation depended on that alone, a node that
stops receiving traffic would never advance its checkpoint again — no append,
no rotation, no barrier — and an idle node is exactly the one whose retained
segments have the least reason to exist. RFC 0053's backpressure sharpens the
same gap into a deadlock (a bound that rejects every append can then never be
cleared by one), which is why it depends on this timer rather than adding its
own. The age sweep is not a substitute: it deliberately takes no snapshot and
so establishes nothing about durability.

The barrier therefore also runs on §3.2's timer, which is append-independent
by construction. One predicate, three callers (rotation, shutdown, timer) —
and the timer caller carries the same prologue the other two already do:
`flush_then_snapshot` does **not** quiesce encode submissions itself, its
callers do it first, so a timer caller that skipped `quiesce_encodes()` would
stamp across in-flight encodes (the barrier's own class-1 case) and lose
exactly what it is meant to protect. "One predicate, three callers" means the
whole sequence, not the inner function.

**Calling the prologue is still not sufficient, and this is where the timer
differs from the other two callers.** Rotation calls `quiesce_encodes()` while
*holding the pipeline's miner lock*, and shutdown has already stopped the
listeners; each therefore has an exclusion keeping new submissions out between
the quiesce and the stamp. A bare timer has neither, so ingest can submit a
fresh encode in that window and the barrier stamps across it — the same loss,
reached by a narrower race.

**And the exclusion is the ingest gate, not the miner lock.** That distinction
is the whole of it: `ingest_bound` releases the miner lock *before*
`pool.submit(mined)` and before advancing `last_durable`, so a timer holding
only that lock can quiesce the pool and then have an already-past-the-lock
ingest submit an encode and advance the mark underneath it. The barrier would
stamp at a mark whose encode was never quiesced — the same loss again, one
layer down.

What actually makes rotation safe is that it runs *inside* an ingest's gate
turn, and the gate serializes turns in order: "every frame ≤ this seq submits
its encodes before any later seq reaches the rotation check". So the timer must
become a participant in that ordering rather than a concurrent holder of a
narrower lock.

**The existing gate cannot give it one, so this is a change to the pipeline,
not a use of it.** `ingest_gate` is a watch counter that append callers advance
with their own commit sequence; `await_ingest_turn` waits for a *sequence*, and
a timer has none to reserve. Inventing a synthetic sequence would interleave
with real ones and could stall an ingest behind a barrier that never arrives.

Instead the pipeline gains an explicit **barrier exclusion**: a lock
`ingest_bound` holds across the span that matters — from the miner work through
`pool.submit` and the `last_durable` update — and that the timer takes
exclusively to **capture a cut**, and for nothing else. It is strictly wider
than the miner lock and strictly narrower than the gate, so it does not
change ingest ordering, and it is the smallest thing that closes the window.
Rotation's *exclusion* needs no change — it already runs inside an ingest's
own span — but its *I/O* does. Today `rotate_for_segment_change` runs under
the miner lock and the gate and performs the whole of `flush_then_snapshot`
there, PUTs included; under this RFC that span is also the barrier
exclusion, so leaving the hook as it is would hold the exclusion across
store I/O at every rotation — exactly the stall the next paragraph takes the
timer out of, and with §3.3's retry it would recur on every retried
rotation during an outage. So the rotation hook becomes **capture-only**:
inside the turn it quiesces the encodes, takes the mark (`prev`, the
rotation point), drains both sinks into owned batches registered as in
flight, serialises the snapshot bytes, and hands that cut to the barrier
task, which runs the same store-I/O sequence the timer's own cuts run
(§3.2's pseudocode from `cut_ok` on). The ingest turn does no store I/O;
the mark invariant is unchanged, because the cut was captured under the
exclusion; and there is one owner of barrier I/O rather than two. The
rotation hook and the timer are then the same barrier with two triggers,
which is also what lets §3.7's `Journal::rotate` fire a rotation from the
timer without a second code path.

**The cut is captured under the exclusion; the store I/O is not.** Under the
exclusion the timer stops new submissions and waits for the *encode phase*
of every batch submitted before the cut to finish — not for store I/O. The
unit is the batch, not the worker: a batch is encode-complete when each of
its records has been appended to the buffers or detached into a registered
publish, and the pool already counts exactly that (`submit` increments the
pending count, `BatchGuard` decrements it when the worker's loop over the
batch ends), so `quiesce`'s unit stays what it is. Waiting on anything
finer is unsound: a worker takes a batch record by record, and
`emit_concurrent` detaches and publishes partitions one at a time, so a
quiesce that returned once a worker had *registered* a publish would leave
the rest of that worker's pre-cut batch unemitted — records at or below the
mark that the cut's drain cannot see and the checkpoint would stamp past.
What changes is that the worker no longer performs store I/O:
`EncodePool::quiesce` today waits for a worker even while it is inside a
`put_blocking`, and the sink's in-flight guard today covers only
`PublishCoordinator::drain_*`, not the `publish_owned` calls
`emit_concurrent` makes itself. So a size- or ceiling-detached partition is
registered as in flight inside `emit_concurrent` and handed to the sink's
off-lock publisher — the path the age sweep's `write_ordered` already uses,
audit barrier first, requeue on transient failure, quarantine on poison —
and the worker moves to its next record. A worker is then never inside a
PUT, the quiesce waits on encodes alone, and the PUTs settle outside the
exclusion under `quiesce_publishes` like any other. The barrier — reads the mark, drains
both sinks into owned batches, registers that publish as in flight, and
serialises each tenant's miner snapshot state — the bytes `write_snapshots`
would write — then releases. The flush of those batches, the snapshot *file* writes from
those bytes, and the checkpoint all run *outside* the exclusion. Snapshot
*installation* is serialised even so: every snapshot file carries that
tenant's own last folded frame from the cut (per-tenant marks, never the
checkpoint's global mark — a tenant with no frame near the cut must not be
stamped past frames it has not folded). Those marks have a receiver-side
source, since the WAL ledger says which frames exist and not which were
folded — a frame whose group fsync failed is in the WAL, never acknowledged
and never mined: each ingest turn advances the tenant's **folded horizon**
to its own frame offset after its mining step, the cut captures those
horizons, and the WAL ledger stays the eligibility source only. The field
that carries the mark is the artefact's existing `wal_high_water`, and its
meaning changes: today every tenant's artefact holds the one global mark,
after this RFC it holds that tenant's own folded horizon. The snapshot
format version is bumped (`SNAPSHOT_VERSION` 1 → 2) so the two readings can
never meet in one reader: a version-1 artefact takes the existing
unknown-version path — it is never a horizon; its tenant is discarded, logged
by tenant and version byte, and fully replayed from the WAL — and that path
is safe for one reason, which startup checks rather than assumes (§3.2's
`RECLAIM` record): a version-1 artefact can only predate this RFC, nothing
was ever reclaimed before it (#793), so its tenant has no reclaim entry and
pins at its oldest surviving frame like any tenant without a snapshot.
Reading the old global mark as a folded horizon would in fact be sound —
that mark was stamped only after every tenant's frames at or below it had
been drained and mined under the miner lock — but one byte with two meanings
across two versions is a rule the next change breaks, and pre-production a
persisted layout is broken rather than dual-read: the implementing PR
carries the `!` marker for the version bump, and no migration tooling is
written. Each writer
uses a **unique** temp
name (`<tenant>.<mark>.snap.tmp`, since today's one fixed `.snap.tmp` could
be truncated or interleaved by a concurrent cut before either rename — and
the writer unlinks its own temp on any write, fsync or rename failure, with
`load_all_durable()` removing any `*.snap.tmp` left by a previous process,
so a failed barrier cannot leak one snapshot per attempt), the whole temp
write, fsync and rename runs under a snapshot-install lock
shared by the timer, the rotation hook and shutdown, and a writer installs
only when its mark is not below the installed snapshot's — so an older cut
can neither overwrite a newer snapshot nor share its temp file, and the
in-memory horizon advances only on an install that happened.
Capturing the snapshot bytes at the cut is what makes `S` cut-consistent: it never
runs past the mark, and every row at or below it is in the detached
batches, so a turn admitted after the cut is neither in the snapshot nor
suppressed by the `max(X, S)` gate on replay. The scope of that guarantee is
stated, because it is narrower than "no duplicates": a frame published
*after* the cut and *before* the next checkpoint — by a size or age flush —
sits above `max(X, S)` and is re-published on replay. That is RFC 0008 and
RFC 0014's existing at-least-once contract above the checkpoint, unchanged
here and not claimed otherwise; a published high-water mark persisted with
every flush would bound it and is recorded in §7 as the follow-on. Detaching only the file I/O
keeps that property, and it means a slow or unavailable object store stalls ingest
for the length of a drain and never for a PUT; holding the exclusion across
`flush_then_snapshot`'s PUTs would let every retry during an outage recreate
the local ingest outage, and would keep RFC 0053's admission bound from
running at all. The mark stays correct because every frame at or below it
was drained into the detached batches, and every append after the release
lands above it. The checkpoint is taken only after the detached publish
*and* every publish registered before the cut have succeeded — `quiesce_publishes`
returns the outcome of those registered publishes rather than merely
waiting, and a failure in any of them means no stamp even though the cut's
own flush succeeded — by re-taking the journal mutex for the stamp alone; a
failed publish requeues (RFC 0053 §3.2 makes that hold on unwind too) and no
stamp happens.

**Two cadences, so the barrier is not a force-all flush cadence.** A cut
drains every buffered partition, and running it on the 60-second
`housekeeping_secs` would create a sub-target Parquet object per low-volume
partition per tick — RFC 0014's small-file hazard reintroduced by the
reclamation path. So the barrier (cut, flush, snapshot, checkpoint) runs on
its own `barrier_secs`, defaulting to the sink's age trigger (300 s) and
living beside `housekeeping_secs` in `WalConfig`, surfaced by `wal_config()`
like the other WAL knobs, validated to at least one second, and owned by the
receiver's barrier task. For a partition holding data that old the age
trigger would flush it anyway; a partition that received its first data just
before the tick is written young, so the barrier does add **at most one
sub-target object per active partition per `barrier_secs`** beyond RFC
0014's policy — the explicit trade-off for a checkpoint that needs every
buffered frame durable, and RFC0052.3 records the objects written per
interval so it is measured rather than assumed. A busy partition still
flushes on size between barriers. Housekeeping runs on `housekeeping_secs`
against the last checkpoint, and an eligible closed segment is reclaimed
by the first capped pass that reaches it — at most `barrier_secs +
housekeeping_secs` after its publication when the backlog is within the
cap, later ones waiting through further passes, and segments a pinned
tenant or a failed snapshot holds retained by design. The current append
segment is never unlinked, and rotation is checked only on an append
today, so the barrier task also calls `Journal::rotate` when the current
segment's age exceeds `segment_age_secs` and it holds a frame — an idle
node's last segment then rotates, and its bytes are reclaimed within
`barrier_secs + housekeeping_secs + segment_age_secs`, which RFC0052.3's
idle leg asserts.

**Acquisition order is fixed, or the two locks deadlock.** `ingest_bound`
takes the miner lock today before the pool submission and the `last_durable`
update; if it took the miner lock first and then waited for the exclusion
while the timer held the exclusion and waited for the miner lock, neither
would proceed. So every ingest turn acquires the exclusion **before** the
miner lock, the timer follows the same order — exclusion, then miner lock,
then the `last_durable` mutex — and the exclusion is never requested while
the miner lock is held.

**The age sweep participates in the same exclusion.** It drains under the
miner lock alone today, so a sweep could begin after the timer's quiesce and
before its stamp, leaving a drained-but-undurable batch outside the buffers
that the barrier then reads as empty; `quiesce_publishes` waits only for a
sweep already in flight and prevents no new drain. The sweep's
drain therefore takes the exclusion in shared mode — **before** the miner
lock, which means the drain helper changes rather than the shared hold being
added inside today's `with_miner(drain_aged)`: taken inside it, the sweep
would hold the miner lock waiting for the shared exclusion while the timer
held the exclusive lock waiting for the miner, and both would deadlock — held only across the
drain and the in-flight registration, and released *before* the off-lock
publish, whose PUTs can block on the store — and the timer takes it
exclusively: no sweep drains inside the barrier, a drain in progress
completes before it, and `quiesce_publishes` then waits for the publishes
already registered. A slow or unavailable store stalls ingest for the length
of a drain, never of a PUT, which is the WAL-local durability this RFC
exists to preserve.

The mark is then read inside that turn and after the quiesce — `last_durable()`
at that point, the offset the receiver's acks are gated on — so every frame
at or below it has finished its turn: mined under the miner lock the barrier
now holds, drained, and its publish waited for. Frames above it may well
exist — appended and synced by turns admitted after the cut, waiting on the
exclusion — and that is the point: they have not touched the miner state the
snapshot serialises, so the snapshot excludes them and replay re-mines them.
Reading the mark before the quiesce, or outside the turn, reopens the window
from the other side: a frame at or below the mark whose encode or publish is
still in flight.

**And the stored mark must not over-cover.** The group-commit `sync` reports
the WAL's EOF, and `CommitCoordinator::flush` captures `covered_seq` before it
locks the journal, so waiter A's outcome can carry an offset that includes
frame B, appended later by a turn that has not yet run — while B is made
durable by that same outcome and will be acknowledged once its own turn
completes. If A stored that EOF as `last_durable`, a barrier between
A's turn and B's would checkpoint across B, and recovery would suppress a
frame nothing had mined. So `last_durable` advances to the turn's **own**
frame offset — the `WalOffset` its append returned, carried through
`CommitOutcome` beside the durable EOF — and never to the EOF itself. Turns
are sequential, so the stored mark is a contiguous-success high-water by
construction, and that is the only mark the barrier reads.

Contiguity is over *acknowledged* frames, which is the property §3.4 needs. A
turn that fails still releases the gate — `IngestGateGuard` drops on every
exit — so its frame can sit below a later successful turn's mark, and a
checkpoint at that mark covers it. That is safe: a failed turn's batch was
never acknowledged (the handler answers 5xx and the client re-sends), so
suppressing its frame on replay loses nothing acknowledged, and the re-sent
copy is a fresh frame above the mark. Tracking the highest contiguous
*successful* sequence instead would hold the checkpoint back for a frame
nobody was promised.

The cost is explicit: ingest stalls for the cut's duration — a quiesce and
two drains, no I/O — once per `barrier_secs`, comparable to the stall
rotation already imposes, now on a timer. `barrier_secs` is therefore the
knob trading reclamation latency against that stall; `housekeeping_secs`
schedules only the capped unlink pass, whose own stall §3.7's per-pass cap
bounds.

See §3.7 for the ownership path and the exact signatures, which the barrier
does not have today:

```text
if barrier_succeeded and high_water is Some(mark):
    journal.checkpoint(mark)            // §6.7, monotonic; Result
      on Err  -> log, do NOT advance; nothing past the PREVIOUS mark is reclaimed
```

Three properties come free from existing code and must not be
reimplemented:

- **Monotonicity and idempotence** — `checkpoint` rejects a regression
  and no-ops on a re-assert, so a repeated stamp at the same mark is
  safe.
- **Fail-closed** — on a sidecar write error the in-memory checkpoint is
  not advanced, so nothing past the *previous* mark is reclaimed — segments
  already eligible under that mark still are (RFC0052.1) — rather than risk
  a post-crash duplicate.
- **Skip-on-retain** — when either sink retains anything,
  `flush_then_snapshot` returns `false` and no checkpoint is attempted.
  The WAL keeps the frames and the next start re-mines them.

**A cadence panic latches the barrier until RFC 0053.** #795 stops the age
sweep after a panic, which was a safe interim only while nothing else could
stamp: this RFC's timer can, and after a panic inside `write_ordered` the
drained batches are gone and the publish guard has dropped, so a timer
barrier would see empty buffers and no in-flight publish and stamp past
records that never reached Parquet. So a panic on **any** path that holds
records outside both the buffers and Parquet sets a `cadence_failed` latch —
set on the unwind path itself, by a guard's `Drop` that runs *before* the
publish guard is released, never by the joining task observing the panic
later, or a barrier could acquire the exclusion, see empty buffers and stamp
in that window. Three paths hold records that way, and all three set it:
the age sweep's step, from the in-flight registration `drain_aged` takes
through `write_ordered` — one guard wraps the whole step, so a panic
mid-drain, with records in a half-built batch, latches too; the barrier's
own detached flush; and an **encode worker**. The worker is the one the
earlier drafts missed: `emit_concurrent` runs after `ingest_bound` has
acknowledged the frame, and the pool's `BatchGuard` today only settles the
pending count on unwind, so a worker panic mid-batch leaves `quiesce` seeing
no pending work while the batch's unemitted remainder is in neither the
buffers nor Parquet — a barrier after it would stamp across the frame. The
`BatchGuard` therefore sets the latch when it drops unwinding, before it
decrements, so worker unwind feeds the same failed-cut path as a cadence
panic in this stage; RFC0052.10 is not gated on RFC 0053 for it. While the
latch is set the barrier neither checkpoints nor snapshots — the frames stay
in the WAL and a restart replays them — the state is exported (RFC0052.7)
and only a restart clears it. The check lives inside `flush_then_snapshot`,
the one function the timer, the rotation hook and shutdown all call, so
every stamping caller honours it rather than the timer alone, and it is made
**twice**: before the cut, and again immediately before the snapshot install
and the checkpoint, after `quiesce_publishes` has returned. The second check
is not redundant. A publish registered before the barrier began can panic
*while the barrier is waiting on it*, and the latch it sets lands after the
first check; a latch observed at the recheck is a **failed cut** — no
install, no stamp; the cut's own batches are in the store already and the
WAL still holds every frame, so a restart replays them into the `max(X, S)`
gate like any other. The same unwind guard also records that publish's
outcome as failed, so `quiesce_publishes().all_ok()` is false for it
independently of the latch: the recheck defends the ordering, the outcome
defends the data, and either alone refuses the stamp. RFC 0053's
requeue-on-unwind is what removes the latch, by making every one of those
panics lose nothing to stamp past.

A snapshot *write* failure is deliberately not a checkpoint blocker:
`flush_then_snapshot` logs it and still returns `true`, because the data
is in the store and the snapshot only governs replay depth. The
truncation floor below is what keeps that safe for the WAL. It is safe for
Parquet only if recovery honours the checkpoint on the Parquet side: after a
restart, frames at or below `X` may be re-fed to the **miner** when the
snapshot lags (`S < X`), but must never be re-published to the record sink,
or every row between `S` and `X` lands in Parquet twice. `recovery.rs`
describes exactly that split and today gates only the miner on `S`; the
Parquet-side gate on `X` is a requirement of this RFC, not an assumption, and
RFC0052.10 asserts it — a restart after a failed snapshot write and an
advanced checkpoint produces no duplicate rows.

### 3.2 Housekeeping on the timer that already exists

`WalConfig::housekeeping_secs` (default 60) is set in every test and
read by nothing. RFC 0008 §6.7 says "the timer lives in the caller".
The caller is the receiver role, on its own interval:

```text
every barrier_secs (default: the sink age trigger, 300 s):
    if cadence_failed: skip                // §3.1; housekeeping below still runs
    cut = with_barrier_exclusion:          // a NEW pipeline lock, not the miner
                                           // lock and not the gate — see §3.1
        quiesce_encodes()                  // the barrier's prologue: every pre-cut
                                           // batch encode-complete (§3.1)
        mark = last_durable()              // read AFTER the quiesce, INSIDE the turn
        batches = drain both sinks         // owned; registered as in flight
        snaps = serialise miner snapshots  // cut-consistent S (§3.1)
        (mark, batches, snaps)             // release the exclusion here
    run_cut(cut)

on a rotation (inside the ingest turn that observed the segment change):
    cut = capture as above, mark = prev    // the turn already holds the exclusion;
                                           // no store I/O in the turn (§3.1)
    hand cut to the barrier task           // which runs run_cut(cut) in order

run_cut(cut):
    cut_ok   = flush(cut.batches)          // store I/O OUTSIDE the exclusion
    prior_ok = quiesce_publishes().all_ok() // earlier in-flight publishes: outcome, not a wait;
                                           // always evaluated, so a failed cut flush cannot
                                           // skip their outcome and requeue path
    ok = cut_ok and prior_ok and not cadence_failed
                                           // the recheck (§3.1): a panic while this
                                           // barrier waited is a failed cut
    if ok and cut.mark is Some(_):
        install_snapshots(cut.snaps)       // under the install lock, monotonic in mark;
                                           // failure logged, not a blocker (§3.1); with
                                           // no mark the previous snapshots stay
    if ok and cut.mark is Some(m):
        coordinator.checkpoint(m)          // re-takes the journal mutex for the stamp alone
      on Err -> log, do NOT advance (fail-closed, §3.1)

every housekeeping_secs (default 60 s), in its own task:
    coordinator.maintain(snapshot_horizons(), max_unlinks_per_pass)
                                           // derives the floor (§3.7), unlinks by the
                                           // per-segment tenant-aware rule (§3.2), sweeps partials
      on Err -> log; the next pass retries (nothing was unlinked past the bound)
```

**The timers have the age sweep's shutdown lifecycle but not its task.**
Both loops are owned by the receiver, signalled by the same shutdown watch
the sweep observes, and joined **before** the final flush and snapshot, in
the order `ReceiverHandle::shutdown` already joins the sweep; they hold no
pipeline or journal handle after the join. They are *separate tasks* from
the sweep, and from each other: the sweep's stop-on-panic (#795) must not
stop reclamation, and a panic in a barrier step — or in the sweep's own
step, or in an encode worker (§3.1's three latching paths) — sets
`cadence_failed` and leaves the housekeeping loop running housekeeping-only
passes, which is the branch the pseudocode's skip relies on. The sweep's
panic is the one #795 already stops the sweep on; what this RFC adds is that
the same unwind now latches *before* the sweep's publish guard drops, so the
barrier that #795 could not foresee cannot stamp past the batch the panic
dropped, whether the barrier was waiting on that publish or arrives later. A timer that was not joined at
shutdown could race the shutdown reclamation or keep the WAL alive past the
handle, which is why the ordering is part of the design rather than of the
implementation.

**`retain_floor` is the MINIMUM over per-tenant snapshot horizons, not the
latest one.** `write_snapshots` persists one snapshot per tenant and can
partially succeed, so there is no single "latest" mark: taking the highest
would let housekeeping unlink frames above a *lagging* tenant's horizon, and
that tenant's miner state could then not be rebuilt. `CLAUDE.md` §3.7 is
explicit that every path touching data is tenant-scoped, and a per-node floor
is exactly the bolted-on form it forbids.

The minimum is already this codebase's stated contract, not a new invention —
`recovery.rs`'s stale-gap detector names it:

> the §6.7 retain floor (min over tenant horizons) keeps any segment holding
> frames above the floor, and a lagging tenant's own `S.segment` is protected
> by its own membership in the min

Under tenant-aware retention that argument survives only if each tenant's
snapshot horizon `S` is **that tenant's own last folded frame offset**, not
the cut's global mark — a global mark's segment can hold none of the
tenant's frames and be legitimately unlinked, and the detector would then
report external mutation for `S < X`. So snapshots record per-tenant
horizons: `S.segment` always holds one of that tenant's frames, is retained
until the tenant's horizon covers it, and is unlinked only under the
checkpoint, so its absence implies `S ≤ X` and the detector stays quiet.

That detector's no-false-positive property *depends* on the minimum, so the
earlier draft's "latest" wording would have broken stale-gap detection as
well as losing data.

**A tenant with WAL data and no valid snapshot pins the floor at its oldest
surviving frame.** It does not halt reclamation outright — an earlier draft
said "reclaim nothing", and with shared segments and tenant churn that is a
permanent halt: a tenant that wrote once and whose snapshot never landed
would block every other tenant forever. What hazard #5 actually requires is
that *that tenant's* frames survive, since with no snapshot a restart must
re-mine all of them. So eligibility is **per segment and tenant-aware**,
not a single global bound — a global "highest offset below the minimum"
rule would let one unsnapshotted frame pin every later segment, even those
holding only other tenants, which is the permanent growth this section
exists to end. The rule, stated once: a closed segment is reclaimable when
its highest offset is at or below the checkpoint (**inclusive**, a
post-append horizon), and for every tenant with a frame in it that tenant's
last offset in that segment is **strictly below** its valid horizon — strict,
so the segment holding the horizon frame itself is retained, which is what
keeps the stale-gap detector's argument true: `S`'s segment is present on
every restart, and an absent segment implies the tenant's frames in it were
all below `S`. A pinned tenant has no horizon, so exactly its own segments
are retained — equality can never unlink the pinned frame —
while segments holding only other tenants' covered frames are reclaimed
whatever their offset. `RetainFloor` is the *reported* summary of that
rule (the minimum over horizons and pins), not the predicate. The pin lifts the moment a valid snapshot for the tenant is
written, since its horizon then replaces the pin. The pin is an
**exclusive** bound: `housekeeping` removes a closed segment only when its
highest offset is strictly below it, whereas a `Min` horizon stays
inclusive as `Wal::housekeeping` compares today — otherwise a segment
holding exactly one frame for the pinning tenant, whose highest offset
*equals* the pin, would be unlinked. `None` is passed only where no
snapshot consumer exists at all.

**The ledger is WAL-owned, so the floor is derived where the frames are.**
Neither the `Journal` API nor `RecoveryReport` could otherwise tell the
receiver which tenants have surviving frames or at what offset. The WAL
learns a frame's tenant from the `TenantOtlpBatch` prefix it already
frames in `append_batch`, and `remeasure_unreclaimed()` rebuilds the ledger
from every surviving frame's prefix at the end of recovery; the receiver
passes only what it knows — the per-tenant snapshot horizons, or "no
consumer" — and the WAL derives `RetainFloor` from the two and reports it.
The accounting is frame-kind-specific: only `TenantOtlpBatch` frames carry
tenant membership and drive the per-tenant horizon rule, because they are
what a restart re-mines. `AuditEvent` frames are governed by the checkpoint
alone — their consumer is the audit sink, which the barrier drains before it
stamps, so an audit frame at or below `X` is published and reclaimable and
one above `X` is retained by the checkpoint rule with no snapshot horizon
involved; a segment holding only a pinned tenant's audit frames is
reclaimable once below `X`, since the pin protects re-mining input and audit
frames are not that.
Membership is per surviving segment, so the ledger has a lifecycle. Each
segment carries, per tenant, the `WalOffset` of that tenant's **first and
last** frame in it — the first is what `Pinned.offset` needs, the last is
what the eligibility rule compares the tenant's horizon with, since a
horizon between the two must not unlink the later frames; a membership set
alone could recover neither, and a segment-wide boundary would be a
different contract — rebuilt at open from the recovery walk, the last
updated on every live append; a tenant's oldest surviving frame is its
first offset in its oldest
surviving segment. A tenant leaves the
ledger when its last surviving segment is unlinked, which can only happen
once its snapshot horizon has passed those frames; nothing else removes it,
and nothing needs to. `recover` returns entries only for tenants that *have*
a `.snap`, so without this ledger an empty or partial snapshot set would be
indistinguishable from "no consumer" and read as `None`.

**And the floor may only be derived from snapshots known to be durable.**
`snapshot_store::write` renames the new snapshot into place *before* its
parent-directory fsync, so a failure there leaves the file visible to this
process while the write returned an error. A floor derived by listing `.snap`
files would then trust a horizon that may not survive a crash, and housekeeping
would unlink segments on the strength of it — losing exactly the frames the
floor exists to retain.

So the derivation uses the in-memory record of snapshots whose write returned
`Ok`, not the directory contents. That record has an owner: a receiver-side
`SnapshotLedger`, restored from `load_all_durable()` at startup, updated by
`write_snapshots`' **per-tenant** report — returned instead of
short-circuiting on the first error into a single `Result<()>` — so it
advances exactly the tenants whose new snapshot completed and keeps the
previous durable horizon for the rest, and read as the `SnapshotHorizons`
handed to `maintain`. No path re-reads the files after a failed fsync. A tenant whose snapshot write failed keeps
its *previous* durable horizon if it has one and otherwise pins the floor at
its oldest surviving frame, per the rule above — conservative, and
self-correcting on the next successful write. Reading the directory is
legitimate only at startup, before anything has been reclaimed in this
process's lifetime — and even then only after one more step.

**The startup listing is revalidated by a directory fsync before it is
trusted.** The in-process rule covers failures this process observed, but a
previous process may have renamed a snapshot into place, failed the
parent-directory fsync, and exited cleanly; the entry is then still visible
to a restarted process, which would load it as a horizon, reclaim on the
strength of it, and lose the frames when a later machine crash drops the
never-durable entry. So before any listed `.snap` is used as a horizon the
snapshots root **and its parent** are fsynced once — `snapshot_store::write`
creates the directory with `create_dir_all` and fsyncs only the child, so
the directory's own entry in `wal_root` is not durable either until this
RFC adds a parent fsync on creation — which makes every entry the listing
saw durable, the one property the listing lacks. This is an explicit
operation, `snapshot_store::load_all_durable()`, which fsyncs before it
lists, treats a missing `snapshots_root` as the empty store (a cold start
has no directory yet, and that is not a durability failure), and **fails
startup** when either fsync fails, as a `RecoveryDriverError` — so an
implementation cannot keep trusting the bare listing. An earlier draft
discarded every snapshot and started empty instead; that is not fail-closed
once reclamation is live, because a tenant's older durable snapshot may be
the only thing that can rebuild its state, the frames below that horizon
being gone, and a node that starts empty then replays a tail that cannot
restore it. A disk that cannot fsync the snapshots directory at startup is
a fault to surface, not to continue past. One directory fsync rather than a manifest, because a manifest
would need the same fsync to be trustworthy itself. Durability is necessary
and not sufficient: `snapshot_store::load_all` returns raw bytes, and a
horizon is admitted only from a snapshot that decodes and restores. A listed
file that fails either is never a horizon — but whether the node may then
*continue* needs durable evidence, because once reclamation has run, the
frames below that tenant's previous horizon are gone and a pin at its oldest
surviving frame cannot rebuild the state they held. So `housekeeping`
persists, per tenant, the horizon it reclaimed under — a `RECLAIM` sidecar
beside `CHECKPOINT`, under the same contract as that file and not a looser
one, because startup's fallback is exactly as trustworthy as this record:

- **Atomic and durable, before the unlinks — and off the writer position.**
  Written whole to `RECLAIM.tmp` (a fixed name, like `CHECKPOINT.tmp`:
  housekeeping is the single writer, so a temp left by a crash is simply
  truncated by the next write, and RFC0052.16's selector leaves it alone),
  fsynced, renamed over `RECLAIM`, parent fsynced — and only then does the
  pass unlink. The record is on disk before any frame it accounts for is
  gone; a torn write is a temp and never the record. None of that file work
  happens under the journal mutex: serialising every tenant's entry and
  three fsyncs is O(tenant count) plus disk latency, which would put fsync
  time in front of every concurrent append and break RFC0052.12's O(cap)
  claim. The pass therefore has two halves. Under the writer position it
  pops at most the cap's worth of eligible segments and stale partials from
  the ledger's structures — removing them from the ledger and its
  accounting, and noting the horizon each popped segment was reclaimed
  under per tenant — and releases. Off the writer position it merges those
  horizons into the record, writes and fsyncs it, and only then unlinks the
  popped files and fsyncs the parent. The ordering rule is what the
  startup contract needs and it is unchanged: the record is durable before
  the segments it covers are gone. A file whose unlink fails stays in a
  small pending-unlink list outside the ledger and is retried by the next
  pass's off-lock half, still within that pass's cap; it is already out of
  the ledger, so it can neither be popped twice nor counted as retained.
  Housekeeping's ledger half is the only half that takes the writer
  position, which is what "runs on rotation's ownership path" means below.
- **Monotonic per tenant, by merge.** The pass reads the record, raises each
  tenant it is about to reclaim under to that horizon, and never lowers an
  entry; a tenant absent from the record has never had a frame reclaimed
  under a horizon. One writer holds the writer position, so the
  read-modify-write cannot interleave.
- **Created at open.** The WAL writes an empty record, durably, when it opens
  a root that has none — before the first pass can run — so the record
  exists on every root this RFC's code has ever reclaimed from. A root with
  no record at startup is therefore a layout that predates this RFC, on
  which nothing was ever reclaimed (#793), and that is the *only* case read
  as "no reclaimed state". There is no second witness to fall back on, which
  is why the record is created before it is needed rather than at first use.
- **Corrupt is fatal.** The record carries a version byte and a checksum; a
  record failing either at open fails startup as `OpenError::Corrupt`,
  naming the file. It is not read as missing: missing means "never
  reclaimed", damaged means "reclaimed, extent unknown", and only the first
  is safe to proceed from.

The WAL exposes the record's per-tenant horizons as
`ReclaimState::reclaimed_through`, and recovery compares: a tenant whose
restorable horizon is below its recorded reclaimed-through — including a
tenant with an entry and no restorable snapshot at all — has unrecoverable
state and recovery **halts**, naming the tenant; a tenant with no entry has
lost nothing and pins at its oldest surviving frame as before. An
undecodable or missing snapshot is therefore safe to fall back from exactly
when the record proves it is, and RFC0052.17 holds each of those cases.

The floor is also the reason §3.1 can tolerate a failed snapshot write.
`housekeeping` reclaims only segments every tenant's horizon covers, and
never past the checkpoint, so a stale floor makes truncation conservative — it retains frames a snapshot has not
captured, which degrades the next start to a fuller replay and never to loss
(hazard #5's retain rule, RFC 0001 §6.9).

Housekeeping's ledger half takes the WAL's single-writer position, so it
runs on the same ownership path as rotation rather than concurrently with
it; its file half — the record write, the unlinks, the parent fsync — runs
after that position is released, on the housekeeping task alone.

### 3.3 Rotation failure becomes recoverable, under a bounded retry

A quiesce must stop being permanent. The five sites — the four the code has
today and the `rename(partial, final)` step the temporary name adds — are
not equally retryable, and the design treats them by what they leave behind:

| Site | Failure | State left behind | Retry |
|---|---|---|---|
| closing-segment `fdatasync` | `sync_file_data` | nothing new; current segment unchanged | directly |
| create fresh segment | `create_fresh_segment` | at most a `.wal.partial`, never a segment | directly; the partial is swept |
| fresh-segment header `fsync` | `sync_file_data` | a `.wal.partial` with possibly-torn bytes | directly; the partial is swept |
| `rename(partial, final)` | `rename` | the `.wal.partial` only; nothing installed | directly; the partial is swept |
| parent-dir `fsync` (post-rename) | `sync_parent_dir` | a **complete, installed** segment whose entry may not be durable | re-fsync it; **never unlink** |

The last row is the one to read carefully. Because the rename happens before
that fsync and the install happens before it too, there is no orphan to clean up
there — the file is the live append target. Unlinking it would destroy the
segment the WAL is currently writing to. The retry is the directory fsync alone,
carried by `dir_fsync_pending`.

The orphan is why the current code quiesces rather than retrying, and the
existing comment says so: a surviving directory entry whose header bytes
were lost fails the next `Wal::open`'s header read, turning a benign crash
into `OpenError::Corrupt`.

**Unlinking the orphan before each retry is not sufficient, and this RFC
does not rely on it.** A `remove_file` is not durable until the parent
directory is fsynced, so a crash in between can bring the entry back; and
after the final failed retry a header-only orphan under the *final* name
can still be sitting there, which `Wal::open` may select as the newest
segment — that is the legacy shape RFC0052.11 handles; a `.wal.partial` is
never selectable, since `list_segments` returns only `*.wal`. Either
case contradicts RFC0052.4/.5's restart property, so the design removes the
orphan *class* instead of cleaning up after it:

**A fresh segment is created under a temporary name, its header fsynced, then
renamed to its final `.wal` name, and only then is the parent directory
fsynced.** The ordering matters in both directions:

- the header must be durable *before* the rename, or a surviving `.wal` entry
  can have unreadable header bytes — today's hazard;
- the parent fsync must come *after* the rename, because the rename creates
  the directory entry and an fsync taken before it does not make that entry
  durable.

**And the new segment is installed before that fsync, not after.** This is what
makes the failure path reachable rather than a description of a state the code
cannot be in: `rotate` sets `current_segment`/`path`/`uuid` to the renamed file
*first*, then fsyncs the parent, and on failure sets `dir_fsync_pending` and
returns **without quiescing**. The new segment is then genuinely current, so
`sync` — which fsyncs `current_segment` and discharges `dir_fsync_pending` —
closes the obligation, exactly as it already does for the segment `open`
creates. Since no batch is acked until `sync` returns `Ok`, no frame is ever
acked in a segment whose directory entry is not durable.

**The retry is the group-commit `sync`, not the next append**, and saying
"a retry re-attempts the fsync" blurred that. Once the new empty segment is
current, the next `append`'s rotation check is false, so it simply writes its
frame; nothing re-enters `rotate`. What retries is the `sync` that follows,
which is the right place: it is the operation acks are gated on, so a repeated
directory-fsync failure repeatedly refuses to ack rather than accumulating
unacked frames behind a rotation that never reruns. Those appends are not lost
either — they are in the segment, and the next successful `sync` covers them.

**That retry draws on the same budget and reaches the same terminal state —
when the obligation came from a rotation.** `dir_fsync_pending` starts
`true` on *every* `Wal::open`, fresh or reopened, so the flag alone cannot
say where the obligation came from; it therefore carries its origin,
`Open` or `Rotation`. A failed discharge of an `Open` obligation is an
ordinary retryable sync failure outside the budget, exactly as today. A
failed discharge of a `Rotation` obligation consumes one unit of the
rotation retry budget, exactly as a failed `rotate` does, and exhausting it
enters the terminal state whichever operation gets there — which is what
keeps RFC0052.15's narrowness true: an ordinary fsync failure never
becomes terminal. Both
`AppendError` and `SyncError` carry a typed terminal variant, and
`IngestFailure::classify` maps both under the terminal-only rule. The
plumbing through group commit is stated too, because today it would lose
the type: `CommitCoordinator::flush` collapses every sync error into
`SyncFailure { detail: String }` and waiters rebuild a generic
`ReceiveError::WalSync`. `FlushOutcome` therefore gains the error's class —
terminal or transient — beside the detail, and waiters rebuild a
`ReceiveError` that carries it, so the classifier sees the terminal state
rather than a string; §6 drives both a rotation-origin terminal sync
failure and an ordinary transient one through `flush` and the waiter, so
the production path cannot erase the class while the classifier tests pass. The retry hint is settled honestly: an append, sync
or still-retrying rotation failure has **no scheduled retry** on the server
— the next attempt is the client's next request — so those transient
classes carry no hint and the client backs off as OTLP prescribes, exactly
as the merged #794 left them; a hint appears only where a real server-side
cadence exists, which in stage 1 is nowhere and in RFC 0053 is the
backpressure state's housekeeping cadence. The transient/terminal
distinction therefore rides the classification and the `Status` message,
not a header — so a
persistent parent-directory fsync failure is not left as an ordinary
`WalSync` that RFC0052.5 and RFC0052.15 could never observe.

Installing *after* the fsync, as the draft implied, leaves the renamed file
owned by nobody: `rotate` would still point at the old segment while a complete
`.wal` sits beside it, which is the orphan case all over again.

A rotation that fails *before the rename* therefore never leaves a file that
looks like a segment. This amends RFC 0008 §6.5 explicitly, and the
amendment is wider than the create sequence: §6.5 required the new
segment's directory entry to be durable before any frame landed and a
permanent refusal after a rotation failure, and Scenario RFC0008.6 asserts
both. Under this RFC the segment is installed before its parent fsync,
frames may land while `dir_fsync_pending` is retried (none acked until
`sync` discharges it), and the refusal is bounded rather than permanent —
so RFC0008.6 is superseded by RFC0052.4/.5 and §6.5's durable-entry-first
wording by §3.3 here. It is the one on-disk behaviour change in this RFC;
it needs no format or schema change, because the temporary name never
becomes a segment.

**The post-rename window is not covered by that, and needs its own rule.** If
the final `sync_parent_dir` fails, the `.wal` name already exists — so the
"never looks like a segment" property does not hold there, and the temporary
-name sweep does not reach it. That file is a *complete, header-durable*
segment whose directory entry may not survive a crash, which is exactly the
state `dir_fsync_pending` already exists to describe. So it is kept, not
unlinked, and the rotation records that the directory fsync is still owed: the
next `sync` discharges it before acking anything, as it already does for the
segment `open` creates. A retry after this failure re-attempts the fsync
rather than creating a second segment.

**Temporary files are swept explicitly, not implicitly.** `list_segments`
returns only `*.wal`, so a temporary file is invisible to it — which is the
point for `Wal::open`, and means housekeeping would *not* remove it either.
Left at that, a persistently failing rotation would leave one temporary file
per retry, consuming disk that reclamation exists to free. So
housekeeping gains a second, explicit step — but "temporary files" is far too
broad a selector to state loosely. The WAL root already holds `CHECKPOINT.tmp`
and the snapshots directory holds `*.snap.tmp`; a sweep matching `*.tmp` could
delete an in-progress checkpoint or snapshot, which is a worse failure than the
debris it collects.

So the name is **reserved and exact**: the in-progress segment is
`<uuid>.wal.partial`, and every file of that shape in the WAL root belongs to
the WAL — the sweep cannot tell rotation debris from a file an operator
placed under a reserved name, so it does not try, and unlinks any stale one.
A file that does not match the shape (`foo.wal.partial`, anything else) is
ignored, as non-segment files are everywhere else in the WAL, and `*.tmp` is
never matched: it stays the checkpoint and snapshot namespace. Two further
rules make it safe:

- there is no in-flight partial to protect: housekeeping takes the
  single-writer position, so no rotation attempt is in progress while it
  runs, and every partial it sees is debris — an earlier draft's name-based
  "skip the live one" rule had no ownership path and is withdrawn;
- the unlinks are followed by a **parent-directory fsync**, as
  `housekeeping`'s segment unlinks already are, so a crash cannot resurrect
  swept debris.

The step runs **inside** the same per-pass cap as segment unlinking (§3.7) and
counts against it, because it pops from the same ledger under the same
single-writer position — a backlog of stale partials would otherwise make a
"bounded" pass do unbounded directory work and break RFC0052.12; the unlink
itself runs in the pass's off-lock half like a segment's (§3.2). An unlink
failure is logged and retried on the next pass like any other.

Two more properties of the sweep are stated because the current
`housekeeping` has neither. **It runs on every pass**, regardless of the
checkpoint precondition: today `housekeeping` returns at once when no
checkpoint exists, and a rotation that fails before the first checkpoint
would otherwise leave partials that every later pass skips; only the
segment-unlink portion is gated on a checkpoint. **And it lists nothing on
the pass**: partials left by a previous process are seeded into an
in-memory partial list by `remeasure_unreclaimed()`, which walks the
directory at the end of recovery anyway, live rotations register the
partial they create, and the sweep pops from that list under the same cap
— so restart debris is found without a directory scan under the writer
lock. **Its error path still
makes completed removals durable**: when a pass fails part-way — a header
read, a stat, a later unlink — the parent-directory fsync covers every unlink
that already succeeded before the error is returned, and
`HousekeepingProgress` reports the partial count. Today's single fsync after
the complete scan would let a crash resurrect a partial the pass had already
removed.

**Pre-existing `*.wal` orphans need their own answer, and neither changing
future rotations nor guessing at open is it.** A node wedged before this RFC
lands may already hold a segment whose header fsync failed, so its header
bytes may be partial — `Wal::open` validates the newest segment's header
and can return `OpenError::Corrupt` on it, while older segments are checked
by `Wal::replay` and surface as recovery errors; either path halts. Claiming these are "real zero-frame segments" was
wrong.

An earlier draft of this section had `Wal::open` unlink such a file when it was
the newest and the only unreadable one. **That is withdrawn.** An unreadable
newest segment is indistinguishable from genuine header corruption, and
RFC0008.5's contract is that corruption halts rather than being cleaned up;
a heuristic that unlinks it can silently discard real data, which is a worse
failure than the one it avoids. No shape-based guess is safe here, because the
shape carries no evidence of which cause produced it.

So: these nodes keep today's behaviour and still halt at open. What this RFC
adds is that the halt is *actionable* — the error names the file, describes
the observable shape (newest segment, unreadable header) and notes that a
failed rotation is one way to produce it, without claiming that it did, so an
operator can weigh removing it. Whether that deserves a WAL verb rather than a documented manual
step is in §7; either way the decision is a human's, because only a human can
weigh "this is probably rotation debris" against "this might be my data".
Future rotations cannot reach this state at all, so the population is finite
and shrinking.

A pre-rename retry is attempted on the next `append`, and a post-rename
directory-fsync retry by the next `sync` (above); neither runs in a
background loop of its own: the WAL is single-writer and has no task, and a
rotation is only needed when there is something to write. The one
append-independent caller is the barrier task's idle rotation (§3.2,
`Journal::rotate`), and it is not exempt: a timer-triggered rotation that
fails draws on the same budget and can reach the terminal state without an
append ever arriving, which RFC0052.4/.5 cover. `quiesced` becomes a typed state
carrying the attempt count and the first underlying error, so the
distinction between "retrying" and "given up" is representable rather
than a bool. The budget is a **count**: three consecutive failed attempts
by default (`rotation_retry_attempts`, beside the other rotation knobs),
counting a failed `rotate` and a failed `Rotation`-origin fsync discharge
alike, reset to zero only when the pending obligation itself succeeds — the
rotation, or the directory-fsync discharge — never by an unrelated
successful operation, or a parent fsync failing behind succeeding data syncs
would never reach the terminal state; with no backoff of its own — each
retry rides the next append or sync, whose cadence is the backoff. After
the budget is exhausted the WAL enters a terminal state that is still
reported distinctly (see §3.5) and still refuses appends — a disk that has failed the same fsync a
dozen times is not going to be fixed by a thirteenth attempt, and
hammering it obscures the real fault.

The retry must not widen the ack surface: a rotation that has not
completed means the append did not happen, so the batch is not acked,
exactly as today.

**This is a contract change against a passing test, and says so.**
`rfc0008_6_rotation_failure_quiesces_the_wal` asserts that after a rotation
failure "every subsequent append is refused ... **even after the underlying
condition clears**". That is precisely the behaviour §3.3 replaces, so the
test is not inconvenient — it is the specification of what we are choosing to
change. Per `CLAUDE.md` §6.2 the implementation must surface that explicitly
and get approval before editing it, rather than quietly adjusting it to pass.
The replacement asserts the new contract on both sides: a transient failure
recovers (RFC0052.4) and a persistent one still refuses forever
(RFC0052.5), so the protection the original test provides is kept, not
dropped.

**This RFC amends RFC 0018 §3.2, and that amendment belongs here, with the
retry it depends on.** §3.2's transient class lists "post-rotation quiesce"
alongside WAL append I/O and fsync failures, all mapping to `UNAVAILABLE` /
`503` "with an optional `Retry-After` header". Classifying the quiesce as
transient was right when the latch was assumed to be a momentary condition;
#791 showed it is permanent until a restart, so the classification is wrong in
the one way that matters — it tells a client the node will recover on its own.

The amendment is narrow and does **not** touch §3.2's binding rule, which is
that a transient failure must never carry a non-retryable code. The status
stays `UNAVAILABLE` / `503` for exactly the reason §3.2 gives: the batch was
not acked, and every non-retryable OTLP status also tells the client to drop
it. What changes is the *class* — and under this section's bounded retry, only
the **terminal** state leaves the transient class. A rotation failure still
within its retry budget genuinely is transient: a later append can succeed, so
it stays in the transient class and its message says it is retrying. Only
once the budget is exhausted is the node in a state no delay fixes, and only
there does the classification change, with the message naming the state. No
retry hint is carried on either — the server schedules no retry of its own
(§3.7), so the client backs off as OTLP prescribes — and RFC0052.15 pins
both halves on class and message.

### 3.4 Backpressure — moved to RFC 0053

The explicit local bound, its pre-append reservation, its `Retry-After`
contract and its livelock fix are RFC 0053 §3.1. They depend on this RFC's
timer (§3.1), barrier exclusion (§3.1) and `ReclaimState` (§3.7), which is why
they follow it rather than share a document with it. The number is kept so
the review history's section references still resolve.

### 3.5 The state becomes observable

`WalMetrics` already carries `unflushed_bytes`, `disk_bytes` and
`segment_count`, and none of the three is exported: the only WAL metric
names in the registry are `ourios.wal.append.duration` and
`ourios.receiver.wal.truncated`. `unflushed_bytes` is read only
internally, for the fsync-batching decision. So "the WAL stopped
growing" — the incident's one observable symptom — was unobservable even
though the numbers existed in memory.

This RFC exports the existing three and adds the state the other sections
actually depend on. `ReclaimState` (§3.7) carries, and the exporter surfaces:

- **unreclaimed bytes and the age of the oldest unreclaimed frame** — the
  bytes are what RFC 0053's bound is measured on, and the age is what an
  operator alerts on; "all unreclaimed" means including the post-checkpoint
  tail, the current segment and anything the floor retains. No frame carries
  a timestamp, so the age is a defined proxy: the oldest surviving segment's
  `UUIDv7` timestamp, its creation time, which is older than or equal to
  every frame in it and therefore conservative — and `None` whenever
  unreclaimed frame bytes are zero, since the header-only current segment
  `Wal::open` creates or retains holds no frame and an idle WAL must not
  report a growing age. An earlier draft said "below the checkpoint", which would have
  exported the one number that does not grow during an outage;
- **the retain floor and its lag** — `lag_bytes`, the frame bytes in the
  segments the floor retains below the checkpoint, and `lag_segments`, their
  count; `WalOffset` is a `(UUIDv7, byte)` pair and has no subtraction of its
  own, and the figures come from per-segment frame-byte accounting the WAL
  keeps incrementally (seeded by `remeasure_unreclaimed()`, updated on every
  append and unlink) — never from an inspection, which §3.7's cap would
  bound and a large backlog would make untrustworthy — including whether it is `RetainFloor::Pinned` and by how many
  tenants — without which RFC0052.13's "an operator can tell a
  lagging tenant from an unexplained stall" is not achievable, since that is
  the case where reclamation stops with a healthy store;
- **the rotation-failure state**: retrying, with its attempt count, versus
  terminal;
- **the `cadence_failed` latch** — receiver-owned rather than a
  `ReclaimState` field, because the timer task sets it and no WAL call
  observes it; the receiver exports it beside the `ReclaimState` fields,
  and RFC0052.7 names which surface each item comes from.

A log event is emitted on entering and on leaving a refusing state — for
transitions observable within one process; the terminal rotation state's
only exit today is a restart a fresh WAL cannot observe, so it has an entry
event and no leave event until §7's operator verb exists to emit one
(RFC0052.7 says the same). Names come
from the shared `ourios-semconv` registry in one bump, not hand-written, and
`error.type` continues to carry the failure class on existing counters rather
than spawning per-error metrics.

**Transitions have an observer, so "exactly one event" is implementable.**
A state snapshot alone cannot emit anything. The receiver's timer task is
the single emission owner for the states that change on ticks: it compares
a **categorical projection** of `ReclaimState` — floor kind and latch, never
the byte, age or lag figures, which change on every pass — before and after
each `maintain` call and emits on change (floor pinned or lifted, latch
set). Rotation-state transitions are *not* sampled that way, because a
rotation can fail on one append and recover on the next before either
sample and the sampler would see no change: the coordinator emits those
edges **synchronously at the transition**, under the journal mutex, as RFC
0053 does for its refusal latch, so entry into `Retrying` or `Terminal` and
the recovery out of `Retrying` each emit exactly once at the point they
happen. The channel is defined: the coordinator records the WAL's
rotation state after every append and sync outcome into a shared
`IngestState` cell — written by the WAL itself under its own mutex, in the
order it observed the outcomes, never from flush outcomes that publish
monotonically by `covered_seq` rather than by completion, so an older
transient outcome cannot overwrite a newer terminal one — and the
projection is built from that cell, from
`ReclaimState`, and from the task's own `cadence_failed` latch — an explicit
input, not a `ReclaimState` field. Rotation-state edges, including the
terminal entry, are emitted synchronously at the transition as stated
above — never sampled, never late. One owner per state, real call sites,
registry names.

### 3.6 Requeue on unwind — moved to RFC 0053

The duplicate-versus-lose decision, the consuming-call ownership rule and
the age-sweep survival that follows from them are RFC 0053 §3.2. They are
deferred rather than dropped: #795's stop-on-panic plus §3.1's
`cadence_failed` latch is the safe interim, and it is what RFC 0053 replaces. The
number is kept so the review history's section references still resolve.

### 3.7 Ownership and API surface

§3.1 and §3.2 are not callable as written today, and saying so precisely is
part of this RFC's job.

**The barrier has no WAL handle.** `flush_then_snapshot` receives the record
sink, the audit sink, the snapshots root, the miner, an
`Option<WalOffset>` high-water mark and a cadence label. The WAL itself is
boxed as a `Box<dyn Journal>` inside a `Mutex` in the commit coordinator,
which is where the single-writer position lives (RFC 0008 §3.1).

**`Journal` has no reclamation surface.** The trait is `append_batch`,
`sync`, `unflushed_bytes`. `Wal::checkpoint` and `Wal::housekeeping` are
inherent methods on the concrete type, so the pipeline cannot reach them
through the trait object.

So the design is:

- **`Journal` gains the two reclamation methods and a state accessor**, with
  a single object-safe error type rather than the concrete WAL's two:

  ```text
  enum ReclaimError {
      Checkpoint(..),
      Housekeeping { progress: HousekeepingProgress, source: .. },
  }                                                        // object-safe, one type
  struct HousekeepingProgress {
      removed_segments: usize,   // what the forced-rotation trigger reads (RFC 0053)
      removed_partials: usize,
      capped: bool,              // "more to do" versus "backlog drained"
      floor: RetainFloor,        // derived on this pass (§3.2)
      lag_bytes: u64,            // §3.5's units, from per-segment accounting —
      lag_segments: usize,       // never from an inspection the cap would bound
  }                              // carried on Err too, so partial work, floor and
                                 // lag stay observable on the failure path

  /// Why a floor is or is not available — `Option` cannot carry this.
  enum RetainFloor {
      Unknown,               // no pass has derived it yet: reclaim nothing, report as such
      None,                  // no snapshot consumer exists: checkpoint alone governs
      Min(WalOffset),        // the minimum over every tenant's horizon
      Pinned { offset: WalOffset, tenants: usize },
                             // some tenant has no valid snapshot: the minimum
                             // includes its oldest surviving frame
  }

  fn append_batch(&mut self, payload: &[u8]) -> Result<WalOffset, ReceiveError>
                                                           // the ReceiveError boundary the
                                                           // coordinator and both transports
                                                           // classify against is kept; only
                                                           // the offset Wal::append already
                                                           // produces is added (§3.1's mark)
  fn checkpoint(&mut self, durable_to: WalOffset) -> Result<(), ReclaimError>
  enum SnapshotHorizons { NoConsumer, Known(HashMap<TenantId, WalOffset>) }
  fn housekeeping(&mut self, horizons: &SnapshotHorizons, max_unlinks: usize)
      -> Result<HousekeepingProgress, ReclaimError>   // derives RetainFloor itself
  fn reclaim_state(&self) -> ReclaimState                  // §3.5's export surface
  fn rotate(&mut self) -> Result<(), ReceiveError>         // §3.3's retried rotation,
                                                           // callable without an append:
                                                           // discharges a pending directory
                                                           // fsync first, no-op when the
                                                           // current segment holds no frame;
                                                           // Wal::rotate is private today.
                                                           // RFC 0053 uses this definition.
  ```

  **The own-frame mark needs plumbing, stated so it cannot be skipped.**
  `Journal::append_batch` discards the `WalOffset` `Wal::append` returns
  today, and `CommitOutcome` exposes only the group-sync result. So
  `append_batch` returns the offset, the coordinator records each waiter's
  own offset by sequence at append time, `CommitOutcome` carries it beside
  the durable EOF, and every test double returns a monotonically increasing
  offset per append. The call site uses that own offset for everything it
  compares with `last_durable` — the segment-change detection that fires
  the rotation hook included — so a flush spanning an old-segment turn and
  a new-segment turn fires the hook once, on the new-segment turn, with the
  old segment's mark, rather than on both. An implementation that kept storing the EOF fails
  RFC0052.14's two-turn flush, which is what that criterion is for.

  `RetainFloor` exists because `Option<WalOffset>` cannot carry what §3.2
  requires: `None` means no snapshot consumer exists and the checkpoint
  alone governs, which is *safe to reclaim*; `Min` is the complete case;
  `Pinned` says the minimum is being held down by tenants without a valid
  snapshot, at their oldest surviving frames, which is *correct but must be
  visible*. An `Option` collapses the last two, and a signature that hides
  the pinned case is the wrong signature.

  **The floor the export reports is the floor the WAL derived on its last
  pass.** The receiver passes `SnapshotHorizons` — what it knows — and the
  WAL combines them with its own ledger (§3.2) into `RetainFloor`, returns
  it in `HousekeepingProgress` and keeps it for `reclaim_state()`, with its
  lag against the checkpoint and its `Pinned` count. Between passes that
  is by definition the floor governing retention, so the export is never
  stale and needs no hidden coupling; before the first pass it reports
  `Unknown`, which `maintain` treats as "reclaim nothing" and the export
  shows as its own state — not `None`, which would claim no consumer
  exists.

  `max_unlinks` is a parameter rather than WAL configuration because the cap
  belongs to the caller's stall budget, and `HousekeepingProgress` reports
  whether the pass hit the cap, so the caller can tell "backlog drained" from
  "more to do" without re-deriving it. Without both, RFC0052.12's
  bounded-stall contract has no way to be implemented.

  The concrete impl maps `CheckpointError` and `HousekeepingError` into
  `ReclaimError`; a `Box<dyn Journal>` cannot infer an associated error type,
  which is why one enum rather than two. `ReclaimState` is a plain snapshot
  struct carrying what §3.5 exports — the existing `unflushed_bytes`,
  `segment_count` and best-effort `disk_bytes` (a diagnostic, and the only
  path the trait object has to it), **all unreclaimed bytes and the age of the oldest
  unreclaimed frame** (the bytes being what RFC 0053's bound is taken on,
  and not a below-checkpoint figure), the
  retain floor with its lag and `Pinned` state, and the rotation-failure
  state — so the server reads it without reaching past the trait.

  **`disk_bytes` is not reused for the admission measurement.** `WalMetrics`
  documents it as best-effort: its directory walk skips unreadable entries and
  reports no error when the walk itself fails, so a silent undercount would let
  admission sail past the bound — the one place a best-effort number is
  unacceptable. The unreclaimed-byte figure is maintained incrementally from
  appends and unlinks, as the WAL already does for `unflushed_bytes`, and
  `disk_bytes` stays what it is: a diagnostic. The figure is **seeded after
  recovery, not at `Wal::open`**: open runs before `recovery::recover`
  replays and heals the newest segment, so a seed taken there would count
  torn bytes. Recovery therefore ends — after every successful replay, and
  after the heal when there was a torn tail to heal, since a clean replay
  never enters that path — by calling `Wal::remeasure_unreclaimed()`, a
  concrete method since recovery holds the WAL before it is boxed, which
  sets the figure to the sum over every surviving `*.wal` of file size less
  the segment header, in frame bytes, and seeds the current segment's own
  frame bytes from the healed newest segment at the same time. The
  coordinator is constructed after recovery (the ordering below), so no
  append can precede the seed, and a restart mid-outage resumes from the
  true backlog rather than from zero. `unflushed_bytes`
  stays its own method: the
  group-commit coordinator reads it per batch and must not allocate a
  snapshot struct on that path.

  On the trait rather than via a downcast, because RFC0052.1 and RFC0052.2
  need test doubles that can observe reclamation.
- **Recovery gains the Parquet-side gate §3.1 requires.** `recovery::DriverSink`
  owns only the miner today and merely records `parquet_horizon` — and
  `MinerCluster::ingest` already emits every mined record to the sink `serve`
  wired in before recovery, so a second sink handle in the driver would emit
  replayed rows twice. Recovery therefore drives the miner through a capture
  path (`ingest_mined`, or the configured sink disconnected for the replay)
  so that replay emits nothing implicitly, feeds the miner only above that
  tenant's snapshot horizon `S`, as now, and forwards the captured records to
  the record sink only above **`max(X, S)` per tenant**, and captures the
  miner's **audit events** the same way — through a replay capture sink the
  driver installs, which receives both mined records and audit events
  tagged with the offset of the frame being replayed, since `AuditEvent`
  carries no offset and `ingest_mined` diverts only records — forwarding
  events only for frames above `X` (the barrier drains the audit sink before
  it stamps, so `X` is the audit horizon): events for `(S, X]` are suppressed
  and counted, events above `X` forwarded, and neither duplicated nor lost.
  Regeneration by the miner is the **only** source of replayed events:
  stored `AuditEvent` frames are not replayed — nothing writes them today
  (`encode_audit_event` is `unimplemented!()`, RFC 0008 §9's deferral) and
  recovery keeps ignoring the kind — and an encoder that lands later must
  either keep replay on regeneration or amend this leg, never feed both,
  or an event would be injected twice;
  RFC0052.10 gains the audit leg — `max`, because the
  snapshot is written before the checkpoint is persisted, so a successful
  snapshot followed by a failed checkpoint write leaves `S > X` with `(X, S]`
  already in Parquet. Frames below that gate are suppressed on the Parquet
  side and counted, so a restart never republishes a row, and RFC0052.10's
  no-duplicate leg — including the `S > X` ordering — is what proves the gate
  exists rather than assumes it.
- **The barrier reaches them through the coordinator**, which already owns the
  journal mutex, rather than taking a second handle to the same WAL. A second
  handle would put two owners on a single-writer resource, which is the one
  thing RFC 0008 §3.1 forbids. Concretely, `CommitCoordinator` gains
  `maintain(&self, horizons: &SnapshotHorizons, cap: usize) ->
  Result<HousekeepingProgress, ReclaimError>` — the documented housekeeping
  contract, no separate report type — and `checkpoint(&self, mark:
  WalOffset) -> Result<(), ReclaimError>`, the two operations that run
  `housekeeping` and `checkpoint` on its private journal mutex; `serve` reaches it from the
  timer through `SharedPipeline`, and runs it on the blocking pool because
  it does file I/O while holding a `std` mutex — the stall is the mutex,
  not the thread, as the cap paragraph below says.
- **`Option<WalOffset>` skips, and the post-recovery call is an explicit
  exception.** `None` means no high-water mark is known, and there is then
  nothing to declare reclaimable. `None` is *not* the same as "post-recovery",
  which the earlier draft conflated: `serve` passes `report.max_delivered`,
  which is `Some` whenever replay delivered a frame, and it does so **before
  the commit coordinator exists**, so there is no journal owner to checkpoint
  through. That call therefore deliberately advances no checkpoint; the first
  timer pass after the coordinator is built does it instead, from the same
  mark or a later one. And when replay delivers **nothing** — the normal
  shape once housekeeping has reclaimed every closed frame and the node
  restarts idle — that post-recovery snapshot write preserves each tenant's
  *restored* horizon rather than writing `None` over it, which would have
  discarded the snapshots on the next restart and forced a full replay every
  time. The mixed case needs the same care: a replay that delivered frames
  for tenant A only must record A's progress and keep B's restored horizon,
  which one global `max_delivered` cannot express, so `RecoveryReport`
  returns **per-tenant delivered horizons** and the post-recovery write is
  driven from those. §3.1's "behind the barrier and nowhere else" holds —
  this is a caller that has the mark but not yet the owner, not a second
  checkpoint site.
- **Errors are fail-closed and never swallowed.** A failed `checkpoint` logs
  and leaves the in-memory checkpoint unadvanced, which is already
  `Wal::checkpoint`'s own behaviour; the WAL then reclaims nothing past the
  *previous* mark, which stays usable — RFC0052.1 requires exactly that,
  not that every segment is kept. A
  failed `housekeeping` logs and the next pass retries, since nothing was
  unlinked past the bound. Neither failure fails the barrier: the data is in
  object storage either way, and refusing to ack over a reclamation error
  would turn a disk-space problem into an availability one.

**One consequence worth naming.** Housekeeping takes the single-writer
position, so every append stalls for the duration of the directory walk plus
the unlinks. Running it on the blocking pool does **not** help: the stall is
caused by holding the journal mutex, not by occupying a runtime worker, so
moving the work to another thread while still holding the lock bounds nothing.

The pass is therefore **capped on the unlinks, and the eligibility
evaluation costs no I/O at all.** The current path enumerates and sorts every
`*.wal` and reads each candidate's header before unlinking any, so a
1,113-segment backlog did 1,113 header reads under the mutex however few files
it then removed — which is why an earlier draft capped the *inspected*
candidates too. That cap is withdrawn, because it conflicts with tenant-aware
eligibility: the oldest segment may be pinned while a later one is eligible,
and a pass that inspected only the oldest few would stop at the pinned one
every tick and never reach the eligible one. It is also no longer needed:
everything a header read supplied — the segment's highest offset, each
tenant's last offset in it, its frame bytes — lives in the per-segment ledger
the WAL maintains in memory (§3.2). An O(n) walk of that ledger under the
writer lock would still not be a bound — `n` is exactly what a pinned tenant
or a store outage leaves unbounded — so eligibility is **indexed
incrementally** rather than evaluated per pass: the WAL keeps, per segment,
the set of tenants whose horizon is still below their last offset in it;
applying new horizons (the `SnapshotHorizons` input, under the mutex)
shrinks those sets for the segments those tenants span — **lazily and
capped**: at most the cap's worth of segments per pass, resumed from a
per-tenant cursor on the next, so a tenant whose snapshot catches up after
the incident's 1,113-segment outage cannot hold the writer position for the
whole backlog — and empty-set segments sit in an ordered structure keyed by
highest offset. The cursor is defined once, here. Per tenant it holds the
highest segment offset up to which that tenant's horizon has been applied;
horizons are monotonic per tenant and a tenant's last offsets rise with the
segments, so application is a prefix walk oldest-first, and a higher
horizon only extends the walk's target — the cursor never moves backwards
and is never reset by a checkpoint advance, which touches no set. Each pass
spends its application budget round-robin across the tenants whose cursor
is behind their horizon, so one tenant cannot starve another's. The only
rebuild is at open, when `remeasure_unreclaimed()` rebuilds the ledger and
every cursor starts at the oldest surviving segment; the first pass after
startup applies from there. The eligible head needs no cursor of its own:
it is re-evaluated from the ordered structure on every pass. There is **no promotion pass**: a checkpoint advance stores
the new mark, O(1) under the mutex, and a segment is eligible when it is
empty-set *and* its key is at or below the stored mark — a predicate the
pass evaluates lazily at the head of the ordered structure, popping while
the head's key is at or below the checkpoint and at most `max_unlinks`
times. An earlier draft promoted every newly covered empty-set segment into
an eligible queue on the checkpoint advance, in one ordered pass; that pass
is O(backlog) under the journal mutex — on the incident's 1,113 segments it
stalls every append before the capped unlink even starts — and contradicts
the bound this section exists to give, so it is withdrawn. Nothing is
stranded without it: a segment whose set empties above the checkpoint waits
in the ordered structure and is at its head once the mark passes it, and a
segment the mark passed while its set was non-empty is found once a horizon
application empties it. The pass therefore does at most the cap's worth of
horizon application plus at most the cap's worth of pops — O(cap) under the
writer lock, with no name listing, since segments are known from the ledger
— and `HousekeepingProgress` reports `capped` from either half hitting its
budget, plus how much horizon application remains. RFC0052.12 is worded to
that guarantee. The next tick continues where this one left off — the
per-tenant cursor is the resume point for horizon application, and the
eligible head is re-evaluated from the ordered structure, oldest first, so
it needs none. That matters most on
the *first* pass of a node that has never reclaimed — the incident node held
1,113 segments, and an uncapped pass would have stalled ingest for as long as
1,113 unlinks take. In steady state, after §3.2 is running, a pass has a
handful of segments to consider and the cap never binds.

## 4. Alternatives considered

**Checkpoint on its own timer, independent of the publication barrier.**
Rejected: it would need to re-derive "everything at or below this offset
is durable in the store", which is precisely what the barrier computes
under the miner lock. A second derivation is a second thing to get
wrong, and the two could disagree.

**Checkpoint at every successful cadence flush rather than at the
barrier.** Rejected: a cadence flush is per-partition and best-effort,
and does not establish the whole-sink condition. It would advance the
checkpoint past partitions that had not drained.

**Leave the quiesce permanent and add an operator verb to clear it.**
This is what the current comments promise, so it deserves a real answer:
it is honest and simple, and it puts a human in the loop before writes
resume after a durability fault. Rejected as the *primary* mechanism
because the fault it most often fires on is transient and
self-correcting, and requiring a human for a condition that has already
cleared is how the incident produced eight hours of downtime. A bounded
retry that gives up into a distinctly-reported terminal state keeps the
human in the loop for faults that are actually persistent. An explicit
verb remains worth adding later for the terminal state.

**Tolerate a torn header in `Wal::open` instead of unlinking the orphan.**
Rejected: it weakens the corruption detection RFC0008.5 depends on in
order to avoid one `remove_file`, and a zero-frame segment with an
unreadable header is indistinguishable from a real corruption at open
time.

**Retry rotation from a background task.** Rejected: the WAL is
single-writer by design (RFC 0008 §3.1) and owns no task. A background
retry would need to take the writer position away from the append path,
which is a far larger change than retrying where the need arises.

**The alternatives to backpressure and to requeue-on-unwind** — a hard sink
ceiling, dropping on unwind — moved with those designs to RFC 0053 §4. The
sink-ceiling finding there (memory can OOM before any disk bound fires)
is a prerequisite of RFC 0053, not of this RFC: nothing here bounds
memory, and nothing here claims to.

## 5. Acceptance criteria

> **Scenario RFC0052.1 — The checkpoint advances only behind a proven
> publication barrier**
> - **Given** a WAL with acknowledged frames and a record sink whose
>   store is healthy
> - **When** the publication barrier completes with both sinks fully
>   drained
> - **Then** the journal's checkpoint is advanced to the barrier's
>   high-water mark
> - **And** when either sink retains anything, no checkpoint is
>   attempted and `last_checkpoint()` is unchanged
> - **And** when the high-water mark is `None`, no checkpoint is attempted
> - **And** while the `cadence_failed` latch is set, the barrier neither
>   checkpoints nor snapshots, however many timer passes run
> - **And** an age-sweep publish registered before the barrier began, which
>   panics while the barrier waits in `quiesce_publishes`, leaves the
>   checkpoint and every snapshot unchanged: the latch set after the
>   barrier's first check is observed at its recheck before stamping, and
>   the publish's outcome is reported failed independently of it
> - **And** an encode worker panicking mid-batch, followed by a barrier,
>   leaves the checkpoint and every snapshot unchanged, and the batch's
>   unemitted records are replayed on restart
> - **And** when the checkpoint write fails, the barrier still reports
>   success, `last_checkpoint()` is unchanged, and the next housekeeping pass
>   reclaims nothing that was **not already eligible under the previous
>   mark** — `Wal::checkpoint` leaves that mark intact on failure, so
>   forbidding all reclamation would reject the fail-closed behaviour §3.1
>   specifies rather than test it

> **Scenario RFC0052.2 — Segments are reclaimed, and never past the
> *minimum* tenant snapshot floor**
> - **Given** a checkpoint advanced past several closed segments and **two
>   tenants** whose snapshot horizons differ, the lagging one below that
>   checkpoint
> - **When** housekeeping runs
> - **Then** only segments at or below the checkpoint whose every tenant's
>   horizon is at or above that tenant's last frame in them (both
>   inclusive) are unlinked, the current append segment survives, and the
>   WAL's segment count falls
> - **And** a segment holding the frame that sits exactly at a tenant's
>   horizon is retained — the per-tenant comparison is strict, only the
>   checkpoint comparison is inclusive — and is unlinked once the tenant's
>   horizon has moved past it
> - **And** a frame above the *lagging* tenant's horizon is still present
>   after the pass, so a restart re-mines it rather than losing that
>   tenant's miner state
> - **And** when a tenant with WAL data has no valid snapshot, the pass
>   retains exactly that tenant's segments (RFC0052.13's pin) and still
>   reclaims a later segment holding only other tenants' covered frames,
>   while the temp sweep still runs

> **Scenario RFC0052.3 — Sustained ingest does not grow the WAL without
> bound**
> - **Given** a node ingesting continuously at a rate its publish and
>   reclamation path can sustain (the soak is capacity-balanced and pinned
>   as such), with a healthy store and every tenant with WAL data holding a
>   valid and advancing snapshot (a complete floor), run long enough to
>   roll many segments
> - **When** the reclamation cadence has had time to act
> - **Then** the WAL's on-disk byte total and segment count are bounded
>   rather than monotonically increasing — the #793 signature (1,113
>   segments, nothing ever unlinked) cannot reproduce
> - **And** while the floor is `Pinned` the pinning tenants' frames are
>   retained by design and the state is visible (RFC0052.13); a healthy
>   store alone does not bound the WAL, a complete floor does
> - **And** an offered rate above that capacity grows the WAL by
>   construction until RFC 0053's admission bound exists; this criterion
>   claims no bound there
> - **And** after the last append, with no further traffic, one
>   `barrier_secs` plus one `housekeeping_secs` later the checkpoint has
>   advanced and every eligible **closed** segment is reclaimed — the
>   append-independent path, which continuous traffic alone cannot prove —
>   and within `segment_age_secs + barrier_secs + housekeeping_secs` the
>   last segment has rotated on a barrier tick and been reclaimed by the
>   following pass

> **Scenario RFC0052.4 — A transient rotation failure recovers without a
> restart**
> - **Given** a WAL whose rotation fails once with a transient I/O error —
>   triggered by an append, or by the barrier task's idle rotation
> - **When** the condition clears and, for a failure **before the rename**,
>   a later `append` arrives — or, for the post-rename directory fsync, the
>   next `sync` runs
> - **Then** the pre-rename case re-enters `rotate`, which succeeds, and the
>   append is accepted and acked; the post-rename case discharges the
>   pending directory fsync in `sync` without re-entering `rotate`, and the
>   batches behind it are acked
> - **And** when the failure was **before the rename**, no file a subsequent
>   `Wal::open` would select as a segment remains from the failed attempt —
>   asserted by opening the WAL again, not by inspecting the directory, since
>   the temporary name is an implementation detail
> - **And** when the failure was the **post-rename directory fsync**, the
>   installed segment is selected by a subsequent open — it is complete and
>   valid — and the first `sync` after open discharges its pending directory
>   fsync
> - **And** the temporary files left by the failed attempts are gone after
>   successive capped housekeeping passes — `max_unlinks_per_pass` is
>   validated to be at least the retry budget, so one rotation's debris
>   clears in one pass — and a persistently retrying node cannot fill its
>   disk with retry debris

> **Scenario RFC0052.5 — A persistent rotation failure gives up
> distinguishably, and never acks**
> - **Given** a WAL whose rotation fails on every attempt — in `rotate`
>   itself, or in the directory-fsync discharge that `sync` retries
> - **When** appends continue past the bounded retry count
> - **Then** every append is refused, no batch is acknowledged, and the
>   refusal is reported as the terminal state rather than as a transient
>   one
> - **And** the first underlying I/O error is still recoverable from the
>   reported state, not replaced by a generic "quiesced" message
> - **And** `Wal::open` on the resulting directory succeeds rather than
>   reporting corruption, including when the cleanup of the last attempt's
>   temporary file never completed

> **Scenario RFC0052.11 — A node already wedged before this RFC halts
> actionably at open**
> - **Given** a WAL directory in the state today's rotation failure leaves:
>   a newest `*.wal` whose header bytes are partial, written before this RFC
>   landed
> - **When** the WAL is opened
> - **Then** it still reports `OpenError::Corrupt` rather than unlinking
>   anything — §3.3 withdrew the shape-based heuristic, because an unreadable
>   newest segment is indistinguishable from real corruption
> - **And** the error names the file, describes the observable shape and
>   names a failed rotation as one possible cause without asserting it, so
>   an operator can decide deliberately
> - **And** a node whose rotations all happened under this RFC cannot reach
>   that state at all, so the population is finite and shrinking

> **Scenario RFC0052.12 — A reclamation pass bounds its per-file work**
> - **Given** a backlog far larger than `max_unlinks_per_pass` (the
>   incident's 1,113 segments is the shape)
> - **When** a housekeeping pass runs
> - **Then** it unlinks at most the cap and returns, having read no segment
>   header and listed no directory — eligibility comes from the incremental
>   index, and the pass pops at most the cap from the eligible queue — so an
>   append taken concurrently waits for O(cap) ledger work under the writer
>   lock whatever the backlog, and never for the `RECLAIM` write, an unlink
>   or an fsync, which run after the position is released; applying changed
>   horizons is itself capped —
>   at most the cap's worth of segments per pass, resumed from a per-tenant
>   cursor next pass, so a tenant catching up after a long outage cannot
>   hold the writer position for the whole backlog — and an unchanged
>   pinned backlog costs none
> - **And** a checkpoint advance is O(1) under the mutex — it promotes
>   nothing eagerly, whatever the backlog — and a segment whose set emptied
>   above the old checkpoint is reclaimed by the first pass after the mark
>   passes it, so it is not stranded when horizons stop changing
> - **And** stale partials left by a previous process are removed from the
>   list seeded at recovery, without a directory listing on the pass
> - **And** a pinned oldest segment does not shadow a later eligible one: the
>   pass reclaims the eligible segment on its first tick
> - **And** successive passes drain the backlog to the same end state an
>   uncapped pass would reach
> - **And** a backlog of stale *temporary* files is also bounded by the same
>   cap, so temp sweeping cannot make a "bounded" pass do unbounded work

> **Scenario RFC0052.16 — The temp sweep touches only files of the reserved
> partial shape**
> - **Given** a WAL root holding a `CHECKPOINT.tmp` and a `RECLAIM.tmp`, a
>   snapshots directory holding a `*.snap.tmp`, and a stale
>   `<uuid>.wal.partial`
> - **When** a housekeeping pass runs
> - **Then** only the partial is unlinked: the checkpoint, reclaim and
>   snapshot temps survive
> - **And** the unlink is followed by a parent-directory fsync, so a crash
>   cannot resurrect it

> **Scenario RFC0052.15 — Only the terminal rotation state is reported
> non-transient**
> - **Given** §3.3's bounded retry, a rotation failure that is still **within**
>   the budget, and one that has exhausted it
> - **When** each is reported on both transports
> - **Then** all of them carry `503` / `UNAVAILABLE` — RFC0018.3 still holds,
>   and a non-retryable code would tell the client to drop an unacked batch
> - **And** a failure still within the retry budget is classified transient
>   and its message says it is retrying, because §3.3 means a later append
>   genuinely can succeed; no retry hint is carried, since the server
>   schedules no retry of its own
> - **And** only the **terminal** state is classified non-transient, with
>   the message naming that state
> - **And** an ordinary append or fsync I/O failure stays transient, so the
>   reclassification is narrow rather than a blanket change to RFC 0018
>   §3.2's transient class
>
> #794 originally classified every rotation failure as wedged, which was
> correct for **today's** permanent latch and wrong the moment §3.3 makes
> rotation retryable. That half of #794 was split out and is held on
> `hold/794-wedged-classification`; it lands with this RFC, rewritten to the
> terminal-only rule above. The merged half of #794 is the `Status` body
> conformance alone and stays correct under both.

> **Scenario RFC0052.14 — The timer cannot stamp across a concurrent submit**
> - **Given** the reclamation timer firing while ingest submits continuously
> - **When** the timer runs its sequence
> - **Then** no checkpoint is advanced past a frame whose encode had not
>   finished its sink emit, under any interleaving
> - **And** the mark used is the one read after the quiesce under the same
>   exclusion, not one read before either
> - **And** that mark is a turn's own frame offset, never the sync's reported
>   EOF, so a later frame made durable by the same flush but not yet mined
>   (nor acknowledged) is never
>   covered — asserted by a flush whose sync covers two turns and a barrier
>   between them
> - **And** a pre-cut batch whose first record detached a partition mid-batch
>   has its remaining records in the cut, under any interleaving: the
>   quiesce waits for the batch's encode phase, not for a worker to register
>   a publish, and the detached partition's PUT is not waited on
> - **And** a rotation-fired cut performs no store I/O inside the ingest
>   turn: the turn hands the captured cut to the barrier task, and an append
>   admitted after the turn is neither in that cut's checkpoint nor in its
>   snapshot

> **Scenario RFC0052.13 — A tenant without a snapshot pins the floor, and is
> never read as unbounded**
> - **Given** a snapshot consumer exists and one tenant with WAL data has no
>   valid snapshot
> - **When** housekeeping runs
> - **Then** every frame of that tenant survives, segments below its oldest
>   surviving frame that hold only other tenants' covered frames are
>   reclaimed, and the floor is reported as `Pinned` — distinguishable in the
>   API from the no-consumer case, which reclaims by checkpoint alone
> - **And** once a valid snapshot for that tenant is written, the pin lifts
>   and the next pass reclaims what it had held
> - **And** a tenant leaves the ledger only when its last surviving segment
>   is unlinked, so tenant churn cannot leave a permanent pin
> - **And** a segment holding exactly one frame for the pinning tenant
>   survives the pass — a pinned tenant has no horizon, so equality can
>   never unlink its frame — while a later segment holding none of its
>   frames is reclaimed
> - **And** a snapshot listed at startup is used as a horizon only after the
>   snapshots root and its parent have been fsynced in this process; a failed
>   startup fsync **fails startup** rather than discarding snapshots whose
>   frames reclamation may already have removed, so a horizon whose directory
>   entry may not be durable never governs reclamation and no state that
>   only a snapshot could rebuild is thrown away
> - **And** the state is exported (§3.5), so an operator seeing a WAL that
>   will not shrink can tell it is a pinning tenant rather than an
>   unexplained stall

> **Scenario RFC0052.6 — moved to RFC 0053 as RFC0053.1 (backpressure).**
> The number is retained so review-history references resolve; it carries
> no obligation in this RFC.

> **Scenario RFC0052.7 — The WAL's state is exported**
> - **Given** a running node
> - **When** metrics are collected
> - **Then** the WAL's unflushed bytes, on-disk bytes, segment count, **all
>   unreclaimed bytes and the age of the oldest unreclaimed frame** (the
>   bytes being what RFC 0053's bound is taken on, including the
>   post-checkpoint tail), the retain floor with its
>   lag and `Pinned` state, the `cadence_failed` latch, and the
>   rotation-failure state are all present
>   in the exported stream under registry names
> - **And** a run whose checkpoint never advances still reports growing
>   unreclaimed bytes — exporting the below-checkpoint figure instead would
>   read as flat during exactly the outage it exists to show
> - **And** entering the terminal rotation state emits exactly one log event,
>   named from the registry; leaving it is a restart today, which a fresh
>   WAL cannot observe, so no leave event exists until §7's operator verb
>   does, and that verb is where it would be emitted — the refusing state's
>   events are RFC0053.5's

> **Scenario RFC0052.8 — moved to RFC 0053 as RFC0053.2 (unwind keeps the
> records).** Number retained; no obligation here.

> **Scenario RFC0052.9 — moved to RFC 0053 as RFC0053.3 (the cadence
> survives a panic).** Number retained; no obligation here.

> **Scenario RFC0052.17 — The reclaim record is the only startup witness,
> and it is fail-closed**
> - **Given** a WAL root housekeeping has reclaimed from under per-tenant
>   horizons, so `RECLAIM` holds an entry per reclaimed tenant
> - **When** the node restarts with one tenant's snapshot undecodable
> - **Then** recovery halts naming that tenant when the record holds an entry
>   for it, and proceeds with that tenant pinned at its oldest surviving
>   frame when the record holds none
> - **And** a root with no record at all — the pre-RFC layout — opens, gains
>   an empty record durably before its first housekeeping pass, and pins
>   rather than halts
> - **And** a record failing its checksum or version byte fails open as
>   `OpenError::Corrupt` naming the file, and is never read as missing
> - **And** a pass reclaiming under a higher horizon for one tenant leaves
>   every other entry unchanged, and no pass ever lowers an entry
> - **And** a crash injected between the record's rename and the first
>   unlink leaves a record whose entries every restorable snapshot satisfies,
>   so the restart proceeds; a crash injected between the temp write and the
>   rename leaves the previous record intact and the temp truncated by the
>   next pass

> **Scenario RFC0052.10 — No acknowledged record is lost across the whole
> cycle**
> - **Given** a node killed with `SIGKILL` mid-batch while reclamation and
>   rotation retry are both live
> - **When** it restarts and recovery completes
> - **Then** every acknowledged record is present in Parquet, including
>   those whose segments were candidates for reclamation at the moment
>   of the kill
> - **And** recovery itself republishes nothing at or below `max(X, S)`: a
>   kill that follows a
>   failed snapshot write and an advanced checkpoint replays frames at or
>   below the checkpoint into the miner only, never into the record sink —
>   a client retry that wrote a second equivalent frame (RFC0003.2's
>   at-least-once contract) is unchanged by this RFC and is not what this
>   leg counts
> - **And** the same holds when the snapshot write succeeded and the
>   checkpoint write then failed, so a tenant restarts with `S > X`: nothing
>   in `(X, S]` is republished
> - **And** the audit stream is gated the same way: no template event for a
>   frame at or below `X` is forwarded again on replay, and every event the
>   miner regenerates for a frame above `X` is forwarded exactly once, in
>   frame order, through the capture sink, with stored `AuditEvent` frames
>   ignored as a source; the test mines the same frames from the same
>   snapshot with the same injected clock as a reference and asserts the
>   forwarded set equals the reference's events for `(X, tail]`

## 6. Testing strategy

Per `CLAUDE.md` §6.2, mapped to the §5 ids.

- **Checkpoint policy (RFC0052.1)** — unit tests over the barrier with a
  healthy store and with a store that fails a partition write, asserting
  `last_checkpoint()` advances in the first case and is untouched in the
  second; plus a third leg that fails the checkpoint sidecar write and its
  directory fsync with a healthy store, asserting the barrier still reports
  success, the previous mark stays usable and the next pass reclaims only
  under it — the fail-closed branch a partition-write failure cannot reach.
  Two unwind legs use seeded interleaving, as RFC0052.14 does: a test sink
  whose publish panics once the barrier is inside `quiesce_publishes`,
  asserting no install and no stamp and a failed outcome; and a record that
  panics the encode worker mid-batch, asserting the next barrier stamps
  nothing and a restart replays the remainder. The retain path is already
  exercised by the existing skip-the-snapshot tests, so these extend them
  rather than duplicating.
- **Truncation bounds (RFC0052.2)** — an integration test building a WAL
  with several closed segments, a checkpoint above them and a snapshot
  floor deliberately below it, asserting exactly which files survive.
  The floor case is the important one: a test that only checks
  "segments disappear" passes on a bound that ignores the floor.
- **Bounded growth (RFC0052.3)** — the `ourios-bench` soak harness on its
  synthetic clock, asserting the WAL's byte total and segment count stay
  bounded over a long run. This is the one criterion a unit test cannot
  express, because the defect is the *absence* of a periodic call; only
  elapsed cadence reveals it.

  **The harness cannot do this today and has to be extended first.** It builds
  a bare WAL and coordinator whose sampler only flushes the sink and runs
  compaction — there is no snapshot, checkpoint or housekeeping cadence in it
  at all, and its synthetic clock advances record timestamps rather than
  driving a timer. So the extension is part of this criterion's cost, not an
  assumption behind it: the soak loop gains the §3.2 sequence on the synthetic
  clock, which is also the only way to reach a multi-hour backlog in a test.
- **Rotation retry (RFC0052.4, RFC0052.5)** — fault injection at each of
  the five rotation steps (the rename included), once-failing and
  always-failing, asserting
  recovery in the first case and a distinguishable terminal refusal in
  the second, plus `Wal::open` succeeding afterwards in both (the
  temporary-name property) and the temp files being swept. A `proptest` over
  which step fails and how many times keeps the five sites from being tested
  only one way.
- **Pre-existing orphans (RFC0052.11)** — a directory fixture hand-built in
  the shape today's failure leaves, asserting that open still halts with
  `OpenError::Corrupt` and that the error names the file and the
  rotation-remnant shape. A fixture rather than fault injection, because
  the state predates the code under test.
- **Capped passes (RFC0052.12)** — a backlog well past the cap, asserting the
  per-pass unlink count and that repeated passes reach the uncapped end
  state. The concurrent-append half is asserted by timing out an append
  against an uncapped pass in a regression guard rather than by measuring
  wall clock, which would be flaky.

  These **replace** `rfc0008_6_rotation_failure_quiesces_the_wal`, whose
  "even after the underlying condition clears" assertion is the contract
  §3.3 changes. That replacement needs explicit approval (`CLAUDE.md` §6.2)
  and must keep both halves of what the original protected: no ack on an
  incomplete rotation, and a permanent refusal when the fault is
  persistent.
- **Reclaim record (RFC0052.17)** — directory fixtures for the four record
  states (absent, empty, valid with entries, corrupt) crossed with a
  restorable and an undecodable snapshot, asserting halt-or-pin per case and
  the `Corrupt` error's file name; fault injection at the two crash points
  (before rename, before first unlink) through the same hook the rotation
  tests use; and a two-pass monotonicity leg reading the record back.
- **Temp sweep (RFC0052.16)** — a directory fixture holding all five file
  kinds, asserting exactly one is removed. A fixture rather than a live
  rotation, because the point is the *selector*, and the dangerous cases
  (`CHECKPOINT.tmp`, `*.snap.tmp`) are produced by other subsystems.
- **Reclassification (RFC0052.15)** — the classifier unit tests held on
  `hold/794-wedged-classification`, rewritten to the terminal-only rule and
  extended to assert the *narrowness*: an ordinary append or fsync I/O
  failure and a still-retrying rotation stay in the transient class while
  only the terminal rotation state leaves it; no arm carries a retry hint. Without that third assertion the test
  passes on a blanket removal of `Retry-After`, which would contradict
  RFC 0018 §3.2 rather than amend it.
- **Floor pinning (RFC0052.13)** — unit tests over the three `RetainFloor`
  cases, asserting a tenant without a snapshot pins the minimum at its oldest
  surviving frame, that segments below it holding only covered frames are
  reclaimed, that the pin lifts when its snapshot lands, and that the pinned
  case is not expressible as the no-consumer case. A churn leg writes one
  tenant once, snapshots it, and asserts it leaves the ledger when its last
  segment is unlinked. The startup leg uses a snapshots-root fixture whose
  directory fsync is made to fail (the file-in-place-of-directory technique
  from the rotation tests, since read-only permissions do not bind under
  root), asserting every tenant is pinned rather than any listed horizon
  used.
- **Timer exclusion (RFC0052.14)** — a seeded-interleaving test rather than a
  timing one: the window is narrow, and a wall-clock test that happens to pass
  proves nothing. Failing that, hold the timer artificially between the
  quiesce and the *end of the cut* while driving ingest, assert the submit
  blocks until the cut is captured and proceeds afterwards, and assert the
  records admitted after the cut are in neither the checkpoint nor the
  snapshot that barrier writes — the lock is never held across store I/O.
- **Telemetry (RFC0052.7)** — the in-memory metric exporter pattern
  already used for the ingest/sink instruments, asserting every name is
  in the exported stream; a runtime tracing assertion that drives each
  transition — terminal entry on the append path, floor pinned and lifted,
  latch set — and verifies event count, name and state, since the
  live-check validates definitions rather than emission; plus a `weaver
  registry live-check` pass over the new log events, since an event the
  tests never emit is an event the live-check never checks (that is how
  #795's un-named event passed CI).
- **No loss (RFC0052.10)** — extends the existing `SIGKILL`
  crash-recovery test rather than adding a parallel one, with
  reclamation configured on a short cadence so the kill lands in the
  regime this RFC introduces. The #791 regression tests (refuse-then-resume
  with no append) move with the bound to RFC 0053.

Maturity, per `docs/rfcs/README.md`: `green` is every §5 criterion passing
with the unit, property and corpus tests green — none of the live scenarios
is optional. RFC0052.3's recorded soak run and RFC0052.10 are *additional*
incident-regression gates this RFC requires before `validated`, since those
two demonstrate the incident cannot recur rather than that a unit behaves;
they are not substitutes for the rest.

## 7. Open questions

- [ ] Whether §3.3's count budget (three consecutive attempts) should become
      a time budget with backoff. A count is simpler to test; a time budget
      degrades better on a disk that is slow rather than broken.
- [ ] Whether the terminal rotation state also gets an operator verb to
      clear it without a restart, or whether a restart remains the
      documented recovery for a persistent durability fault.
- [ ] Whether `disk_bytes` should be computed per pass or cached.
      `WalMetrics` documents the directory walk as best-effort; exporting
      it on every collection may not be free on a WAL with many
      segments — though after §3.2 there should be far fewer.
- [ ] `max_unlinks_per_pass`'s default, which trades first-pass stall against
      how long a large backlog takes to clear.
- [ ] Whether the actionable halt for a pre-existing rotation remnant (§3.3)
      deserves a WAL verb to remove the file, or stays a documented manual
      step. The decision must remain a human's either way — no shape-based
      heuristic can tell rotation debris from real corruption — so this is
      about ergonomics, not safety.
- [ ] Whether a stale tenant floor blocking reclamation indefinitely should
      itself escalate (a second, louder state) or stay a visible metric an
      operator alerts on. §3.5 makes it visible; it does not decide.
- [ ] Whether to persist a published high-water mark with every flush, so
      that frames published between two barriers are not re-published on
      replay. Today that is RFC 0008/0014's at-least-once contract above the
      checkpoint; this RFC narrows nothing there and bounds nothing there.

## 8. References

- Issue #791 — the incident: ingest wedged at `503`, no self-recovery,
  no observability.
- Issue #793 — `checkpoint`/`housekeeping` called only from tests.
- Issue #796 — a publish unwind drops drained batches and the snapshot
  guard reads empty buffers as fully drained; specified by RFC 0053 §3.2.
- PR #794 — the OTLP `Status` body on ingest rejections (conformance only;
  the wedged classification was split out to
  `hold/794-wedged-classification` and lands with §3.3).
- PR #795 — counts a cadence panic (makes a dead sweep alertable;
  deliberately still stops, pending RFC0053.2).
- RFC 0053 — WAL backpressure and unwind safety; the stage that depends on
  this one.
- RFC 0008 §6.5 (rotation), §6.6 (recovery horizon), §6.7 (checkpoint
  and housekeeping), §6.8 (counters), §6.9 (tunables) — the mechanism
  this RFC supplies the policy for; §6.5's durable-entry-first rule and
  Scenario RFC0008.6's permanent refusal are **superseded** by §3.3 and
  RFC0052.4/.5.
- RFC 0018 §3.2 (retryable error mapping) — **amended** by §3.3: its transient
  class lists "post-rotation quiesce", which #791 disproved. RFC0018.3 stays
  satisfied, since the status is unchanged; only the class and the optional
  `Retry-After` move.
- RFC 0001 §6.9 — the miner snapshot high-water mark, and the hazard-#5
  retain rule that makes it the truncation floor — **amended** by §3.2:
  horizons are per tenant (each tenant's own last folded frame), the retain
  rule is tenant-aware and strict at the horizon, and the stale-gap
  detector's argument is restated on that basis.
- RFC 0014 — the record sink and its flush triggers.
- `CLAUDE.md` §3.4 (WAL-before-ack), §3.6 (object storage is the source
  of truth), §6.3 (observability of ourselves).
- `docs/hazards.md` #3 (WAL durability versus latency), #4 (small
  files), #5 (template schema evolution across deploys — the retain
  rule).
