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

> **Status note.** `drafted`. Motivated by a production incident
> (issue #791) and the three defects found tracing it (#791, #793,
> #796). Amends RFC 0008 §6.5 and §6.7 with the *policy* those
> sections left to a caller that was never written, and adds the
> ingest-rejection contract and WAL telemetry the incident showed are
> missing. Touches `CLAUDE.md` §3.4 throughout, which is why it is an
> RFC and not four patches.
>
> **Scope note for reviewers.** Successive review rounds have found most
> defects at the *seams* between the four parts — backpressure crossed with
> checkpointing, unwind crossed with the snapshot barrier, reclamation crossed
> with the tenant floor. That is an argument that the scope is wide, and it is
> deliberate: those seams are where §3.4 is actually at risk, and splitting
> them into four documents would put each seam outside every document. If the
> maintainer would rather land this as one spec RFC plus per-part
> implementation RFCs, §§3.1–3.2 (reclamation) and §3.3 (rotation) are the
> natural first split, since §3.4 depends on both.

## 1. Summary

The WAL has never reclaimed a segment in any deployment: `checkpoint`
and `housekeeping` are called only from tests, so `wal_housekeeping_secs`
times nothing and the WAL grows monotonically for the life of the volume.
When the volume fills, rotation fails and sets a `quiesced` latch that no
code clears, so every subsequent write is refused until the process is
restarted — and the refusal carries no log, no metric, and (until #794)
no reason. This RFC commits to: advancing the checkpoint at the existing
publication barrier and running housekeeping on the existing timer;
making a rotation failure recoverable in place under a bounded retry
rather than permanent; requeueing drained batches when a publish unwinds;
declaring an explicit, configurable local backpressure limit with a
stated rejection instead of an implicit one; and exporting the WAL state
an operator needs to see any of this happening.

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

### 2.2 Three defects, one question

Tracing it turned up three separate defects that are all the same
question seen from different sides — *when is it safe to declare WAL
frames reclaimable, and what happens when we cannot?*

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

**#796 — a publish unwind loses drained batches.** `drain_aged` removes
batches from the sink buffers and `write_ordered` consumes them, so a
panic inside the publish drops them: no longer buffered, never written.
`flush_then_snapshot` then reads the empty buffers as "fully drained"
and can stamp a WAL high-water mark over frames that never reached the
store. The non-panic paths requeue correctly; only the unwind does not.

### 2.3 Why at this layer

Each of these could be patched locally, and two of them were (#794 makes
the rejection legible, #795 makes a dead cadence alertable). Neither
patch could touch the behaviour, because all three decisions turn on the
same invariant: §3.4 says an acknowledged record must not be lost, and
every candidate fix either declares frames reclaimable (checkpoint,
truncation) or declares them unnecessary to keep (stamping, requeueing)
or decides whether to keep accepting them at all (backpressure,
quiesce recovery). Getting any one of them wrong loses acknowledged
data. That is the definition of an RFC-level change.

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
change, and from shutdown. If reclamation depended on that alone, §3.4's
backpressure would deadlock: once the bound rejects every append, no rotation
happens, so the checkpoint cannot advance, so the bound never clears — a
second route to exactly the wedge this RFC exists to remove. The age sweep is
not a substitute: it deliberately takes no snapshot and so establishes nothing
about durability.

The barrier therefore also runs on §3.2's timer, which is append-independent
by construction. One predicate, three callers (rotation, shutdown, timer) —
and the timer caller carries the same prologue the other two already do:
`flush_then_snapshot` does **not** quiesce encode submissions itself, its
callers do it first, so a timer caller that skipped `quiesce_encodes()` would
stamp across in-flight encodes (the barrier's own class-1 case) and lose
exactly what it is meant to protect. "One predicate, three callers" means the
whole sequence, not the inner function.

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
truncation floor below is what keeps that safe.

### 3.2 Housekeeping on the timer that already exists

`WalConfig::housekeeping_secs` (default 60) is set in every test and
read by nothing. RFC 0008 §6.7 says "the timer lives in the caller".
The caller is the receiver role, on its own interval:

```text
every housekeeping_secs:
    quiesce_encodes()                                          // the barrier's prologue
    if barrier_succeeded and high_water is Some(mark):          // §3.1, append-free
        journal.checkpoint(mark)
    journal.housekeeping(retain_floor(), max_unlinks_per_pass)  // Result
      on Err -> log; the next pass retries (nothing was unlinked past the bound)
```

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

| Site | Failure | State left behind | Retryable |
|---|---|---|---|
| closing-segment `fdatasync` | `sync_file_data` | nothing new; current segment unchanged | yes, directly |
| create fresh segment | `create_fresh_segment` | possibly a partial file | yes, after unlinking the partial |
| fresh-segment header `fsync` | `sync_file_data` | a fresh segment with a possibly-torn header | yes, after unlinking it |
| parent-dir `fsync` | `sync_parent_dir` | a fresh segment with a durable header, no durable entry | yes, after unlinking it |

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
  durable. Rotation is not complete until that fsync returns, and no frame is
  acked in a segment whose entry is not durable — the same obligation
  `dir_fsync_pending` already carries for the segment `open` creates.

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
per retry, consuming the same disk §3.4's bound exists to protect. So
housekeeping gains a second, explicit step: unlink temporary files that are
not the in-flight one. The bounded retry budget caps how many can exist
between passes, and an unlink failure is logged and retried on the next pass
like any other.

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
adds is that the halt is *actionable* — the error names the file and says it is
the known rotation-failure remnant shape, so an operator can remove it
deliberately. Whether that deserves a WAL verb rather than a documented manual
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

### 3.4 Backpressure becomes explicit

Today the only limit on local accumulation is the volume. That is an
implicit limit with an undefined failure mode, which is what turned an
outage into a wedge. The WAL gains a declared local bound, and crossing it is
a *stated* rejection, specified as a transport contract rather than gestured
at: `ReceiveError` gains a `WalBackpressure` variant carrying the limit that
was hit and the measurement that crossed it; `IngestFailure::classify` maps it
to a new `Backpressure` outcome, which that exhaustive match then forces both
transports to handle; both render `503` / `UNAVAILABLE` with the limit named
in the `Status` message and a `Retry-After` derived from the limit rather than
#794's fixed second. `503` because the batch was not acked — the same
reasoning as `Wedged` — and a distinct outcome rather than reusing
`Unavailable` because the remedy differs: waiting genuinely helps here, which
is what `Retry-After` is for.

**The bound is over ALL unreclaimed bytes and the age of the oldest
unreclaimed frame — not over bytes retained below the checkpoint.** During
the outage this is meant to bound, the checkpoint is precisely what stops
advancing, so new frames pile up *above* it: a below-checkpoint limit would
measure the one quantity that is not growing and never fire. "Unreclaimed"
therefore means every byte housekeeping has not removed, including the
post-checkpoint tail, the current append segment and anything the tenant
floor is retaining.

**The check is a pre-append reservation, not a post-append test.** The commit
coordinator appends under the journal lock before waiting for the sync, so a
limit checked after the append would leave the supposedly-rejected batch
sitting in the WAL — and a client that retries on the rejection would then
have the batch replayed or published twice. The reservation is taken under the
same lock that performs the append, so a refused batch is never written at
all. There is no rollback path, deliberately: truncating an appended frame is
a second way to corrupt the tail.

The bound is configuration with a conservative default, and the rejection is
the contract: ingest keeps accepting while the object store is unreachable
until the declared limit, then refuses with a reason that names the limit it
hit. Crucially, crossing the limit does **not** set the quiesce latch — it is
a pressure state that clears itself the moment the checkpoint advances, which
§3.1's timer-driven barrier guarantees can happen without an append.

### 3.5 The state becomes observable

`WalMetrics` already carries `unflushed_bytes`, `disk_bytes` and
`segment_count`, and none of the three is exported: the only WAL metric
names in the registry are `ourios.wal.append.duration` and
`ourios.receiver.wal.truncated`. `unflushed_bytes` is read only
internally, for the fsync-batching decision. So "the WAL stopped
growing" — the incident's one observable symptom — was unobservable even
though the numbers existed in memory.

This RFC exports the existing three, adds the reclamation and
rotation-health state (a quiesce/retry state and the bytes and age held
below the checkpoint), and emits a log event on entering and leaving a
refusing state. Names come from the shared `ourios-semconv` registry in
one bump, not hand-written, and `error.type` continues to carry the
failure class on existing counters rather than spawning per-error
metrics.

### 3.6 Requeue on unwind

`write_ordered` requeues on every error arm already. It must do the same
on an unwind, which means the handling lives inside `write_ordered`
rather than at the call site, because the call site sees a partially
moved `Drained`.

The decision the unwind forces is duplicate-versus-lose: a panic
part-way through the audit write may have made some rows durable, so
requeueing risks a duplicate and dropping risks the loss in §2.2.
**This RFC chooses duplicates.** §3.4 forbids losing acknowledged data
and says nothing against delivering it twice; the recovery driver's
Parquet-side suppression is already the project's answer to at-least-once
replay, and the audit-ordering barrier is preserved either way because
the record flush is skipped whenever the audit sink has not drained.

**Choosing the policy is not the same as making it hold, so the mechanism is
specified too.** `write_ordered` moves `drained.audit` into `write_owned`
before it can touch `drained.records`, so a panic inside that call drops
whatever is still owned as `Drained` unwinds — the policy alone changes
nothing. An outer guard alone is not sufficient either, and this is the subtlety that
makes the fix structural rather than local: `write_owned` and `publish_owned`
take their `Vec`s **by value**, so the moment either is called the guard no
longer owns the batch and cannot requeue it if that callee panics. A guard
around `write_ordered` covers the panic points *between* the writes and none
of the ones inside them — which are the likely ones, since that is where the
store I/O and encoding live.

So the requirement lands on the consuming calls, not on their caller: each
takes the batch in a form that leaves ownership recoverable on unwind —
`&mut Vec` drained only on success, or an owned handle the callee itself
guards — so that after any panic the batch is still reachable and requeued.
The caller-side guard remains, covering the between-write points. What the
design requires is that **no** panic arm anywhere in the sequence can drop an
un-requeued batch; a `Drop` impl at one level cannot deliver that alone, and
neither can a `catch_unwind` at one call site.

With that settled, the age sweep can survive a panic and keep sweeping
(#795 deliberately stops, because without this it would repeat the loss
every tick).

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

  fn checkpoint(&mut self, durable_to: WalOffset) -> Result<(), ReclaimError>
  fn housekeeping(&mut self, floor: RetainFloor, max_unlinks: usize)
      -> Result<HousekeepingProgress, ReclaimError>
  fn reclaim_state(&self) -> ReclaimState                  // §3.5's export surface
  ```

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
  `disk_bytes` and `segment_count`, plus the bytes and age held below the
  checkpoint and the rotation-failure state — so the server reads it without
  reaching past the trait. `unflushed_bytes` stays its own method: the
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

The pass is therefore **capped**: at most `max_unlinks_per_pass` segments are
removed, and the pass returns having made partial progress rather than
finishing the backlog. The next timer tick continues where it left off, which
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

**Drop on unwind rather than requeue (§3.6).** Rejected: it prefers
silent loss of acknowledged data over a duplicate that the existing
suppression horizon already handles. That is the wrong way round under
§3.4.

**Make the sink ceiling a hard cap instead of adding a WAL bound
(§3.4).** Worth stating because `SINK_CEILING_BYTES` looks like the
natural place: the drain loop exits when `flush_largest()` fails and
buffers the record anyway, so the ceiling is a hint and memory grows
unbounded when the store is down. Blocking there instead would apply
backpressure in the wrong unit — buffered Parquet bytes rather than
unreclaimed WAL bytes — and would stall ingest on a condition that does not
threaten durability.

But rejecting it as the *signal* is not the same as leaving it alone, and the
earlier draft's "tracked separately" was a hand-wave. Both the record and the
audit sink retain past their ceilings whenever a store flush fails, so an
outage grows memory without bound and can OOM the process **before** the WAL
bound is anywhere near reached — in which case §3.4 never fires and this RFC
has bounded the wrong resource. That makes it a prerequisite, not a neighbour.

This RFC does not solve it, because the fix is a different decision (what does
a full sink do — block, spill, or drop, and under whose invariant), but it
states the dependency: §3.4's bound is only the operative limit if memory
growth during an outage is separately bounded, and until it is, the honest
claim is that this RFC bounds *disk* and the OOM path remains. RFC0052.6's
unreachable-store leg should be run long enough to show which limit is hit
first.

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
> - **Given** a node ingesting continuously with a healthy store, run
>   long enough to roll many segments
> - **When** the reclamation cadence has had time to act
> - **Then** the WAL's on-disk byte total and segment count are bounded
>   rather than monotonically increasing — the #793 signature (1,113
>   segments, nothing ever unlinked) cannot reproduce

> **Scenario RFC0052.4 — A transient rotation failure recovers without a
> restart**
> - **Given** a WAL whose rotation fails once with a transient I/O error
> - **When** a later `append` arrives after the condition clears
> - **Then** rotation is retried, succeeds, and the append is accepted
>   and acked
> - **And** no file a subsequent `Wal::open` would select as a segment
>   remains from the failed attempt — asserted by opening the WAL again,
>   not by inspecting the directory, since the temporary name is an
>   implementation detail
> - **And** the temporary files left by the failed attempts are gone after a
>   housekeeping pass, so a persistently retrying node cannot fill its disk
>   with retry debris

> **Scenario RFC0052.5 — A persistent rotation failure gives up
> distinguishably, and never acks**
> - **Given** a WAL whose rotation fails on every attempt
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
> - **Then** the remnant is unlinked durably and the open succeeds on the
>   preceding segment
> - **And** a partial header on an **older** segment, or more than one
>   unreadable file, still reports `OpenError::Corrupt` — the narrowing must
>   not become a general "discard what we cannot parse"

> **Scenario RFC0052.12 — A reclamation pass bounds the writer stall**
> - **Given** a backlog far larger than `max_unlinks_per_pass` (the
>   incident's 1,113 segments is the shape)
> - **When** a housekeeping pass runs
> - **Then** it unlinks at most the cap and returns, and an append taken
>   concurrently waits for at most that much work rather than the whole
>   backlog
> - **And** successive passes drain the backlog to the same end state an
>   uncapped pass would reach
> - **And** a backlog of stale *temporary* files is also bounded by the same
>   cap, so temp sweeping cannot make a "bounded" pass do unbounded work

> **Scenario RFC0052.13 — An incomplete tenant floor cannot be read as
> unbounded**
> - **Given** a snapshot consumer exists and one tenant with WAL data has no
>   valid snapshot
> - **When** housekeeping runs
> - **Then** nothing is reclaimed, and this is distinguishable in the API from
>   the no-consumer case, which reclaims by checkpoint alone
> - **And** the state is exported (§3.5), so an operator seeing backpressure
>   that will not clear can tell it is a lagging tenant rather than an
>   unexplained refusal

> **Scenario RFC0052.6 — Backpressure is a stated limit, and clears
> itself**
> - **Given** an unreachable object store and a configured local
>   retention bound
> - **When** ingest continues until the bound is crossed
> - **Then** earlier batches were accepted and acked, and the rejecting
>   batch is refused with a reason naming the limit and a `Retry-After`
>   derived from it
> - **And** the refused batch is **not present in the WAL** — asserted by
>   replaying after a restart, so a post-append check that left the frame
>   behind fails here
> - **And** the bound fires on bytes accumulated *above* a stalled
>   checkpoint, not only below it: a run where the checkpoint never advances
>   must still reach the limit
> - **And** the rejection does **not** set the rotation-failure state
> - **And** when the store returns, ingest resumes **with no append and no
>   restart** — the timer-driven barrier advances the checkpoint, which is
>   the only path that can clear a state that rejects every append

> **Scenario RFC0052.7 — The WAL's state is exported**
> - **Given** a running node
> - **When** metrics are collected
> - **Then** the WAL's unflushed bytes, on-disk bytes, segment count,
>   bytes and age held below the checkpoint, and rotation-failure state
>   are all present in the exported stream under registry names
> - **And** entering and leaving a refusing state each emit exactly one
>   log event, named from the registry

> **Scenario RFC0052.8 — A publish unwind keeps the records**
> - **Given** a cadence step whose publish panics after the batches have
>   been drained out of the sink
> - **When** the unwind completes
> - **Then** the drained records and audit events are back in their
>   buffers
> - **And** this holds for a panic raised **at each** point the publish can
>   reach it — before the audit write, inside it, and inside the record
>   publish — since the partial-move shape means only the last of those is
>   caught by a naive guard
> - **And** the publication barrier does not read the buffers as fully
>   drained, so no high-water mark is stamped across them
> - **And** a subsequent barrier with a healthy store publishes them

> **Scenario RFC0052.9 — The cadence survives a panic once unwinds are
> safe**
> - **Given** RFC0052.8 holding
> - **When** a cadence step panics
> - **Then** the sweep counts the panic and continues on the next tick
> - **And** a repeating panic loses no records, however many ticks it
>   spans

> **Scenario RFC0052.10 — No acknowledged record is lost across the whole
> cycle**
> - **Given** a node killed with `SIGKILL` mid-batch while reclamation,
>   rotation retry and backpressure are all live
> - **When** it restarts and recovery completes
> - **Then** every acknowledged record is present in Parquet, including
>   those whose segments were candidates for reclamation at the moment
>   of the kill

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
  the four rotation steps, once-failing and always-failing, asserting
  recovery in the first case and a distinguishable terminal refusal in
  the second, plus `Wal::open` succeeding afterwards in both (the
  temporary-name property) and the temp files being swept. A `proptest` over
  which step fails and how many times keeps the four sites from being tested
  only one way.
- **Pre-existing orphans (RFC0052.11)** — a directory fixture hand-built in
  the shape today's failure leaves, asserting both the unlink-and-open arm
  and the two arms that must still report corruption. A fixture rather than
  fault injection, because the state predates the code under test.
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
- **Backpressure (RFC0052.6)** — an integration test with an unreachable
  store asserting the accept-then-refuse-then-resume sequence, the reason
  text, the `Retry-After`, and that the rotation-failure state was never
  entered. Two legs carry most of the value: the refused batch must be absent
  after a restart-and-replay (a post-append check would leave it), and the
  resume must happen with **no append at all**, which is what proves the
  timer-driven barrier can clear a state that rejects every append. Those two
  are the regression tests for #791 itself.
- **Telemetry (RFC0052.7)** — the in-memory metric exporter pattern
  already used for the ingest/sink instruments, asserting every name is
  in the exported stream; plus a `weaver registry live-check` pass over
  the new log events, since an event the tests never emit is an event
  the live-check never checks (that is how #795's un-named event passed
  CI).
- **Unwind safety (RFC0052.8, RFC0052.9)** — a publish double that panics on
  demand at **each** reachable point (before the audit write, inside it,
  inside the record publish), asserting the buffers are repopulated in every
  case, the barrier refuses to stamp, and a later healthy barrier publishes.
  Parameterising the panic point is the whole test: the partial-move shape
  means a guard that only covers the last point passes a single-point test.
  RFC0052.9 drives many consecutive panicking ticks and asserts the record
  count is conserved.
- **No loss (RFC0052.10)** — extends the existing `SIGKILL`
  crash-recovery test rather than adding a parallel one, with
  reclamation and a small backpressure bound configured so the kill
  lands in the regime this RFC introduces.

Validation: this RFC reaches `validated` when RFC0052.3's soak run is
recorded and RFC0052.10 passes in CI, since those two are the ones that
demonstrate the incident cannot recur rather than that a unit behaves.

## 7. Open questions

- [ ] The backpressure bound's default. A bytes limit, an age limit, or
      both? The incident node held 42 MB over five days, so a default
      tuned for it would be far too small for a busy node; the honest
      default may be a fraction of the volume rather than an absolute.
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
- [ ] What a full record or audit sink should do during an outage: block,
      spill, or drop. §4 now states that §3.4's disk bound is only the
      operative limit once memory growth is separately bounded, and that
      question is not answered here. It may need to land *before* this RFC's
      backpressure to be meaningful.
- [ ] Whether a stale tenant floor blocking reclamation indefinitely should
      itself escalate (a second, louder state) or stay a visible metric an
      operator alerts on. §3.4 makes it visible; it does not decide.
- [ ] Whether backpressure should be per-tenant rather than per-node.
      It is a local-disk property, so per-node is the natural unit, but a
      single noisy tenant can then refuse every other tenant's writes.

## 8. References

- Issue #791 — the incident: ingest wedged at `503`, no self-recovery,
  no observability.
- Issue #793 — `checkpoint`/`housekeeping` called only from tests.
- Issue #796 — a publish unwind drops drained batches and the snapshot
  guard reads empty buffers as fully drained.
- PR #794 — the OTLP `Status` body and the transient-versus-wedged
  distinction (makes the state legible; changes no behaviour).
- PR #795 — counts a cadence panic (makes a dead sweep alertable;
  deliberately still stops, pending RFC0052.8).
- RFC 0008 §6.5 (rotation), §6.6 (recovery horizon), §6.7 (checkpoint
  and housekeeping), §6.8 (counters), §6.9 (tunables) — the mechanism
  this RFC supplies the policy for.
- RFC 0001 §6.9 — the miner snapshot high-water mark, and the hazard-#5
  retain rule that makes it the truncation floor.
- RFC 0014 — the record sink and its flush triggers.
- `CLAUDE.md` §3.4 (WAL-before-ack), §3.6 (object storage is the source
  of truth), §6.3 (observability of ourselves).
- `docs/hazards.md` #3 (WAL durability versus latency), #4 (small
  files), #5 (template schema evolution across deploys — the retain
  rule).
