---
rfc: 0053
title: WAL backpressure
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-09-13
supersedes: —
superseded-by: —
---

# RFC 0053 — WAL backpressure

> **Status note.** `drafted`. **Stage 2 of two**, split out of RFC 0052 at
> the maintainer's direction — and since split again: this RFC is the WAL
> **backpressure** bound alone. Publish unwind safety is **RFC 0054**,
> publication frontiers and tenant settlement **RFC 0055**, and the
> RFC 0005 §7 audit-durability amendment **RFC 0056**. The wording moved
> into each is the reviewed wording of this document at
> [`#802`](https://github.com/jensholdgaard/ourios/pull/802) @ `30a21f80`,
> which stays the quarry. This RFC **depends on RFC 0052** and cannot land
> before it: backpressure clears only when reclamation removes bytes, and
> its livelock fix needs the timer RFC 0052 introduces — §3.2 lists what
> this RFC asks of that one rather than specifying it here. It **also
> depends on RFC 0055** for the tenant guard, the admission order and the
> lock order §3.1 defers to and RFC0053.1/.5 assert. Touches `CLAUDE.md`
> §3.4 throughout. It amends no accepted RFC.

## 1. Summary

One thing RFC 0052 deliberately leaves open: the only limit on local
accumulation during an object-store outage is the volume itself — an implicit
limit with an undefined failure mode, which is what turned the #791 outage
into a wedge. This RFC declares an explicit, configurable local bound and
makes crossing it a *stated* rejection with a transport contract, cleared by
reclamation rather than by restart.

## 2. Motivation

RFC 0052 makes the WAL reclaim segments and makes a rotation failure
recoverable, so a node no longer fills its volume in steady state or wedges
permanently on one failed fsync. It does not say what happens when the object
store is unreachable for long enough that the WAL *cannot* be reclaimed —
frames accumulate above a checkpoint that cannot advance — and the only thing
that eventually stops accepting them is `ENOSPC`. That is the incident's shape
with the permanent quiesce removed: the node degrades into a rotation-failure
state rather than an unrecoverable one, but still gets there by running out of
disk, with no warning and no stated limit.

## 3. Proposed design

### 3.1 Backpressure becomes explicit

Today the only limit on local accumulation is the volume — an implicit limit
with an undefined failure mode. The WAL gains a declared local bound, and
crossing it is a *stated* rejection, specified as a transport contract.

**The transport contract.** `ReceiveError` gains two variants, because
§3.1 refuses for reasons of two different kinds. `WalBackpressure` carries
a typed **cause** and the delay to advertise, for the two bounds that
reclamation clears: `BackpressureCause::Bytes { pre, projected:
Option<u64>, limit }` — the pre-reservation unreclaimed total and the
projected total including this batch's `framed_len` (or, when the
projection overflows, `None` in its place), since a per-request refusal
can happen with the live total still below the limit and a single
"measurement" would be ambiguous — and `Segments { count, limit }` for the
`max_segments` ceiling; the `Status` message renders the cause's own
measurements and names its limit, so a refusal always says which bound it
hit. `TenantCapacity { count, limit }` is its own variant, not a cause,
because the tenant-capacity guard RFC 0055 defines is **capacity**: no
pass clears it, so it
carries no delay and sets no latch (§3.1). `IngestFailure::classify` maps
the first to a new `Backpressure` outcome and the second to a new
`TenantCapacity` outcome, which that exhaustive match then forces both
transports to handle; both render `503` / `UNAVAILABLE` with the limit
named in the `Status` message, and only `Backpressure` carries the delay.
`503` because the batch was not acked — RFC 0018 §3.2's reasoning — and
distinct outcomes rather than reusing `Unavailable` because the remedy
differs: for backpressure, waiting genuinely helps; for capacity, only an
operator does, and the client backs off as OTLP prescribes when no
`Retry-After` / `RetryInfo` is sent.

The wire shape is stated, because an empty body is how #791 hid for eight
hours. On HTTP the rejection is the `google.rpc.Status` body the OTLP spec
requires and #794 established — binary protobuf with
`application/x-protobuf` whatever the request's encoding, because the
Collector's exporter decodes every failure body as protobuf regardless of
`Content-Type`, with the message naming the limit and the measurement — and
`Retry-After` is a response header, present for `Backpressure` and for
RFC 0055's `SettlementInProgress` — both self-clearing — and absent for
`TenantCapacity`, which only an operator clears. On gRPC it is the
`Status` message plus, for those two, a `RetryInfo` detail carrying the
same delay. A bare status code with no body,
which is what the HTTP arm returned before #794, does not satisfy this
contract, and RFC0053.1 asserts the body on both transports.

