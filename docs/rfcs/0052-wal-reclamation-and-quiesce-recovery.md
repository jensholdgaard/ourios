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
> that was never written, **amends RFC 0018 §3.2** (whose transient class
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
exclusively for its whole sequence. It is strictly wider than the miner lock and
strictly narrower than the gate, so it does not change ingest ordering, and it
is the smallest thing that closes the window. Rotation needs no change: it
already runs inside an ingest's own span.

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
drain-and-publish step therefore takes the exclusion in shared mode and the
timer takes it exclusively: no sweep starts inside the barrier, and one in
flight completes before it. That subsumes `quiesce_publishes` rather than
relying on it.

The mark is then read inside that turn and after the quiesce — `last_durable()`
at that point, the offset the receiver's acks are gated on — so no frame above
it can still be in flight. Reading it before the quiesce, or outside the turn,
reopens the window from the other side.

**And the stored mark must not over-cover.** The group-commit `sync` reports
the WAL's EOF, and `CommitCoordinator::flush` captures `covered_seq` before it
locks the journal, so waiter A's outcome can carry an offset that includes
frame B, appended later by a turn that has not yet run — while B is acked on
the same outcome. If A stored that EOF as `last_durable`, a barrier between
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

The cost is explicit: ingest stalls for the barrier's duration once per
`housekeeping_secs`, the same stall rotation already imposes, now on a timer.
`housekeeping_secs` is therefore the knob trading reclamation latency against
that stall, and §3.7's per-pass cap bounds the sequence's second half.

See §3.7 for the ownership path and the exact signatures, which the barrier
does not have today:

```text
if barrier_succeeded and high_water is Some(mark):
    journal.checkpoint(mark)            // §6.7, monotonic; Result
      on Err  -> log, do NOT advance, retain every segment
```

Three properties come free from existing code and must not be
reimplemented:

- **Monotonicity and idempotence** — `checkpoint` rejects a regression
  and no-ops on a re-assert, so a repeated stamp at the same mark is
  safe.
- **Fail-closed** — on a sidecar write error the in-memory checkpoint is
  not advanced, so the WAL conservatively keeps every segment rather
  than risk a post-crash duplicate.
- **Skip-on-retain** — when either sink retains anything,
  `flush_then_snapshot` returns `false` and no checkpoint is attempted.
  The WAL keeps the frames and the next start re-mines them.

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
every housekeeping_secs:
    with_barrier_exclusion:                // a NEW pipeline lock, not the miner
                                           // lock and not the gate — see §3.1
        quiesce_encodes()                  // the barrier's prologue
        mark = last_durable()              // read AFTER the quiesce, INSIDE the turn
        if barrier_succeeded(mark) and mark is Some(m):   // §3.1, append-free
            journal.checkpoint(m)
    journal.housekeeping(retain_floor(), max_unlinks_per_pass)  // Result
      on Err -> log; the next pass retries (nothing was unlinked past the bound)
```

**The timer has the age sweep's lifecycle, stated so it cannot outlive the
handle.** It is owned by the receiver, signalled by the same shutdown watch
the sweep already observes, and joined **before** the final flush and
snapshot, in the order `ReceiverHandle::shutdown` already joins the sweep; it
holds no pipeline or journal handle after the join. A timer that was not
joined there could race the shutdown reclamation or keep the WAL alive past
the handle, which is why the ordering is part of the design rather than of
the implementation.

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

That detector's no-false-positive property *depends* on the minimum, so the
earlier draft's "latest" wording would have broken stale-gap detection as
well as losing data.

**Reclamation is skipped entirely while any tenant with WAL data has no
valid snapshot.** There is no horizon to include in the minimum, so the
minimum is undefined, and guessing it either way is unsafe. `None` is passed
only where no snapshot consumer exists at all.

Deriving `Incomplete` needs a ledger the recovery model does not keep:
`recover` returns entries only for tenants that *have* a `.snap`, and nothing
records which tenants the WAL holds, so an empty or partial snapshot set is
indistinguishable from "no consumer" and would read as `None`. The floor
derivation therefore keeps a per-tenant ledger of its own — tenants seen in
WAL frames, from replay and from every live append, against tenants with a
durable, decodable snapshot — and whenever a snapshot consumer is configured,
a tenant present in the first set and absent from the second yields
`Incomplete`. `None` comes only from the absence of a consumer, never from
the absence of snapshots.

**And the floor may only be derived from snapshots known to be durable.**
`snapshot_store::write` renames the new snapshot into place *before* its
parent-directory fsync, so a failure there leaves the file visible to this
process while the write returned an error. A floor derived by listing `.snap`
files would then trust a horizon that may not survive a crash, and housekeeping
would unlink segments on the strength of it — losing exactly the frames the
floor exists to retain.

So the derivation uses the in-memory record of snapshots whose write returned
`Ok`, not the directory contents. A tenant whose snapshot write failed counts as
`Incomplete`, which per the rule above reclaims nothing — conservative, and
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
snapshots root is fsynced once, which makes every entry the listing saw
durable — the one property the listing lacks. If that fsync fails the floor
is `Incomplete` and nothing is reclaimed until a later snapshot write
succeeds. One directory fsync rather than a manifest, because a manifest
would need the same fsync to be trustworthy itself. Durability is necessary
and not sufficient: `snapshot_store::load_all` returns raw bytes, and a
horizon is admitted only from a snapshot that decodes and restores. A listed
file that fails either counts as that tenant having no snapshot — hence
`Incomplete` — rather than as a horizon, so a durable-but-invalid file can
never authorise reclamation.

The floor is also the reason §3.1 can tolerate a failed snapshot write.
`housekeeping` truncates below `min(checkpoint, floor)`, so a stale floor
makes truncation conservative — it retains frames a snapshot has not
captured, which degrades the next start to a fuller replay and never to loss
(hazard #5's retain rule, RFC 0001 §6.9).

Housekeeping takes the WAL's single-writer position, so it runs on the
same ownership path as rotation rather than concurrently with it.

### 3.3 Rotation failure becomes recoverable, under a bounded retry

A quiesce must stop being permanent. The four sites are not equally
retryable, and the design treats them by what they leave behind:

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
after the final failed retry a partial or header-only orphan can still be
sitting there, which `Wal::open` may select as the newest segment. Either
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

**That retry draws on the same budget and reaches the same terminal state.**
Each failed discharge of `dir_fsync_pending` in `sync` consumes one unit of
the rotation retry budget, exactly as a failed `rotate` does, and exhausting
it enters the terminal state whichever operation gets there. Both
`AppendError` and `SyncError` carry a typed terminal variant, and
`IngestFailure::classify` maps both under the terminal-only rule — so a
persistent parent-directory fsync failure is not left as an ordinary
`WalSync` that RFC0052.5 and RFC0052.15 could never observe.

Installing *after* the fsync, as the draft implied, leaves the renamed file
owned by nobody: `rotate` would still point at the old segment while a complete
`.wal` sits beside it, which is the orphan case all over again.

A rotation that fails *before the rename* therefore never leaves a file that
looks like a segment. This amends RFC 0008 §6.5's on-disk create sequence and
is the one on-disk behaviour change in this RFC; it needs no format or schema
change, because the temporary name never becomes a segment.

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

- the **in-flight** partial — the one the current rotation attempt owns — is
  skipped, identified by name rather than by age, since a slow fsync must not
  make a live file look stale;
- the unlinks are followed by a **parent-directory fsync**, as
  `housekeeping`'s segment unlinks already are, so a crash cannot resurrect
  swept debris.

The step runs **inside** the same per-pass cap as segment unlinking (§3.7) and
counts against it, because it takes the same single-writer position — a backlog
of stale partials would otherwise make a "bounded" pass do unbounded directory
work and break RFC0052.12. An unlink failure is logged and retried on the next
pass like any other.

Two more properties of the sweep are stated because the current
`housekeeping` has neither. **It runs on every pass**, regardless of the
checkpoint precondition: today `housekeeping` returns at once when no
checkpoint exists, and a rotation that fails before the first checkpoint
would otherwise leave partials that every later pass skips; only the
segment-unlink portion is gated on a checkpoint. **Its error path still
makes completed removals durable**: when a pass fails part-way — a header
read, a stat, a later unlink — the parent-directory fsync covers every unlink
that already succeeded before the error is returned, and
`HousekeepingProgress` reports the partial count. Today's single fsync after
the complete scan would let a crash resurrect a partial the pass had already
removed.

**Pre-existing `*.wal` orphans need their own answer, and neither changing
future rotations nor guessing at open is it.** A node wedged before this RFC
lands may already hold a segment whose header fsync failed, so its header
bytes may be partial — `Wal::open` validates every `*.wal` and can return
`OpenError::Corrupt` on it. Claiming these are "real zero-frame segments" was
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

Retry is attempted on the next `append`, not in a background loop: the
WAL is single-writer and has no task of its own, and a rotation is only
needed when there is something to write. `quiesced` becomes a typed state
carrying the attempt count and the first underlying error, so the
distinction between "retrying" and "given up" is representable rather
than a bool. After a bounded number of consecutive failed retries the
WAL enters a terminal state that is still reported distinctly (see §3.5)
and still refuses appends — a disk that has failed the same fsync a
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
it keeps `Retry-After`. Only once the budget is exhausted is the node in a state
no delay fixes, and only there is `Retry-After` — already optional in §3.2 —
withheld, with the message naming the state. RFC0052.15 pins both halves.

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
- **the retain floor and its lag**, including whether it is
  `RetainFloor::Incomplete` — without which RFC0052.13's "an operator can tell a
  lagging tenant from an unexplained stall" is not achievable, since that is
  the case where reclamation stops with a healthy store;
- **the rotation-failure state**: retrying, with its attempt count, versus
  terminal.

A log event is emitted on entering and on leaving a refusing state. Names come
from the shared `ourios-semconv` registry in one bump, not hand-written, and
`error.type` continues to carry the failure class on existing counters rather
than spawning per-error metrics.

### 3.6 Requeue on unwind — moved to RFC 0053

The duplicate-versus-lose decision, the consuming-call ownership rule and
the age-sweep survival that follows from them are RFC 0053 §3.2. They are
deferred rather than dropped: the #795 stop-on-panic behaviour this RFC
leaves in place is the safe interim, and it is what RFC 0053 replaces. The
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
  enum ReclaimError { Checkpoint(..), Housekeeping(..) }   // object-safe, one type

  /// Why a floor is or is not available — `Option` cannot carry this.
  enum RetainFloor {
      None,                  // no snapshot consumer exists: checkpoint alone governs
      Min(WalOffset),        // the minimum over every tenant's horizon
      Incomplete,            // a consumer exists but some tenant has no snapshot
  }

  fn append_batch(&mut self, payload: &[u8]) -> Result<WalOffset, AppendError>
                                                           // returns the offset
                                                           // Wal::append already
                                                           // produces (§3.1's mark)
  fn checkpoint(&mut self, durable_to: WalOffset) -> Result<(), ReclaimError>
  fn housekeeping(&mut self, floor: RetainFloor, max_unlinks: usize)
      -> Result<HousekeepingProgress, ReclaimError>
  fn reclaim_state(&self) -> ReclaimState                  // §3.5's export surface
  ```

  **The own-frame mark needs plumbing, stated so it cannot be skipped.**
  `Journal::append_batch` discards the `WalOffset` `Wal::append` returns
  today, and `CommitOutcome` exposes only the group-sync result. So
  `append_batch` returns the offset, the coordinator records each waiter's
  own offset by sequence at append time, `CommitOutcome` carries it beside
  the durable EOF, and every test double returns a monotonically increasing
  offset per append. An implementation that kept storing the EOF fails
  RFC0052.14's two-turn flush, which is what that criterion is for.

  `RetainFloor` exists because `Option<WalOffset>` cannot distinguish the two
  cases §3.2 requires: `None` means no snapshot consumer exists and the
  checkpoint alone governs, which is *safe to reclaim*, while `Incomplete`
  means a consumer exists but a tenant has no valid snapshot, which must
  reclaim **nothing**. An `Option` makes the unsafe reading — treating
  "incomplete" as "unbounded" — expressible, and a signature that can express
  the data-losing case is the wrong signature.

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
  retain floor with its lag and `Incomplete` state, and the rotation-failure
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
  torn bytes. Recovery's heal step therefore ends by calling
  `Wal::remeasure_unreclaimed()` — a concrete method, since recovery holds
  the WAL before it is boxed — which sets the figure to the sum over every
  surviving `*.wal` of file size less the segment header, in frame bytes.
  The coordinator is constructed after recovery (the ordering below), so no
  append can precede the seed, and a restart mid-outage resumes from the
  true backlog rather than from zero. `unflushed_bytes`
  stays its own method: the
  group-commit coordinator reads it per batch and must not allocate a
  snapshot struct on that path.

  On the trait rather than via a downcast, because RFC0052.1 and RFC0052.2
  need test doubles that can observe reclamation.
- **The barrier reaches them through the coordinator**, which already owns the
  journal mutex, rather than taking a second handle to the same WAL. A second
  handle would put two owners on a single-writer resource, which is the one
  thing RFC 0008 §3.1 forbids.
- **`Option<WalOffset>` skips, and the post-recovery call is an explicit
  exception.** `None` means no high-water mark is known, and there is then
  nothing to declare reclaimable. `None` is *not* the same as "post-recovery",
  which the earlier draft conflated: `serve` passes `report.max_delivered`,
  which is `Some` whenever replay delivered a frame, and it does so **before
  the commit coordinator exists**, so there is no journal owner to checkpoint
  through. That call therefore deliberately advances no checkpoint; the first
  timer pass after the coordinator is built does it instead, from the same
  mark or a later one. §3.1's "behind the barrier and nowhere else" holds —
  this is a caller that has the mark but not yet the owner, not a second
  checkpoint site.
- **Errors are fail-closed and never swallowed.** A failed `checkpoint` logs
  and leaves the in-memory checkpoint unadvanced, which is already
  `Wal::checkpoint`'s own behaviour; the WAL then keeps every segment. A
  failed `housekeeping` logs and the next pass retries, since nothing was
  unlinked past the bound. Neither failure fails the barrier: the data is in
  object storage either way, and refusing to ack over a reclamation error
  would turn a disk-space problem into an availability one.

**One consequence worth naming.** Housekeeping takes the single-writer
position, so every append stalls for the duration of the directory walk plus
the unlinks. Running it on the blocking pool does **not** help: the stall is
caused by holding the journal mutex, not by occupying a runtime worker, so
moving the work to another thread while still holding the lock bounds nothing.

The pass is therefore **capped on the work, not only on the removals**. Capping
unlinks alone would not bound the stall: the current path enumerates and sorts
every `*.wal` and inspects each candidate before unlinking any, so a
1,113-segment backlog does 1,113 header reads under the mutex however few files
it then removes. The cap therefore bounds **candidates inspected** as well as
segments removed, stopping the walk once it has found that many — the oldest are
encountered first, which is the order reclamation wants anyway. What the cap
does **not** bound is the listing itself: enumerating and sorting the names is
O(n) in directory entries and stays under the mutex, but it is one `readdir`
with no per-file I/O — a fraction of a millisecond at the incident's 1,113
entries, against 1,113 header reads and fsyncs. The guarantee is therefore
bounded *per-file* work, and RFC0052.12 is worded that way; a bounded
oldest-first iterator would remove the sort but not the `readdir`, so it is
not worth a second code path. The next timer tick continues where it left off, which
needs no cursor because the bound is recomputed each time and the oldest
eligible segments are always the ones taken first. That matters most on the
*first* pass of a node that has never reclaimed — the incident node held 1,113
segments, and an uncapped pass would have stalled ingest for as long as 1,113
unlinks take. In steady state, after §3.2 is running, a pass has a handful of
segments to consider and the cap never binds.

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
> - **Then** only segments wholly below `min(checkpoint, min over tenant
>   horizons)` are unlinked, the current append segment survives, and the
>   WAL's segment count falls
> - **And** a frame above the *lagging* tenant's horizon is still present
>   after the pass, so a restart re-mines it rather than losing that
>   tenant's miner state
> - **And** when any tenant with WAL data has no valid snapshot, the pass
>   reclaims nothing at all

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
> - **And** while the floor is `Incomplete` retention is unbounded by design
>   and the state is visible (RFC0052.13); a healthy store alone does not
>   bound the WAL, a complete floor does
> - **And** an offered rate above that capacity grows the WAL by
>   construction until RFC 0053's admission bound exists; this criterion
>   claims no bound there

> **Scenario RFC0052.4 — A transient rotation failure recovers without a
> restart**
> - **Given** a WAL whose rotation fails once with a transient I/O error
> - **When** a later `append` arrives after the condition clears
> - **Then** rotation is retried, succeeds, and the append is accepted
>   and acked
> - **And** when the failure was **before the rename**, no file a subsequent
>   `Wal::open` would select as a segment remains from the failed attempt —
>   asserted by opening the WAL again, not by inspecting the directory, since
>   the temporary name is an implementation detail
> - **And** when the failure was the **post-rename directory fsync**, the
>   installed segment is selected by a subsequent open — it is complete and
>   valid — and the first `sync` after open discharges its pending directory
>   fsync
> - **And** the temporary files left by the failed attempts are gone after a
>   housekeeping pass, so a persistently retrying node cannot fill its disk
>   with retry debris

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

> **Scenario RFC0052.11 — A node already wedged before this RFC still opens**
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
> - **Then** it reads at most the cap's worth of segment headers, unlinks at
>   most the cap, and returns; the only O(n) step is the name listing, which
>   does no per-file I/O — so an append taken concurrently waits for that
>   bounded work rather than the whole backlog
> - **And** successive passes drain the backlog to the same end state an
>   uncapped pass would reach
> - **And** a backlog of stale *temporary* files is also bounded by the same
>   cap, so temp sweeping cannot make a "bounded" pass do unbounded work

> **Scenario RFC0052.16 — The temp sweep touches only files of the reserved
> partial shape**
> - **Given** a WAL root holding a `CHECKPOINT.tmp`, a snapshots directory
>   holding a `*.snap.tmp`, a stale `<uuid>.wal.partial`, and the in-flight
>   partial of a rotation in progress
> - **When** a housekeeping pass runs
> - **Then** only the stale partial is unlinked: the checkpoint temp, the
>   snapshot temp and the in-flight partial all survive
> - **And** the unlink is followed by a parent-directory fsync, so a crash
>   cannot resurrect it

> **Scenario RFC0052.15 — Only the terminal rotation state is reported
> non-transient**
> - **Given** §3.3's bounded retry, a rotation failure that is still **within**
>   the budget, and one that has exhausted it
> - **When** each is reported on both transports
> - **Then** all of them carry `503` / `UNAVAILABLE` — RFC0018.3 still holds,
>   and a non-retryable code would tell the client to drop an unacked batch
> - **And** a failure still within the retry budget carries `Retry-After`,
>   because §3.3 means a later append genuinely can succeed
> - **And** only the **terminal** state omits it, with the message naming that
>   state
> - **And** an ordinary append or fsync I/O failure still carries
>   `Retry-After`, so the reclassification is narrow rather than a blanket
>   change to RFC 0018 §3.2's transient class
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
>   EOF, so a later frame acked on the same flush but not yet mined is never
>   covered — asserted by a flush whose sync covers two turns and a barrier
>   between them

> **Scenario RFC0052.13 — An incomplete tenant floor cannot be read as
> unbounded**
> - **Given** a snapshot consumer exists and one tenant with WAL data has no
>   valid snapshot
> - **When** housekeeping runs
> - **Then** nothing is reclaimed, and this is distinguishable in the API from
>   the no-consumer case, which reclaims by checkpoint alone
> - **And** a snapshot listed at startup is used as a horizon only after the
>   snapshots root has been fsynced in this process; a failed startup fsync
>   yields `Incomplete`, so a horizon whose directory entry may not be durable
>   never governs reclamation
> - **And** the state is exported (§3.5), so an operator seeing a WAL that
>   will not shrink can tell it is a lagging tenant rather than an
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
>   lag and `Incomplete` state, and the rotation-failure state are all present
>   in the exported stream under registry names
> - **And** a run whose checkpoint never advances still reports growing
>   unreclaimed bytes — exporting the below-checkpoint figure instead would
>   read as flat during exactly the outage it exists to show
> - **And** entering and leaving the terminal rotation state each emit
>   exactly one log event, named from the registry — the refusing state's
>   events are RFC0053.5's

> **Scenario RFC0052.8 — moved to RFC 0053 as RFC0053.2 (unwind keeps the
> records).** Number retained; no obligation here.

> **Scenario RFC0052.9 — moved to RFC 0053 as RFC0053.3 (the cadence
> survives a panic).** Number retained; no obligation here.

> **Scenario RFC0052.10 — No acknowledged record is lost across the whole
> cycle**
> - **Given** a node killed with `SIGKILL` mid-batch while reclamation and
>   rotation retry are both live
> - **When** it restarts and recovery completes
> - **Then** every acknowledged record is present in Parquet, including
>   those whose segments were candidates for reclamation at the moment
>   of the kill
> - **And** no acknowledged record is present twice: a kill that follows a
>   failed snapshot write and an advanced checkpoint replays frames at or
>   below the checkpoint into the miner only, never into the record sink

## 6. Testing strategy

Per `CLAUDE.md` §6.2, mapped to the §5 ids.

- **Checkpoint policy (RFC0052.1)** — unit tests over the barrier with a
  healthy store and with a store that fails a partition write, asserting
  `last_checkpoint()` advances in the first case and is untouched in the
  second. The retain path is already exercised by the existing
  skip-the-snapshot tests, so these extend them rather than duplicating.
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
- **Temp sweep (RFC0052.16)** — a directory fixture holding all four file
  kinds, asserting exactly one is removed. A fixture rather than a live
  rotation, because the point is the *selector*, and the dangerous cases
  (`CHECKPOINT.tmp`, `*.snap.tmp`) are produced by other subsystems.
- **Reclassification (RFC0052.15)** — the classifier unit tests held on
  `hold/794-wedged-classification`, rewritten to the terminal-only rule and
  extended to assert the *narrowness*: an ordinary append or fsync I/O
  failure and a still-retrying rotation keep `Retry-After` while only the
  terminal rotation state does not. Without that third assertion the test
  passes on a blanket removal of `Retry-After`, which would contradict
  RFC 0018 §3.2 rather than amend it.
- **Floor completeness (RFC0052.13)** — unit tests over the three
  `RetainFloor` cases, asserting the incomplete case reclaims nothing and is
  not expressible as the no-consumer case. The type makes the unsafe reading
  unrepresentable, so the test mostly guards the *derivation* of the floor
  from per-tenant snapshots rather than `housekeeping` itself. The startup
  leg uses a snapshots-root fixture whose directory fsync is made to fail
  (the file-in-place-of-directory technique from the rotation tests, since
  read-only permissions do not bind under root), asserting the derived floor
  is `Incomplete` rather than the listed horizon.
- **Timer exclusion (RFC0052.14)** — a seeded-interleaving test rather than a
  timing one: the window is narrow, and a wall-clock test that happens to pass
  proves nothing. Failing that, hold the timer artificially between quiesce and
  stamp while driving ingest, and assert the submit blocks rather than
  proceeding.
- **Telemetry (RFC0052.7)** — the in-memory metric exporter pattern
  already used for the ingest/sink instruments, asserting every name is
  in the exported stream; plus a `weaver registry live-check` pass over
  the new log events, since an event the tests never emit is an event
  the live-check never checks (that is how #795's un-named event passed
  CI).
- **No loss (RFC0052.10)** — extends the existing `SIGKILL`
  crash-recovery test rather than adding a parallel one, with
  reclamation configured on a short cadence so the kill lands in the
  regime this RFC introduces. The #791 regression tests (refuse-then-resume
  with no append) move with the bound to RFC 0053.

Validation: this RFC reaches `validated` when RFC0052.3's soak run is
recorded and RFC0052.10 passes in CI, since those two are the ones that
demonstrate the incident cannot recur rather than that a unit behaves.

## 7. Open questions

- [ ] The retry budget in §3.3. A fixed count, or a time budget with
      backoff? A count is simpler to test; a time budget degrades better
      on a disk that is slow rather than broken.
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
  this RFC supplies the policy for.
- RFC 0018 §3.2 (retryable error mapping) — **amended** by §3.3: its transient
  class lists "post-rotation quiesce", which #791 disproved. RFC0018.3 stays
  satisfied, since the status is unchanged; only the class and the optional
  `Retry-After` move.
- RFC 0001 §6.9 — the miner snapshot high-water mark, and the hazard-#5
  retain rule that makes it the truncation floor.
- RFC 0014 — the record sink and its flush triggers.
- `CLAUDE.md` §3.4 (WAL-before-ack), §3.6 (object storage is the source
  of truth), §6.3 (observability of ourselves).
- `docs/hazards.md` #3 (WAL durability versus latency), #4 (small
  files), #5 (template schema evolution across deploys — the retain
  rule).
