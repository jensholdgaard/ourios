---
rfc: 0053
title: WAL backpressure and unwind safety
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-09-13
supersedes: —
superseded-by: —
---

# RFC 0053 — WAL backpressure and unwind safety

> **Status note.** `drafted`. **Stage 2 of two**, split out of RFC 0052 at the
> maintainer's direction: that RFC carried reclamation, rotation recovery,
> backpressure and unwind safety as one document, and six review rounds found
> most of their defects at the seams between the parts. This RFC **depends on
> RFC 0052** and cannot land before it — backpressure clears only when
> reclamation removes bytes, its livelock fix needs the timer RFC 0052
> introduces, and unwind safety is what lets the age sweep survive a panic
> once the records it drops have somewhere safe to go. Touches `CLAUDE.md`
> §3.4 throughout.

## 1. Summary

Two things RFC 0052 deliberately leaves open. First, the only limit on local
accumulation during an object-store outage is the volume itself — an implicit
limit with an undefined failure mode, which is what turned the #791 outage
into a wedge. This RFC declares an explicit, configurable local bound and
makes crossing it a *stated* rejection with a transport contract, cleared by
reclamation rather than by restart. Second, a panic inside the publish drops
batches the age sweep has already taken out of the sink, and the snapshot
barrier then reads the emptied buffers as fully drained (#796); this RFC
requeues them on unwind, choosing duplicates over loss, and only then lets the
sweep keep running after a panic (#795 stops it today for exactly this
reason).

## 2. Motivation

### 2.1 What RFC 0052 fixes, and what it does not

RFC 0052 makes the WAL reclaim segments and makes a rotation failure
recoverable. After it, a node no longer fills its volume in steady state and no
longer wedges permanently on one failed fsync. It does not say what happens
when the object store is unreachable for long enough that the WAL *cannot* be
reclaimed — frames accumulate above a checkpoint that cannot advance — and the
only thing that eventually stops accepting them is `ENOSPC`. That is the
incident's shape with the permanent latch removed: the node degrades into a
rotation-failure state rather than an unrecoverable one, but it still gets
there by running out of disk, with no warning and no stated limit.

### 2.2 The unwind defect

**#796 — a publish unwind loses drained batches.** `drain_aged` removes
batches from the sink buffers and `write_ordered` consumes them, so a panic
inside the publish drops them: no longer buffered, never written.
`flush_then_snapshot` then reads the empty buffers as "fully drained" and can
stamp a WAL high-water mark over frames that never reached the store, after
which recovery suppresses them. The non-panic paths requeue correctly; only
the unwind does not. #795 counts the panic and **stops** the sweep, because
continuing would repeat that loss every tick; this RFC is what makes
continuing safe.

### 2.3 Why these two together, and why after RFC 0052

Both are decisions about what to do when acknowledged data cannot yet reach
object storage: keep accepting more, or refuse, and how to hold what is
already in flight. Both depend on RFC 0052's machinery — backpressure clears
through its reclamation sequence and needs its timer to break a livelock; the
sweep's survival needs its barrier semantics to stay sound under a requeue.
Neither can be specified precisely against a WAL that never reclaims, which is
why they were the parts of the combined draft that kept moving.

## 3. Proposed design

### 3.1 Backpressure becomes explicit

Today the only limit on local accumulation is the volume. That is an implicit
limit with an undefined failure mode. The WAL gains a declared local bound, and
crossing it is a *stated* rejection, specified as a transport contract rather
than gestured at.

**The transport contract.** `ReceiveError` gains a `WalBackpressure` variant
carrying the limit that was hit and the measurement that crossed it;
`IngestFailure::classify` maps it to a new `Backpressure` outcome, which that
exhaustive match then forces both transports to handle; both render `503` /
`UNAVAILABLE` with the limit named in the `Status` message. `503` because the
batch was not acked — RFC 0018 §3.2's reasoning — and a distinct outcome
rather than reusing `Unavailable` because the remedy differs: waiting genuinely
helps here.

The wire shape is stated, because an empty body is how #791 hid for eight
hours. On HTTP the rejection is the `google.rpc.Status` body the OTLP spec
requires and #794 established — protobuf or JSON, mirroring the request's
`Content-Type`, with the message naming the limit and the measurement — and
`Retry-After` is a response header. On gRPC it is the `Status` message plus a
`RetryInfo` detail carrying the same delay. A bare status code with no body,
which is what the HTTP arm returned before #794, does not satisfy this
contract, and RFC0053.1 asserts the body on both transports.

**`Retry-After` is the reclamation interval, not a function of the limit.**
A byte bound yields no delta-seconds value, and a client cannot
implement against it. What the server actually knows is *when it will next
try to reclaim* — `housekeeping_secs`, RFC 0052 §3.2's cadence — so that is the
advertised delay in seconds, and a client honouring it arrives just after the
next pass. Nothing finer would be honest, because whether that pass frees
enough depends on the store and the tenant floor; a backoff estimator would be
inventing a prediction the server cannot make.

gRPC carries the same number as a `RetryInfo` detail, which is what
[OTLP's throttling section](https://opentelemetry.io/docs/specs/otlp/#otlpgrpc-throttling)
specifies for the backpressure case. Whether that makes RFC 0018 §3.2's
`RESOURCE_EXHAUSTED`-with-`RetryInfo` option the better code here is a §7
question, not one this RFC settles.

**The bound is a byte bound over ALL unreclaimed bytes — not over bytes
retained below the checkpoint, and not an age.** During the outage this is
meant to bound, the checkpoint is precisely what stops advancing, so new
frames pile up *above* it: a below-checkpoint limit would measure the one
quantity that is not growing and never fire. "Unreclaimed" therefore means
every byte housekeeping has not removed, including the post-checkpoint tail,
the current append segment and anything the tenant floor is retaining. RFC
0052 §3.7's `ReclaimState` carries exactly this figure, maintained
incrementally rather than from the best-effort `disk_bytes` walk, for the
reason given there.

The age of the oldest unreclaimed frame is exported beside it (RFC 0052 §3.5)
and is the right thing to *alert* on, but it is not an admission rule: an age
does not grow with an append, so an age limit could only fire from the timer,
which makes it a state flip with different clearing semantics rather than a
reservation. An earlier draft left "bytes, age, or both" open; this one is
byte-only, and an age-driven refusal, if ever wanted, is a separate decision.

**The check is a pre-append reservation, not a post-append test — and it has
an owner.** The commit coordinator appends under the journal mutex before
waiting for the sync, so a limit checked after the append would leave the
supposedly-rejected batch sitting in the WAL, and a client retrying on the
rejection would have it replayed or published twice.

The check therefore lives in `CommitCoordinator`, **inside the same journal
mutex acquisition that performs the append**, reading the measurement from
`Journal::reclaim_state()` immediately before calling `append_batch`. Not in
the receiver and not in a layer above: anywhere outside that mutex races
concurrent appends, so two batches could each observe room and both be
written. `append_batch` itself is unchanged — the reservation is the caller's
responsibility because the caller is what holds the mutex.

- **The limit is the coordinator's, not the WAL's.** `CommitCoordinator::new`
  takes it alongside the batch window and segment size it already receives, so
  it sits with the other admission policy rather than inside a journal whose
  job is durability. `Journal` reports the *measurement*; the coordinator owns
  the *threshold*. That also keeps the limit configurable without widening the
  trait.
- **The frame length comes from the journal, not from `payload.len()`.** The
  coordinator sees only the encoded payload; `Wal::append` prepends a
  12-byte frame header before it accounts the bytes, so a reservation taken
  on the payload length alone under-reserves by the header on every batch.
  `Journal` therefore gains `fn framed_len(&self, payload_len: usize) -> u64`,
  and the WAL's own append accounting is rewritten to call the same function,
  so the reservation and the write cannot disagree and a test double reports
  the same number the real WAL would. An oversize batch still fails as
  `TooLarge` first, since that check precedes admission.

There is no rollback path, deliberately: truncating an appended frame is a
second way to corrupt the tail, so the only safe reservation is one taken
before the write.

The bound is configuration with a conservative default, and the rejection is
the contract: ingest keeps accepting while the object store is unreachable
until the declared limit, then refuses with a reason that names the limit it
hit. Crucially, crossing the limit does **not** set RFC 0052's
rotation-failure state — it is a pressure state, not a fault.

**It clears when a housekeeping pass actually removes bytes, not when the
checkpoint advances.** Advancing the sidecar declares frames reclaimable; it
does not reclaim them, and the bound is measured in bytes still on disk. So
the clearing path is the whole RFC 0052 §3.2 sequence — barrier, checkpoint,
housekeeping — which that RFC's timer can drive without an append.

**It also needs the timer to be able to force a rotation, or it deadlocks on
its own.** Housekeeping never unlinks the *current* append segment, and a
crossed bound rejects the appends that would trigger size- or age-based
rotation. If an outage's whole backlog sits in that one segment — the normal
case for a low-volume node, whose segments roll on age — then every pass finds
nothing reclaimable, the bound never clears, and no append will ever arrive to
roll the segment. A livelock built out of two individually-correct rules.

So the timer, holding the barrier exclusion RFC 0052 §3.1 gives it,
**rotates** when the bound is crossed and the current segment is the only
thing holding unreclaimed bytes. Rotation is a WAL operation rather than an
append, so backpressure does not block it; the segment closes, the next pass
can reclaim it, and the state clears. Nothing is acked by that rotation, so
the no-ack-on-refusal property is untouched.

That needs an owner, because today `Wal::rotate` is private and is reached
only from `append`. `Journal` gains `fn rotate(&mut self) -> Result<(),
ReclaimError>`: it closes the current segment and opens a fresh one through
the same retried path RFC 0052 §3.3 specifies for an append-driven rotation,
so a failure enters the same bounded-retry state and the same terminal state,
reported the same way; it is a no-op returning `Ok` when the current segment
holds no frames, so a timer that calls it unconditionally cannot manufacture
empty segments. The timer reaches it through the coordinator's journal mutex,
exactly as RFC 0052 §3.7 routes `checkpoint` and `housekeeping`, so the
single-writer position still has one owner.

A second way the state can persist, which is *not* a deadlock and is handled
differently: a stale tenant floor holds `min(checkpoint, floor)` down, so
housekeeping removes nothing and the bound stays crossed even though the
object store recovered. That is not a bug to paper over — a tenant whose
snapshot is not advancing is a real problem — but it must be *visible* rather
than presenting as an unexplained refusal, which is why RFC 0052 §3.5 exports
the floor and its lag alongside the bytes. RFC0053.1 asserts the healthy-store
resume; the stale-floor case is a named open question in §7 rather than a
silently accepted behaviour.

### 3.2 Requeue on unwind

`write_ordered` requeues on every error arm already. It must do the same on
an unwind, which means the handling lives inside `write_ordered` rather than
at the call site, because the call site sees a partially moved `Drained`.

The decision the unwind forces is duplicate-versus-lose: a panic part-way
through the audit write may have made some rows durable, so requeueing risks a
duplicate and dropping risks the loss in §2.2. **This RFC chooses
duplicates.** `CLAUDE.md` §3.4 forbids losing acknowledged data and says
nothing against delivering it twice; the recovery driver's Parquet-side
suppression is already the project's answer to at-least-once replay, and the
audit-ordering barrier is preserved either way because the record flush is
skipped whenever the audit sink has not drained.

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

So the requirement lands on the consuming calls, not on their caller: each
takes the batch in a form that leaves ownership recoverable on unwind — `&mut
Vec` drained only on success, or an owned handle the callee itself guards — so
that after any panic the batch is still reachable and requeued. The
caller-side guard remains, covering the between-write points. What the design
requires is that **no** panic arm anywhere in the sequence can drop an
un-requeued batch; a `Drop` impl at one level cannot deliver that alone, and
neither can a `catch_unwind` at one call site.

**What is requeued depends on where the panic lands.** The audit events are
requeued only while the audit write has not completed; once `write_owned` has
returned true they are durable in the audit store, and a later panic inside
the record publish requeues the records alone. Requeueing durable audit
events would manufacture a duplicate the policy tolerates but does not seek,
and the ordering barrier does not need it: the next flush skips the record
publish only when the audit sink has *undrained* events, which after a
completed audit write it does not. So the recoverable audit handle need only
live until that call returns; the recoverable record handle must live until
the record publish returns.

With that settled, the age sweep can survive a panic and keep sweeping. #795
deliberately stops, because without this it would repeat the loss every tick;
that stop is reversed here, and the `cadence_panic` counter it added stays,
now meaning "a step panicked and was retried" rather than "the cadence is
dead".

### 3.3 Telemetry

RFC 0052 §3.5 already exports the measurement this RFC's bound is taken on and
the floor state that explains a bound that will not clear. This RFC adds the
backpressure state itself — whether the node is currently refusing on the
bound, and the limit and measurement at the last refusal — and a log event on
entering and leaving that state. Names come from the shared `ourios-semconv`
registry in one bump with RFC 0052's, not hand-written, and `error.type`
continues to carry the failure class on existing counters rather than
spawning per-error metrics.

## 4. Alternatives considered

**Drop on unwind rather than requeue (§3.2).** Rejected: it prefers silent
loss of acknowledged data over a duplicate that the existing suppression
horizon already handles. That is the wrong way round under `CLAUDE.md` §3.4.

**Make the sink ceiling a hard cap instead of adding a WAL bound (§3.1).**
Worth stating because `SINK_CEILING_BYTES` looks like the natural place: the
drain loop exits when `flush_largest()` fails and buffers the record anyway,
so the ceiling is a hint and memory grows unbounded when the store is down.
Blocking there instead would apply backpressure in the wrong unit — buffered
Parquet bytes rather than unreclaimed WAL bytes — and would stall ingest on a
condition that does not threaten durability.

But rejecting it as the *signal* is not the same as leaving it alone. Both the
record and the audit sink retain past their ceilings whenever a store flush
fails, so an outage grows memory without bound and can OOM the process
**before** the WAL bound is anywhere near reached — in which case §3.1 never
fires and this RFC has bounded the wrong resource. That makes it a
prerequisite, not a neighbour.

This RFC does not solve it, because the fix is a different decision (what does
a full sink do — block, spill, or drop, and under whose invariant), but it
states the dependency: §3.1's bound is only the operative limit if memory
growth during an outage is separately bounded, and until it is, the honest
claim is that this RFC bounds *disk* and the OOM path remains. RFC0053.1's
unreachable-store leg should be run long enough to show which limit is hit
first.

**Backpressure as a rotation-failure state.** Rejected: it would reuse RFC
0052's terminal-state reporting for a condition that is not a fault and clears
on its own, telling clients a node is broken when it is merely full. The two
are kept distinct so that the remedy each advertises is the true one.

## 5. Acceptance criteria

> **Scenario RFC0053.1 — Backpressure is a stated limit, and clears itself**
> - **Given** an unreachable object store and a configured local retention
>   bound, on a node running RFC 0052
> - **When** ingest continues until the bound is crossed
> - **Then** earlier batches were accepted and acked, and the rejecting batch
>   is refused with a reason naming the limit, and a `Retry-After` equal to
>   the reclamation cadence — **not** a value derived from the limit, which
>   yields no delta-seconds
> - **And** on HTTP the reason is a `google.rpc.Status` body in the request's
>   format with `Retry-After` as a header, and on gRPC a `Status` message
>   with a `RetryInfo` detail — a bare status code with an empty body fails
>   this scenario
> - **And** the refused batch is **not present in the WAL** — asserted by
>   replaying after a restart, so a post-append check that left the frame
>   behind fails here
> - **And** the bound fires on bytes accumulated *above* a stalled
>   checkpoint, not only below it: a run where the checkpoint never advances
>   must still reach the limit
> - **And** the rejection does **not** set the rotation-failure state
> - **And** when the store returns, ingest resumes **with no append and no
>   restart** — the timer-driven sequence reclaims, which is the only path
>   that can clear a state that rejects every append
> - **And** when the whole backlog sits in the current append segment, the
>   timer's forced rotation lets the next pass reclaim it, so the state clears
>   without an append ever arriving

> **Scenario RFC0053.2 — A publish unwind keeps the records**
> - **Given** a cadence step whose publish panics after the batches have been
>   drained out of the sink
> - **When** the unwind completes
> - **Then** the drained records are back in their buffer
> - **And** the drained audit events are back in theirs whenever the audit
>   write had not completed; after it has, they are durable and are **not**
>   requeued, so a later panic does not duplicate them
> - **And** this holds for a panic raised **at each** point the publish can
>   reach it — before the audit write, inside it, and inside the record
>   publish — since the partial-move shape means only the last of those is
>   caught by a naive guard
> - **And** the publication barrier does not read the buffers as fully
>   drained, so no high-water mark is stamped across them
> - **And** a subsequent barrier with a healthy store publishes them

> **Scenario RFC0053.3 — The cadence survives a panic once unwinds are safe**
> - **Given** RFC0053.2 holding
> - **When** a cadence step panics
> - **Then** the sweep counts the panic and continues on the next tick
> - **And** a repeating panic loses no records, however many ticks it spans

> **Scenario RFC0053.4 — No acknowledged record is lost with backpressure
> live**
> - **Given** a node killed with `SIGKILL` mid-batch while reclamation,
>   rotation retry **and backpressure** are all live, with the bound small
>   enough that the kill lands in the refusing regime
> - **When** it restarts and recovery completes
> - **Then** every acknowledged record is present in Parquet, and no refused
>   batch is

## 6. Testing strategy

Per `CLAUDE.md` §6.2, mapped to the §5 ids.

- **Backpressure (RFC0053.1)** — an integration test with an unreachable
  store asserting the accept-then-refuse-then-resume sequence, the reason
  text, the `Retry-After`, and that the rotation-failure state was never
  entered. Three legs carry most of the value: the refused batch must be
  absent after a restart-and-replay (a post-append check would leave it); the
  resume must happen with **no append at all**, which is what proves the
  timer-driven sequence can clear a state that rejects every append; and the
  single-segment backlog must clear through the forced rotation, which is the
  livelock case. Those are the regression tests for #791's second half.
- **Unwind safety (RFC0053.2, RFC0053.3)** — a publish double that panics on
  demand at **each** reachable point (before the audit write, inside it,
  inside the record publish), asserting the buffers are repopulated in every
  case, the barrier refuses to stamp, and a later healthy barrier publishes.
  Parameterising the panic point is the whole test: the partial-move shape
  means a guard that only covers the last point passes a single-point test.
  RFC0053.3 drives many consecutive panicking ticks and asserts the record
  count is conserved.
- **No loss (RFC0053.4)** — extends RFC 0052's `SIGKILL` crash-recovery
  extension rather than adding a parallel one, with a small backpressure
  bound configured so the kill lands in the refusing regime.

Maturity, per `docs/rfcs/README.md`: `green` is RFC0053.1–.4 all passing in
CI, and this RFC touches no thesis gate in `docs/benchmarks.md` §7. It does
**not** proceed to `validated` on those alone: §4 records that the sinks can
exhaust memory before the WAL bound fires, so a green run could mark a bound
validated that is never the operative limit. `validated` therefore also
requires the §7 sink decision to have landed and RFC0053.1's
unreachable-store leg, run under default configuration for long enough to
show the WAL bound refusing **before** either sink exceeds its ceiling. Until
that decision lands the RFC stops at `green`, and says so.

## 7. Open questions

- [ ] The byte bound's default size. The incident node held 42 MB over five
      days, so a default tuned for it would be far too small for a busy node;
      the honest default may be a fraction of the volume rather than an
      absolute.
- [ ] What a full record or audit sink should do during an outage: block,
      spill, or drop. §4 states that §3.1's disk bound is only the operative
      limit once memory growth is separately bounded, and that question is not
      answered here. §6 makes it a gate on `validated`, so it must land
      before this RFC can be more than `green`.
- [ ] Whether a stale tenant floor blocking reclamation indefinitely should
      itself escalate (a second, louder state) or stay a visible metric an
      operator alerts on. §3.1 makes it visible; it does not decide.
- [ ] Whether backpressure should be per-tenant rather than per-node. It is a
      local-disk property, so per-node is the natural unit, but a single noisy
      tenant can then refuse every other tenant's writes.
- [ ] Whether `RESOURCE_EXHAUSTED` with `RetryInfo` is the better gRPC code
      for the backpressure outcome, as RFC 0018 §3.2 allows for saturation,
      rather than `UNAVAILABLE`. The HTTP side has no such choice to make.

## 8. References

- RFC 0052 — WAL reclamation and quiesce recovery: the stage this one depends
  on, for the timer, the barrier exclusion, `ReclaimState`, `RetainFloor`,
  and the terminal-only classification of a rotation failure.
- Issue #791 — the incident; this RFC is its "no stated limit" half.
- Issue #796 — a publish unwind drops drained batches and the snapshot guard
  reads empty buffers as fully drained.
- PR #795 — counts a cadence panic and stops the sweep; §3.2 is what makes
  reversing that stop safe.
- RFC 0018 §3.2 (retryable error mapping) — the reasoning for `503` on an
  unacked batch, and the `RESOURCE_EXHAUSTED` option §7 leaves open.
- RFC 0014 — the record sink and its flush triggers.
- `CLAUDE.md` §3.4 (WAL-before-ack), §6.3 (observability of ourselves).
- `docs/hazards.md` #3 (WAL durability versus latency), #4 (small files).