**`Retry-After` is the next reclaim opportunity, not a function of the
limit.** A byte bound yields no delta-seconds value, and a client cannot
implement against it. What the server actually knows is *when it will next
be able to remove bytes*, and under RFC 0052 that is two cadences, not one:
the barrier (`barrier_secs`, default 300 s) is what advances the checkpoint
and so makes segments eligible, and housekeeping (`housekeeping_secs`,
60 s) is what unlinks them. A refusal just after both ticks therefore
cannot be cleared by the next housekeeping pass alone. So the advertised
delay is an **upper bound on the next reclaim opportunity the coordinator
can see**, computed at the refusal from two inputs. The first is a
trait-visible eligibility signal: RFC 0052 withdrew the eligible queue, so
this RFC adds `ReclaimState::reclaimable_now: bool` — true when the head
of RFC 0052 §3.2's ordered structure satisfies the lazily evaluated predicate (empty
set, key at or below the checkpoint) or an entry is still *reclaiming* from
a pass whose unlink failed — an O(1) read of the head under the journal
mutex the refusal already holds. The second is the schedule, which the
timer owns: the coordinator is constructed with a `ReclaimSchedule` handle
that the barrier and housekeeping tasks update with their next due instants
each tick, so the coordinator never derives a due time from a cadence
alone. The delay is then the least of the opportunities the state admits:
when `reclaimable_now`, the time to the next housekeeping due instant; when
§3.1's forced-rotation predicate holds — the latch set by a byte refusal,
nothing reclaimable, the current segment the only holder — so the next
pass rotates and the one after reclaims, the time to the next housekeeping
due instant plus one housekeeping cadence; otherwise the
time to the next barrier due instant plus one housekeeping cadence, since
nothing becomes reclaimable before the barrier runs. Seconds are rounded
up, with a minimum of one. The value is a **non-binding hint**: whether
the pass frees enough depends on the store and the tenant floor, and a
client that arrives earlier is simply refused again with a fresh hint.
Nothing finer would be honest; a backoff estimator would be inventing a
prediction the server cannot make.

The delay travels *in the error*. The transport mappers see only a
`&ReceiveError` and their handler state knows nothing of the WAL's cadence, so
the coordinator — constructed with the schedule handle alongside the bound —
stamps the delay into `WalBackpressure` at the refusal, and both mappers read
it from there. No new state is threaded through either receiver.

gRPC carries the same number as a `RetryInfo` detail, which is what
[OTLP's throttling section](https://opentelemetry.io/docs/specs/otlp/#otlpgrpc-throttling)
specifies for the backpressure case, and the code is `UNAVAILABLE` — chosen
here rather than left open, so both adapters carry one retry semantics and
RFC0053.1 can assert it. RFC 0018 §3.2's `RESOURCE_EXHAUSTED` option is
not taken: it is the saturation code, and a client that maps it to a
different backoff than the `503` its HTTP twin receives would treat the
same condition two ways.

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

Three boundaries of that figure are stated so the reservation and the
measurement cannot drift apart. **It is over frame bytes**, the unit RFC
0052 §3.7 gives `rebuild_ledger()` — the sum of the validated frames'
lengths, never file size — so the live figure and the restart figure are
the same number and this RFC amends nothing there. Segment headers are outside the
measurement and the reservation alike: a pre-write rotation adds one, and
the coordinator cannot know whether an append will rotate. Headers are
bounded separately, because frame bytes do not bound them — §3.1's forced
rotation and age-based rotation both close near-empty segments, so an
outage on a low-volume node accumulates headers while its frame bytes
stand still: `WalConfig` gains **`max_segments`**, a ceiling on retained
**closed** segments — the *current* segment is outside the cap, for the
reason §3.1 gives below — defaulting to
`unreclaimed_bytes_limit / segment_size_bytes + 16` so a bound's worth of
full closed segments always fits with slack for the near-empty ones, and a
reservation whose append would rotate, while `closed_retained` is at the
ceiling, is refused under the same backpressure class with the `Segments`
cause. Whether the append would rotate is the WAL's question, not the
coordinator's, and its predicate is size **or age** (§3.2's
`rotation_due`); the reservation takes a segment slot exactly when it is
true: `closed_retained + 1 ≤ max_segments`, else refuse. **The cap counts
*closed* retained segments, and the current segment is outside it**, which
is what keeps an owed rotation from deadlocking: housekeeping never
unlinks the current segment, so a cap that counted it could be reached in
a state no pass can relieve — every older segment pinned or ineligible
while the current segment's own frames are checkpoint-covered — and
the deferred owed rotation would wait for a slot that never comes, with
the WAL unable to accept anything at all. Capacity is therefore reserved
rather than hoped for. The invariant, stated once: **an owed rotation
always has room.** The segment it closes enters the cap ungated — the one
admission the cap does not gate, since refusing it would mean refusing to
close a segment that must never take another frame. A *discretionary* rotation is gated as before, so the cap
still bounds retained header overhead and still refuses an append that
would grow it. `validate_config` rejects `max_segments < 1`:
a ceiling of zero admits no retained closed segment at all, so the first
closed segment would refuse every later discretionary rotation and the
node would stall on any frame needing one. The derived default is far above it. RFC0053.1 asserts the
validation. At the ceiling a **discretionary** due rotation is **refused,
never squeezed in** — an *owed* rotation is the stated exception and
proceeds regardless, since closing the current segment necessarily adds a
closed one and refusing that is the deadlock §3.1 removes; the exception
is why the ceiling reaches no rotation-failure state at all. For the discretionary case: the request path runs no housekeeping; the refusal sets the latch, the next `maintain` pass
reclaims what it can, and the client returns after the `Retry-After` §3.1
computes — so with a reclaimable segment the request is refused once and
admitted after the pass, and with nothing reclaimable it stays refused,
and no forced rotation runs at the ceiling (below). A reservation that would not rotate
is unaffected, since it creates no header. **A segment stays in it
until its unlink succeeds.** RFC 0052 §3.2 marks a popped ledger entry
*reclaiming* and keeps it in the byte and segment accounting until the
off-lock unlink has succeeded, so the bytes of a segment whose `RECLAIM`
write or unlink failed are still `unreclaimed` here and a refusal is never
computed against bytes that are still on disk. **It survives a
restart by being rebuilt, not persisted, in the same unit.** Once replay
and heal have settled the newest segment's tail — a torn frame there is
truncated by heal and must not be counted — the figure is initialised as
the sum over every surviving segment, closed and current, of its
**validated frame lengths**, so the rebuilt number is frame bytes like the
live one and matches what the reservation adds to it. The rebuild hook is §3.2's, run after recovery and before the
coordinator is constructed, so the seed completes before any append is
admitted and a node restarted mid-outage resumes refusing at the same bound
rather than admitting from zero. The same scan seeds the current segment's
frame bytes (§3.1's rotation trigger) from the healed newest segment. And
it fails closed: a listing, read or frame-validation error from the scan
fails startup before the coordinator or any listener is constructed,
because a partial or zero seed is precisely a node that admits past its
bound. RFC0053.4's restart asserts both.

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
written. `append_batch`'s responsibility is unchanged — it appends, and per
RFC 0052 §3.7 now returns the frame's `WalOffset` — and the reservation is
the caller's responsibility because the caller is what holds the mutex.

- **The limit is the coordinator's, not the WAL's.** `CommitCoordinator::new`
  takes it alongside the batch window and segment size it already receives, so
  it sits with the other admission policy rather than inside a journal whose
  job is durability. `Journal` reports the *measurement*; the coordinator owns
  the *threshold*. That also keeps the limit configurable without widening the
  trait.
- **An oversize batch is still `TooLarge`, never backpressure — and the
  coordinator makes that so.** Because the reservation now runs before
  `append_batch`, a refused request never reaches `Wal::append`'s own
  `MAX_FRAME_BYTES` check, so the ordering cannot be left to the WAL. `Journal`
  gives the coordinator the number **at construction** rather than on the
  path: `CommitCoordinator::new` takes `max_frame_bytes` beside the batch
  window, the segment size and the byte limit it already receives, and the
  coordinator rejects `payload_len > max_frame_bytes` as `TooLarge` under
  the admission mutex, before any journal lock is taken — the WAL's own
  check remaining as the backstop.
  The full admission order is one sequence, stated once, and it spans the
  two locks RFC 0055 fixes rather than living in either alone: under the
  **admission mutex**, *max-frame validation, then the tenant checks*
  RFC 0055 defines; then, under the **journal mutex** and in the same hold as
  everything else that reads journal state, *the terminal-rotation check,
  then the bound*, then the rotation decision, the reservation and the
  append. So an oversize payload against a terminal WAL is `TooLarge`, a
  legal payload against a terminal WAL is the terminal classification
  without a `reclaim_state()` read, and only a legal payload against a healthy
  WAL reaches the reservation. **Terminal still precedes backpressure** —
  both are read in the same journal hold, terminal first, so a terminal WAL
  is never reported as a bound. That check has a named source: `Journal`
  gains `fn rotation_state(&self) -> RotationState` (`Healthy`, `Retrying {
  attempts }`, `Terminal`), a cheap categorical read with no snapshot struct
  on the append path, taken **under the journal mutex in the same hold as
  the reservation and the append** — one acquisition, so it can neither race
  outside the mutex nor be confused with `reclaim_state()`. RFC0053.1 covers
  the combined case.

There is no rollback path, deliberately: truncating an appended frame is a
second way to corrupt the tail, so the only safe reservation is one taken
before the write. Two append failures need their own rules. A failure the WAL
rolls back cleanly leaves the figure and the refusing state unchanged. A failure after bytes reached disk whose
best-effort truncate-back *also* fails is not something the counter alone
can absorb: the frame's full framed length is added to the unreclaimed
figure at once — an over-count is safe, an under-count admits past the
bound.

**Repeated rollback failures are bounded.** The torn bytes are *inside* the
bound for as long as they exist: the failed frame's full framed length is
added, and at a restart the heal truncates them before `rebuild_ledger()`
runs, so the rebuilt figure excludes what is no longer on disk and no
sequence of failed rollbacks grows the frame bytes past the limit.
The bound is configuration, and it has a home: `WalConfig` gains
`unreclaimed_bytes_limit`, and it is the first WAL knob the deployment
surface exposes — `ReceiverSection` carries only `wal_root` today, so it
gains `wal_unreclaimed_bytes_limit` under its existing
`deny_unknown_fields` struct, taking the config file's `${env:VAR}`
substitution (RFC 0020) rather than a bespoke environment path, and the
Helm chart exposes it under the receiver's config block. `validate_config` rejects an explicit value below `segment_size_bytes`: the
smallest useful bound is one full segment, since housekeeping never unlinks
the current segment and a bound smaller than one would refuse before a
rotation could ever reclaim anything — which also puts it above the largest
legal frame, because the segment floor already is. The default is
**derived**, `max(1 GiB, segment_size_bytes)`, so an implicit default can
never fail that validation whatever segment size is chosen (the range allows
up to 2 GiB); at the default segment size that is eight segments, far above
the 16 MiB frame ceiling, a few hours of a busy node's frames and a few
weeks of the incident node's. Whether it should instead scale with the
volume is §7's question; the acceptance run uses the derived default. The rejection is the
contract: ingest keeps accepting while the object store is unreachable until
the declared limit, then refuses with a reason that names the limit it hit.
Crucially, crossing the limit does **not** set RFC 0052's rotation-failure
state — it is a pressure state, not a fault. The converse precedence is
stated too: once the WAL is in RFC 0052's terminal rotation state — which
the timer's own `rotate` can reach after the bound is crossed — the
pre-append check reports that terminal classification — RFC 0052 §3.3's
**server-terminal, client-retryable** class, RFC 0018 §3.2's third: the
client keeps its data and backs off exponentially, and neither
`Retry-After` nor `RetryInfo` is sent, since the server cannot predict when
an operator clears the node — before it consults the bound at all.

**It clears when a housekeeping pass actually removes bytes, not when the
checkpoint advances.** Advancing the sidecar declares frames reclaimable
without reclaiming them, and the bound is measured in bytes still on disk,
so the clearing path is the whole RFC 0052 §3.2 sequence — barrier,
checkpoint, housekeeping — which that RFC's timer drives without an append.

**Admission is per request; the reported state is a latch with a defined
leave condition.** Each reservation is its own decision — `unreclaimed +
framed_len ≤ limit` — so a smaller batch can be admitted while a larger one is
refused, and no batch is ever refused on the strength of an earlier refusal.
The comparison and every accounting update use **checked** arithmetic and
fail closed: a projected total that would wrap is a refusal, never an
admission — saturating would not do, since a sum saturated to `u64::MAX`
still passes `≤ u64::MAX` — and `validate_config` rejects a limit of
`u64::MAX` outright, so the boundary is unreachable from both sides. An
overflowing refusal has no projected total to report, and the shape says so
rather than inventing one: `WalBackpressure::projected` is an
`Option<u64>`, `None` on overflow; the message names the limit and the
pre-reservation total and says the projected total overflows; the
`last_refusal` gauge records only its `pre_reservation` datapoint; and the
entered event carries `ourios.wal.unreclaimed` without a projected value.
The refusal latch and `Retry-After` behave as for any other refusal. Only
`capacity_remaining` (§3.3) saturates, being a report, not a decision.
The *state* §3.3 exports is entered by the first refused reservation and left
by whichever comes first: a housekeeping pass that actually removed a
segment (`removed_segments > 0`) *and* left the unreclaimed total strictly
below the limit — a no-op pass cannot clear it, since a refusal can occur
with the total already below the limit — evaluated by `maintain` under the guard it
holds for `housekeeping_commit`, from the `HousekeepingProgress` that
commit returns (which is how the state leaves with no append arriving), or
an append that is **acknowledged** — `append_batch` returning `Ok` is not
enough, since it returns before the group sync that makes the frame
durable (RFC 0052 §3.7's `CommitOutcome`), and a batch whose covering sync
then fails was never acked and its frame is not durable; so the leave
transition is the ack path, the append *and* its covering sync succeeding,
and the `.left` event says the state left on an acknowledged batch. A
reservation that passes its check but whose append then fails on rotation
or write I/O, or whose sync fails, leaves the *state* unchanged, so
neither the preflight check nor a bare `append_batch` is the leave
transition. The *figure* is another matter, and the two must not be
conflated: an append that reached the segment and whose covering sync
then failed leaves those bytes on disk — the frame is in the segment,
unacknowledged but present, and the ledger rebuild will count it — so the
reservation stays charged until the segment is reclaimed, exactly as for
an acknowledged frame. Only an append that never reached the segment (a
rotation failure, or a write whose truncate-back succeeded) releases its
reservation. Charging a present frame is what keeps the bound honest
about disk; leaving the latch alone is what keeps it honest about acks. The enter
and leave events fire on exactly those transitions, once each, and the gauge
follows them.

**It also needs the timer to be able to force a rotation, or it deadlocks on
its own.** Housekeeping never unlinks the *current* append segment, and a
crossed bound rejects the appends that would trigger size- or age-based
rotation. If an outage's whole backlog sits in that one segment — the normal
case for a low-volume node, whose segments roll on age — then every pass finds
nothing reclaimable, the bound never clears, and no append will ever arrive to
roll the segment. A livelock built out of two individually-correct rules.

So the housekeeping pass, through `CommitCoordinator::maintain`,
**rotates** only in the single-segment livelock, stated as one predicate
over what the coordinator holds after the commit: the refusing state is
set **and its retained cause is `Bytes`** — the latch keeps the cause of
the refusal that set it, so a `Segments` refusal never
rotates, since a rotation at the ceiling would breach it, and a
`TenantCapacity` refusal sets no latch at all — the `HousekeepingProgress` that
`housekeeping_commit` has just returned reports `removed_segments == 0`
**and the pass actually ran** — RFC 0052 §3.2 makes a pass a no-op before
the version-2 witness exists, returning zero counts, and a skipped pass is
"no progress", not "nothing left to reclaim"; forcing a rotation on the
strength of it would rotate a legacy root that has not yet upgraded, so
the trigger requires a pass that planned segments. The trigger also requires that
`reclaimable_now` is false, `current_segment_frame_bytes` equals the whole
`unreclaimed_bytes` — the current segment holds the only unreclaimed
bytes, which is the case no other rule can clear — and the retained count
is below `max_segments`. The trigger is the latch and its cause, never
`unreclaimed_bytes >= limit`, since a per-request refusal can happen with
the live total still below the limit. **A pending rotation fsync is its own trigger, ahead of that predicate.**
RFC 0052 §3.3 keeps a post-rename parent-fsync failure as
`dir_fsync_pending` with origin `Rotation`, discharged by the next `sync`
or `rotate`; under a crossed bound no append reaches `sync`, and the
predicate above cannot call `rotate` either, because a forced rotation
that hit exactly that failure has left an *empty* current segment while
the backlog sits in the closed one — `current_segment_frame_bytes` is no
longer the whole `unreclaimed_bytes`, so the only-holder leg is false for
good and nothing acks again. So, as an **amendment to RFC 0052 §3.3 and
§3.7**, the housekeeping pass discharges a pending `Rotation`-origin fsync
**before** it evaluates the forced-rotation predicate, whenever
`ReclaimState` reports the obligation outstanding — a discharge, not a new
segment. A failed discharge draws on the same rotation retry budget
it does on the append path and can reach the terminal state, which the
next request reports; a successful one lets the next append ack. The
predicate then runs as stated, on a WAL that owes nothing. RFC0053.1
asserts the wedge: a forced rotation whose parent fsync fails is
discharged by a later pass with no append arriving. The rotation itself runs **under the journal mutex alone** — the guard
`maintain` still holds for the commit — with the **admission mutex
released**, matching the lock order RFC 0055 states: the journal hold is
what keeps an append from interleaving between the commit's verdict and
the rotation, and holding admission across directory work would queue
every request behind the rotation's fsync.
That last
condition needs a state surface the inherited `ReclaimState` lacks —
`unflushed_bytes` resets on every sync, so it cannot tell a synced current
segment from an empty one — and this RFC adds one field to it:
`current_segment_frame_bytes`, maintained by the same append accounting and
seeded from the healed newest segment by the post-recovery ledger rebuild. The predicate is the only-holder case exactly. Rotation is a WAL operation rather than an
append, so backpressure does not
block it; the segment closes, the next pass can reclaim it, and the state
clears. Nothing is acked by that rotation, so the no-ack-on-refusal property
is untouched.

A second way the state can persist, which is *not* a deadlock and is handled
differently: a stale tenant horizon keeps the segments holding that
tenant's uncovered frames ineligible. The unlink rule is per segment
and per tenant, so this is not "the whole WAL is ineligible": segments holding only *other* tenants' covered
frames are still removed, and the bound stays crossed only when the
segments that remain are the ones the lagging tenant's frames hold — a
node where one tenant's frames are spread across the backlog, which is the
fixture RFC0053.1 builds for this leg. In that case housekeeping removes
nothing further and the bound stays crossed even though the object store
recovered. That is not a bug to paper over — a tenant whose
snapshot is not advancing is a real problem — but it must be *visible* rather
than presenting as an unexplained refusal, which is why RFC 0052 §3.5 exports
the floor and its lag alongside the bytes. RFC0053.1 asserts the healthy-store
resume; the held-floor cases are normative — a tenant with no valid
snapshot pins the floor and is reported `Pinned`, a valid but lagging
horizon is `Min` with `lag_bytes` reported beside it and in both the bound stays crossed for
a batch of that size after the store returns, while the refusal latch
itself still follows §3.1 and may leave on a smaller successful append
(RFC0053.1) — and only the escalation policy is the §7 question.

### 3.2 Amendments requested of RFC 0052

The bound above is measured and enforced through WAL surfaces that do not
exist today. They are **RFC 0052's** to specify; this RFC asks for them and
says only what it needs from each, and the wording is filed on
[`#798`](https://github.com/jensholdgaard/ourios/pull/798). If RFC 0052
cannot land without them, they were never this RFC's.

- `Journal::framed_len`, `Journal::rotation_due`, and a `RotationDecision`
  token on `append_batch` — so the reservation and the append act on one
  evaluation of a predicate whose age half moves with wall time.
- A `RotationKind` argument on `Journal::rotate` (discretionary versus
  owed) and a typed result — so a refusal at the segment ceiling is an
  outcome rather than a rotation failure, and an owed rotation always has
  room.
- `.wal.seal` and the torn-tail heal — the durable marker for an append
  whose rollback also failed, and the narrowed recovery rule that reads it.
- `rebuild_ledger()` / `remeasure_unreclaimed()` as the restart seed for
  the unreclaimed figure, failing closed on a scan error.
- `HousekeepingProgress.removed_segments`, `reclaimable_now`, and a
  `ReclaimSchedule` handle — what `Retry-After` and the latch's leave
  condition are computed from.
- Retirement of `cadence_failed`, and `failure_generation` if RFC 0052
  still needs it.
- A ceiling on **sealed** segments, reached only by a fault and so
  terminal, unlike `max_segments`: the segment cap governs *discretionary*
  rotations and never becomes a fault, while a seal happens only when an
  append's write *and* its truncate-back both failed. It is the one unit
  this RFC cannot bound, an owed rotation entering `max_segments` ungated
  (§3.1).

Unwind safety, publication frontiers and the audit-durability amendment
that shared this document are RFC 0054, RFC 0055 and RFC 0056; each states
its own dependency on this one.

### 3.3 Telemetry

RFC 0052 §3.5 already exports the measurement this RFC's bound is taken on and
the floor state that explains a bound that will not clear. This RFC adds the
backpressure state itself, named for what it is: a **refusal latch** (set
by a refused reservation, cleared as §3.1 defines), the limit and
measurement at the last refusal, and a separate `capacity_remaining` gauge,
saturating at zero — the live figure may legitimately sit above the limit
after a lowered limit or a conservative over-count —
— because admission is per request, a cleared latch at 900 of 1,000 bytes
still refuses a 200-byte batch, and a dashboard must not read `0` as "every
request is admitted" — and a log event on entering and leaving the latch.
Transitions can happen entirely on request paths between ticks (a refusal
and then a smaller successful append), and a queue that had to be both
lossless and bounded in exactly that failure mode would be the wrong
shape — so there is no queue: the coordinator **emits the enter and leave
events itself, synchronously at the transition, under the journal mutex**.
The order is the mutex order and every transition emits exactly once at a
real call site; RFC 0052 §3.5's tick-based projection is amended by this
RFC to exclude the **backpressure refusal latch** explicitly. The two
latches are distinct and this RFC never merges them: the refusal latch is
§3.1's, set by a refused reservation and left by reclamation or an
acknowledged append, and it is what the tick-based projection must stop
carrying. So there is one emitter per
transition and never two events for one. RFC0053.5 counts every transition.

The contract is enumerated here, as RFC 0009 §3.6 does, rather than
deferred to the registry bump; the names are proposals for the shared
`ourios-semconv` registry (one bump with RFC 0052's, through that
repository's review) and nothing is hand-written in the code:

| Signal | Instrument | Unit | Attributes |
|---|---|---|---|
| `ourios.wal.backpressure.refusing` | gauge (int) | `1` | `ourios.wal.backpressure.cause` ∈ {`bytes`, `segments`} (`1` while the refusal latch is set, on the cause that set it; a `TenantCapacity` refusal is capacity and never sets the latch) |
| `ourios.wal.backpressure.limit` | gauge | `By` | — (the byte limit; the segment and tenant limits are the gauges below, in their own units) |
| `ourios.wal.backpressure.last_refusal` | gauge | `By` | `ourios.wal.backpressure.cause` = `bytes`, `ourios.wal.measurement` ∈ {`pre_reservation`, `projected`} |
| `ourios.wal.backpressure.last_refusal.segments` | gauge (int) | `{segment}` | `ourios.wal.backpressure.cause` = `segments` (the retained count at the last segment refusal) |
| `ourios.wal.tenant_capacity.last_refusal` | gauge (int) | `{tenant}` | — (the held count at the last `TenantCapacity` refusal; capacity, not a backpressure cause) |
| `ourios.wal.capacity_remaining` | gauge | `By` | — (saturating at zero; byte headroom) |
| `ourios.wal.segments.usage` | gauge (int) | `{segment}` | `ourios.wal.segment.state` ∈ {`retained`, `free`, `over_cap`} — `retained + free = limit + over_cap`, with `free` saturating at zero and `over_cap` carrying the overrun an owed rotation may create |
| `ourios.wal.segments.limit` | gauge (int) | `{segment}` | — (`max_segments`) |
| `ourios.wal.backpressure.entered` / `.left` | log events | — | `ourios.wal.backpressure.cause`, plus the cause's measurements: `ourios.wal.limit` (By), `ourios.wal.unreclaimed` (By), `ourios.wal.measurement` for `bytes`; the count and limit for `segments` |

That pair follows the OTel instrument-naming rule for a measured
amount out of a known total — `entity.usage` with a `state` attribute
whose values sum to `entity.limit`, beside `entity.limit` itself, as
`system.memory.usage` / `.limit` do — rather than carrying the limit as an
attribute of the usage gauge, which reads as a dimension rather than a
total.

`error.type` continues to carry the failure class on existing counters
rather than spawning per-error metrics: the refused batch is counted on
the ingest counter with `error.type` ∈ {`wal_backpressure_bytes`,
`wal_backpressure_segments`, `wal_tenant_cap`} — the last a capacity
refusal, not backpressure, per §3.1. The names follow the registry's rules — `ourios.*` is the system
namespace, dotted, snake_case — and go through `ourios-semconv` like the
rest.

## 4. Alternatives considered

**Make the sink ceiling a hard cap instead of adding a WAL bound (§3.1).**
Worth stating because `SINK_CEILING_BYTES` looks like the natural place: the
drain loop exits when `flush_largest()` fails and buffers the record anyway,
so the ceiling is a hint and memory grows unbounded when the store is down.
Blocking there instead would apply backpressure in the wrong unit — buffered
Parquet bytes rather than unreclaimed WAL bytes — and would stall ingest on a
condition that does not threaten durability.

But rejecting it as the *signal* is not the same as leaving it alone. Both
sinks retain past their ceilings whenever a store flush fails, so an outage
grows memory without bound and can OOM the process **before** the WAL bound
is anywhere near reached — in which case §3.1 never fires and this RFC has
bounded the wrong resource. That makes it a prerequisite, not a neighbour.

**The decision is not open, though — it is unimplemented.** RFC 0014 §3.4
is accepted and specifies exactly this: the sink tracks buffered bytes
against a *hard* ceiling, force-flushes the largest or oldest partitions
under soft pressure, and when early flush cannot keep up `emit` **blocks**
until an in-flight flush frees memory, so the buffer never exceeds the
ceiling. Today's retention past the ceiling is a gap against that contract,
not a gap in the design. So this RFC does not decide
block-versus-spill-versus-drop — RFC 0014 §3.4 decided it — and states
instead the scope of its own claim: **it bounds local disk, not process
memory.** §3.1's bound is the operative limit for the WAL directory and
nothing else, the OOM path during a long outage stays open, and the
incident's end-to-end property — ingest that stays bounded and keeps
refusing rather than dying — needs both RFCs. The sink decision is
therefore the **blocking follow-up for the end-to-end claim**: §6 keeps it
as the gate on `validated` and §5 asserts only the disk bound, with
RFC0053.1's unreachable-store leg run long enough to show which limit is
hit first, which is what makes the follow-up concrete.

**Backpressure as a rotation-failure state.** Rejected: it would reuse RFC
0052's terminal-state reporting for a condition that is not a fault and
clears on its own, telling clients a node is broken when it is merely full.
The two stay distinct so each advertises the true remedy.

## 5. Acceptance criteria

> **Scenario RFC0053.1 — Backpressure is a stated limit, and clears itself**
> - **Given** an unreachable object store and a configured local retention
>   bound, on a node running RFC 0052 — the scenario asserts the **local
>   disk** bound only, memory growth in the sinks being §4's separate
>   follow-up and §6's gate on `validated`
> - **When** ingest continues until a reservation would exceed the bound
> - **Then** earlier batches were accepted and acked, and the rejecting batch
>   is refused with a reason naming the bound it hit and that cause's measurements (for bytes, the
>   pre-reservation total, and the projected total when representable or
>   the overflow indication otherwise), on both transports, and
>   a `Retry-After` computed from the reclaim schedule §3.1 states —
>   asserted with the barrier and housekeeping cadences set apart, against
>   the tasks' published due instants: the time to the next pass when
>   `reclaimable_now` is set, the next pass plus one housekeeping cadence
>   when the latch is set with nothing removed, and the next barrier plus
>   one housekeeping cadence otherwise; rounded up, never below one second
>   — **not** a value derived from the limit, which yields no delta-seconds
> - **And** on HTTP the reason is a binary protobuf `google.rpc.Status` body
>   (`application/x-protobuf`, whatever the request's encoding) with
>   `Retry-After` as a header, and on gRPC the status code is `UNAVAILABLE`
>   with a `RetryInfo` detail — a bare status code with an empty body, or
>   any other gRPC code, fails this scenario
> - **And** the refused batch is **not present in the WAL** — asserted by
>   replaying after a restart, so a post-append check that left the frame
>   behind fails here
> - **And** the bound fires on bytes accumulated *above* a stalled
>   checkpoint, not only below it: a run where the checkpoint never advances
>   must still reach the limit
> - **And** the refusal transition itself does **not** set the
>   rotation-failure state; a forced rotation that later fails may enter
>   RFC 0052's retrying or terminal state on its own account
> - **And** when the store returns and no tenant pins the floor (RFC 0052
>   §3.2), ingest resumes **with no append and no restart** — the
>   timer-driven sequence reclaims, which is the only path that can clear a
>   state that rejects every append
> - **And** when a tenant pins the floor (no valid snapshot), or a valid
>   horizon lags so that the bytes cannot be reclaimed, the bound stays
>   crossed for a batch of that size after the store returns, by design; the
>   floor is reported `Pinned` in the first case and `Min` with a nonzero
>   `lag_bytes` in the second, rather than as an unexplained refusal, and the latch itself
>   still leaves on a smaller successful append as §3.1 defines
> - **And** when the whole backlog sits in the current append segment, the
>   timer's forced rotation lets the next pass reclaim it, so the state clears
>   without an append ever arriving; the same latch set by a `Segments` or
>   `TenantCapacity` refusal (which sets no latch at all), or with closed segments holding part of the backlog,
>   or with the retained count at `max_segments`, forces no rotation
> - **And** a fitting frame whose segment has aged past `segment_age_secs`
>   reserves a segment slot: below the ceiling it is admitted and rotates,
>   at the ceiling it is refused with the `Segments` cause — once and then
>   admitted after the next pass when a segment was reclaimable, and on
>   every retry while nothing is reclaimable
> - **And** under concurrent submits the live unreclaimed total — admitted
>   frame bytes not yet reclaimed, not a cumulative sum reclamation would
>   legitimately let grow — never exceeds the limit, so two batches cannot
>   both observe room
> - **And** an oversize payload against a terminal WAL is `TooLarge`, and a
>   legal payload against a terminal WAL is the terminal classification with
>   no `reclaim_state()` read — the admission order §3.1 states
> - **And** the lagging-floor leg above is built so the surviving segments
>   are the ones holding the lagging tenant's frames; segments holding only
>   other tenants' covered frames are reclaimed even then
> - **And** a housekeeping pass skipped for a missing version-2 witness
>   forces no rotation: a legacy root that has not yet upgraded is left
>   alone
> - **And** `max_segments < 1` is rejected at config validation, naming the
>   reason: a ceiling of zero admits no retained closed segment, so the
>   first closed segment refuses every later discretionary rotation
> - **And** the segment gauge stays consistent across the overrun an owed
>   rotation creates: `free` saturates at zero, `over_cap` carries the
>   excess, and `retained + free = limit + over_cap` holds while the
>   rotation is outstanding and after the next pass reclaims
> - **And** no rotation-failure state is entered from the **segment cap**
>   at all (§3.2's sealed-segment ceiling is a fault and is not this cap):
>   `Terminal` is reached only by an exhausted rotation retry budget, and
>   no `Deferred` state exists
> - **And** a forced rotation whose post-rename parent fsync fails does not
>   wedge the node: a later pass discharges the pending `Rotation`-origin
>   fsync before it evaluates the predicate, with no append arriving, and
>   the next append acks
> - **And** under repeated rollback failures the byte accounting stays
>   within the limit — the torn bytes count inside it — while the fixed
>   overhead outside it is capped for **discretionary** rotations only:
>   at the ceiling `max_segments` refuses a reservation needing one,
>   naming it, and admits one that fits the current segment. The seals a
>   rollback failure leaves sit outside that cap — each forces an owed
>   rotation, which enters it ungated (§3.1) — so their ceiling is §3.2's
>   request of RFC 0052, and this leg asserts only the frame bytes

> **Scenario RFC0053.4 — No acknowledged record is lost with backpressure
> live**
> - **Given** a node killed with `SIGKILL` mid-batch while reclamation,
>   rotation retry **and backpressure** are all live, with the bound small
>   enough that the kill lands in the refusing regime
> - **When** it restarts and recovery completes
> - **Then** every acknowledged record is present in Parquet, and no refused
>   batch is present in Parquet or in the WAL
> - **And** the restarted node's admission starts from the rebuilt unreclaimed
>   figure, so a batch the bound refused before the kill is refused after it
> - **And** a restart whose ledger rebuild fails does not come up: no coordinator,
>   no listener, and no batch admitted

> **Scenario RFC0053.5 — The backpressure state is observable**
> - **Given** a node that enters and then leaves the refusing state
> - **When** metrics are collected and logs are read across both transitions
> - **Then** the refusal-latch gauge with its cause, the limit and
>   measurement at the last refusal per cause (bytes with its measurement,
>   the retained count for segments), the held count at the last
>   `TenantCapacity` refusal on its own gauge, and `capacity_remaining` — equal to `max(limit − unreclaimed,
>   0)` over the live unreclaimed figure at the moment of the read, not a
>   stale value — are present in the exported stream under registry names
> - **And** entering and leaving each emit exactly one log event, named from
>   the registry, on the transitions §3.1 defines and on no other tick
> - **And** a `weaver registry live-check` over the emitted events passes,
>   since an event the tests never emit is an event the check never sees

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
  livelock case. A fourth leg is concurrent rather than sequential: a
  `proptest` driving many tasks that submit arbitrary-sized batches at once
  against a small limit, asserting after every admission that the live
  unreclaimed figure — admitted minus reclaimed, since housekeeping
  legitimately lets a cumulative sum grow — never exceeds the limit. It is
  the only test a reservation taken outside the journal mutex fails, the
  sequential flow passing it. A fifth leg fills the limit and submits an
  oversize payload, asserting `TooLarge` with no `reclaim_state()` read and
  no append — the reversed-check regression. The sequence is driven through
  **both** adapters: on HTTP the protobuf `Status`,
  `application/x-protobuf` and the exact `Retry-After`; on gRPC
  `UNAVAILABLE` with the `RetryInfo` detail carrying the same seconds. A
  sixth leg pins a tenant's floor (a lagging or invalid horizon) and asserts
  that refusal persists after the store returns and the state is reported
  `Pinned`, as RFC0053.1 makes normative.
- **Configuration (RFC0053.1's precondition)** — resolver tests for an
  explicit value and an `${env:VAR}`-substituted one, plus a Helm render leg
  that the chart value reaches the config file; a missing field under
  `deny_unknown_fields` would otherwise leave the limit at its default while
  every scenario passed. The derived default at a 2 GiB segment and the
  invalid below-segment value are `WalConfig` validation tests, since the
  resolver has no segment-size input (`wal_config()` hardcodes it).
- **No loss (RFC0053.4)** — extends RFC 0052's `SIGKILL` crash-recovery
  extension rather than adding a parallel one, in **both** restart shapes —
  a clean replay and a torn newest tail, since the existing crash test has
  no torn tail and a rebuild hook placed only on the heal path would pass
  it — with a small backpressure bound configured so the kill lands in the
  refusing regime, and a
  post-restart append that asserts the rebuilt figure still refuses, and
  an exact-figure assertion against a fixture with several closed segments,
  a header-only current segment (contributing nothing) and a torn newest
  tail — a refused-stays-refused check alone would pass a rebuild that
  counted headers or torn bytes; a fault-injected leg makes the rebuild's scan fail and asserts
  startup refuses to construct the coordinator.
- **Telemetry (RFC0053.5)** — the in-memory metric exporter pattern used for
  the ingest instruments, driving one enter and one leave and asserting the
  latch gauge, the last-refusal figures, `capacity_remaining` against the
  live headroom, and exactly one event per transition — including a refusal
  and a smaller successful append inside one tick; plus the `weaver registry
  live-check` pass over those events.

Maturity, per `docs/rfcs/README.md`: `green` is RFC0053.1, .4 and .5 all passing in
CI with the unit, property and corpus suites green, and this RFC touches no thesis gate in `docs/benchmarks.md` §7. It does
**not** proceed to `validated` on those alone: §4 records that the sinks can
exhaust memory before the WAL bound fires, so a green run could mark a bound
validated that is never the operative limit. `validated` therefore also
requires **RFC 0014 §3.4's hard ceiling to be implemented** (§7 tracks the
confirmation, not a decision) and RFC0053.1's
unreachable-store leg, run under default configuration for long enough to
show the WAL bound refusing **before** either sink exceeds its ceiling. Until
that decision lands the RFC stops at `green`, and says so.

## 7. Open questions

- [ ] Whether the byte bound's default should scale with the volume rather
      than be the fixed 1 GiB §3.1 sets. The incident node held 42 MB over
      five days, so a default tuned for it would be far too small for a busy
      node; a fraction of the volume may be the honest default.
- [ ] Confirm RFC 0014 §3.4's hard ceiling is implemented — the blocking
      `emit` that keeps buffered bytes under the ceiling. Not a design
      question (§3.4 decided it; today's sinks retain past it when a flush
      fails), but §4 and §6 gate `validated` on it, so it must land before
      this RFC can be more than `green`.
- [ ] Whether a stale tenant floor blocking reclamation indefinitely should
      itself escalate (a second, louder state) or stay a visible metric an
      operator alerts on. §3.1 makes it visible; it does not decide.
- [ ] Whether backpressure should be per-tenant rather than per-node. It is a
      local-disk property, so per-node is the natural unit, but a single noisy
      tenant can then refuse every other tenant's writes.

## 8. References

- RFC 0052 — WAL reclamation and quiesce recovery: the stage this one depends
  on, for the timer, the barrier exclusion, `ReclaimState`, `RetainFloor`,
  and the terminal-only classification of a rotation failure.
- Issue #791 — the incident; this RFC is its "no stated limit" half.
- RFC 0018 §3.2 (retryable error mapping) — the reasoning for `503` on an
  unacked batch; §3.1 takes `UNAVAILABLE` over its `RESOURCE_EXHAUSTED`
  option.
- RFC 0014 — the record sink and its flush triggers; its §3.4 sink memory
  ceiling is the governing contract for the memory half of the bound, and
  §4 and §6 make its implementation a prerequisite for the end-to-end
  claim.
- `CLAUDE.md` §3.4 (WAL-before-ack), §6.3 (observability of ourselves).
- `docs/hazards.md` #3 (WAL durability versus latency), #4 (small files).
