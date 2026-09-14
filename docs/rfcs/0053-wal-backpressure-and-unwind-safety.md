---
rfc: 0053
title: WAL backpressure and unwind safety
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-09-13
supersedes: —
superseded-by: —
---

# RFC 0053 — WAL backpressure and unwind safety

> **Status note.** `specified` — §5 and §6 are written; no gate runs
> until RFC 0052 lands. **Stage 2 of two**, split out of RFC 0052 at the
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
carrying the limit, both measurements — the pre-reservation unreclaimed total
and the projected total including this batch's `framed_len`, since a
per-request refusal can happen with the live total still below the limit
and a single "measurement" would be ambiguous — and the delay to advertise; `IngestFailure::classify` maps it to a new `Backpressure` outcome, which that
exhaustive match then forces both transports to handle; both render `503` /
`UNAVAILABLE` with the limit named in the `Status` message. `503` because the
batch was not acked — RFC 0018 §3.2's reasoning — and a distinct outcome
rather than reusing `Unavailable` because the remedy differs: waiting genuinely
helps here.

The wire shape is stated, because an empty body is how #791 hid for eight
hours. On HTTP the rejection is the `google.rpc.Status` body the OTLP spec
requires and #794 established — binary protobuf with
`application/x-protobuf` whatever the request's encoding, because the
Collector's exporter decodes every failure body as protobuf regardless of
`Content-Type`, with the message naming the limit and the measurement — and
`Retry-After` is a response header. On gRPC it is the `Status` message plus a
`RetryInfo` detail carrying the same delay. A bare status code with no body,
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
delay is computed from the schedule at the refusal: when RFC 0052 §3.2's
eligible queue is **non-empty**, the next pass can remove bytes and the
delay is the time to that pass (at most `housekeeping_secs`); when it is
**empty**, the delay is the time to the next barrier plus one housekeeping
cadence, since nothing becomes reclaimable before the barrier runs. The
coordinator is constructed with both cadences and the barrier task's next
due time, and reads the queue's emptiness under the journal mutex it
already holds at the refusal. The value is a **non-binding hint**: whether
the pass frees enough depends on the store and the tenant floor, and a
client that arrives earlier is simply refused again with a fresh hint.
Nothing finer would be honest; a backoff estimator would be inventing a
prediction the server cannot make.

The delay travels *in the error*. The transport mappers see only a
`&ReceiveError` and their handler state knows nothing of the WAL's cadence, so
the coordinator — constructed with both cadences alongside the bound —
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

Two boundaries of that figure are stated so the reservation and the
measurement cannot drift apart. **It is over frame bytes.** Segment headers
(24 B each) are outside the measurement and the reservation alike: a pre-write
rotation adds one, the coordinator cannot know whether an append will rotate,
and the overhead is one header per surviving segment — which does grow by
24 B per rotation while the store is down, a quantity the limit's
segment-size floor makes negligible against the bound and which no frame
can inflate. This is a **segment-frame admission bound, not a directory
cap**: the `CHECKPOINT` and `RECLAIM` sidecars, their temp files, the
`.wal.seal` sidecars §3.1 introduces below and their `.wal.seal.partial`
temps, and rotation partials awaiting RFC 0052's sweep are all outside it,
so `disk_bytes` can exceed the bound by those as well as by the headers —
each of them is either a fixed-size record or debris the sweep removes, and
the seal count is bounded below. **It survives a restart
by being rebuilt, not persisted, in the same unit.** Once replay and heal
have settled the newest segment's tail — a torn frame there is truncated by
heal and must not be counted — the figure is initialised as the sum over
every surviving segment, closed and current, of its file size **less the
24-byte segment header**, so the rebuilt number is frame bytes like the live
one and matches what the reservation adds to it. The hook is RFC 0052 §3.7's
`Wal::remeasure_unreclaimed()`, called after every successful replay and
after the heal when there was a torn tail to heal — not at `Wal::open`,
which runs before either and would count torn bytes, and not only on the
heal path, which a clean replay never enters — and the coordinator is
constructed after recovery, so the seed completes before any append is
admitted and a node restarted mid-outage resumes refusing at the same bound
rather than admitting from zero. The same hook seeds the current segment's
frame bytes (§3.1's rotation trigger) from the healed newest segment. And
it fails closed: a listing, stat or header error from the remeasure fails
startup before the coordinator or any listener is constructed, because a
partial or zero seed is precisely a node that admits past its bound.
RFC0053.4's restart asserts both.

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
- **The frame length comes from the journal, not from `payload.len()`.** The
  coordinator sees only the encoded payload; `Wal::append` prepends a
  12-byte frame header before it accounts the bytes, so a reservation taken
  on the payload length alone under-reserves by the header on every batch.
  `Journal` therefore gains `fn framed_len(&self, payload_len: usize) -> u64`,
  and the WAL's own append accounting is rewritten to call the same function,
  so the reservation and the write cannot disagree and a test double reports
  the same number the real WAL would.
- **An oversize batch is still `TooLarge`, never backpressure — and the
  coordinator makes that so.** Because the reservation now runs before
  `append_batch`, a refused request never reaches `Wal::append`'s own
  `MAX_FRAME_BYTES` check, so the ordering cannot be left to the WAL. `Journal`
  exposes `fn max_frame_bytes(&self) -> usize`, and the coordinator rejects
  `payload_len > max_frame_bytes()` as `TooLarge` before it reads
  `reclaim_state()` at all; the WAL's own check remains as the backstop.
  The full admission order is one sequence, stated once: **max-frame
  validation, then the terminal-rotation check, then the bound** — so an
  oversize payload against a terminal WAL is `TooLarge`, a legal payload
  against a terminal WAL is the terminal classification without a
  `reclaim_state()` read, and only a legal payload against a healthy WAL
  reaches the reservation. The terminal check has a named source: `Journal`
  gains `fn rotation_state(&self) -> RotationState` (`Healthy`, `Retrying {
  attempts }`, `Terminal`), a cheap categorical read with no snapshot
  struct on the append path, taken under the journal mutex immediately
  before the reservation, so it can neither race outside the mutex nor be
  confused with `reclaim_state()`; the test doubles implement it. RFC0053.1
  covers the combined case.

There is no rollback path, deliberately: truncating an appended frame is a
second way to corrupt the tail, so the only safe reservation is one taken
before the write. Two append failures need their own rules. A failure the
WAL rolls back cleanly — the truncate-back succeeds — leaves the figure and
the refusing state unchanged. A failure after bytes reached disk whose
best-effort truncate-back *also* fails is not something the counter alone
can absorb: the frame's full framed length is added to the unreclaimed
figure at once (an over-count is safe, an under-count admits past the
bound; the next `remeasure_unreclaimed()` corrects it), **and the segment
is sealed with a durable marker** — the WAL refuses further appends into
that segment, writes the seal to `<uuid>.wal.seal.partial`, fsyncs it,
renames it to `<uuid>.wal.seal` and fsyncs the parent — the rename is what
makes a seal all-or-nothing, so a crash mid-write leaves a `.partial` that
is debris, never a seal that half-verifies — and only then rotates through
RFC 0052 §3.3's retry path into a fresh segment, since appending after a
partial frame would let recovery consume later bytes as part of it. The seal is **versioned and
segment-bound**: it carries a format version, the segment's own UUID, the
last good length, and a checksum of those three, so a stale or corrupted
seal cannot authorise anything. If the seal write itself fails the WAL
enters the terminal rotation state: it can neither repair nor mark the
segment. That state needs no durable discriminator: the WAL is terminal, so
the segment stays the *newest* one, and the frame at its EOF was never
acknowledged — `append` returned the error before any ack — so a restart
that heals it as an ordinary newest-segment torn tail loses nothing, and a
restart is exactly what RFC 0052 §3.3 says clears the terminal state. A
seal that *succeeds* is followed by the rotation, and only then does the
torn frame sit at the EOF of a *closed* segment; there RFC
0052's replay heal is extended, as an amendment from this RFC, to **exactly
the sealed shape** and nothing looser: the seal must verify, must name
this segment, its length must fall on a frame boundary of the frames
replay has already validated, and the bytes past it must be exactly one
partial frame — on any mismatch the segment is RFC0008.5 corruption and
recovery halts, never truncates. An unmarked closed-segment tail stays
fatal as well, because an in-memory "sealed" state cannot tell rollback
debris from a torn acknowledged frame after a restart, and healing on shape
alone would lose data exactly where the contract says to halt.

**The seal is durable before the rotation is, so a crash between the two
is a stated shape, not an undefined one.** After the parent fsync and
before the rotation completes, the sealed segment is still the *newest*
segment, and RFC 0008's recovery treats a newest-segment tail as
heal-able on shape alone. With a seal present that rule is narrowed, not
widened: a newest segment whose seal verifies is healed to the seal's
length under exactly the closed-segment checks above, and the seal is
consumed; a newest segment with no seal keeps RFC 0008's existing
torn-tail heal; a newest segment with a seal that does *not* verify halts
as corruption, since a seal that exists but disagrees with its segment is
evidence of a state the process did not reach cleanly, and a `.partial`
seal beside any segment is debris and never consulted. The heal itself is
crash-idempotent by an explicit third shape: a process can die after the
truncate and before the seal is consumed, so a verifying seal whose length
*equals* the segment's length — no bytes past it — is the already-healed
segment, and recovery consumes the seal and continues; only a seal whose
length falls inside the segment demands the one-partial-frame shape. The
rotation that was owed is then performed on the `Rotation` origin by the
**post-recovery step** — the same step that calls `remeasure_unreclaimed()`,
after replay and heal and before the coordinator is constructed — not by
`Wal::open`, which `serve` runs *before* `recovery::recover` and which
therefore has neither the replay-validated boundaries nor the remeasured
figure the rule depends on; so a restart never appends into a sealed
segment either.

**Seal removal is an amendment to RFC 0052 §3.7, stated here because that
section's sweep knows only `.wal.partial` and segments.** A seal belongs
to its segment: the per-segment ledger entry (RFC 0052 §3.2) records that
the segment is sealed, live sealing sets it, and `remeasure_unreclaimed()`
seeds it at recovery from the seals it finds — a seal whose segment is
gone is an orphan and is pushed onto the same seeded debris list as
partials, together with every `.wal.seal.partial`. When housekeeping
unlinks a sealed segment it unlinks the **segment first, then the seal**,
in the same step: a crash between the two leaves an orphan seal, which the
next pass sweeps, whereas the other order would leave a segment without its
seal — an unmarked closed tail, which §3.1 says halts recovery. Each is
counted as its own unlink against the per-pass cap, under the same
parent-directory fsync; orphan seals and `.partial` seals are
swept on every pass regardless of the checkpoint precondition, exactly as
partials are, so a restart with no checkpoint yet still clears them.
RFC0053.4 covers the crash window, the orphan and the restart-before-
checkpoint case.

**Repeated rollback failures are bounded, in two units.** The torn bytes
are *inside* the frame-byte bound for as long as they exist: the failed
frame's full framed length is added at the seal, and at a restart the heal
truncates them before `remeasure_unreclaimed()` runs, so the rebuilt figure
excludes what is no longer on disk and no sequence of seals can grow the
frame bytes past the limit.
What sits *outside* it is a 24-byte header and a fixed-size seal per
sealed segment, and their number is capped: `WalConfig` gains
`max_sealed_segments` (default 8, a WAL-internal knob, not on the
deployment surface), the ledger counts sealed segments still on disk, and
sealing the segment that would exceed the cap puts the WAL in the terminal
rotation state instead — a disk that keeps failing writes *and* their
truncate-back is a fault, not pressure, and it is classified as one. The
count is exported (`ourios.wal.sealed_segments`, §3.3), housekeeping
reclaims a sealed segment like any closed one, and RFC0053.1's last leg
asserts the cap.

The bound is configuration, and it has a home: `WalConfig` gains
`unreclaimed_bytes_limit`, and it is the first WAL knob the deployment
surface exposes — `ReceiverSection` today carries only `wal_root`, with the
other WAL settings hardcoded in `wal_config()`, so the section gains
`wal_unreclaimed_bytes_limit` under its existing `deny_unknown_fields`
struct, `wal_config()` resolves it, the value takes the config file's
`${env:VAR}` substitution (RFC 0020) rather than a bespoke environment path,
and the Helm chart exposes it under the receiver's config block.
`validate_config` rejects an explicit value below `segment_size_bytes`: the
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
pre-append check reports that terminal classification, with no
`Retry-After`, before it consults the bound at all. Backpressure never masks
a state no retry can clear.

**It clears when a housekeeping pass actually removes bytes, not when the
checkpoint advances.** Advancing the sidecar declares frames reclaimable; it
does not reclaim them, and the bound is measured in bytes still on disk. So
the clearing path is the whole RFC 0052 §3.2 sequence — barrier, checkpoint,
housekeeping — which that RFC's timer can drive without an append.

**Admission is per request; the reported state is a latch with a defined
leave condition.** Each reservation is its own decision — `unreclaimed +
framed_len ≤ limit` — so a smaller batch can be admitted while a larger one is
refused, and no batch is ever refused on the strength of an earlier refusal.
The comparison and every accounting update use **checked** arithmetic and
fail closed: a projected total that would wrap is a refusal, never an
admission — saturating would not do, since a sum saturated to `u64::MAX`
still passes `≤ u64::MAX` — and `validate_config` rejects a limit of
`u64::MAX` outright, so the boundary is unreachable from both sides. An
overflowing refusal has no projected total to report, and the shape says
so rather than inventing one: `WalBackpressure::projected` is an
`Option<u64>`, `None` on overflow; the message then names the limit and
the pre-reservation total and says the projected total overflows; the
`last_refusal` gauge records only its `pre_reservation` datapoint for that
refusal; and the entered event carries `ourios.wal.unreclaimed` without a
projected value. The refusal latch and `Retry-After` behave as for any
other refusal. Only
`capacity_remaining` (§3.3) saturates, and it is a report, not a decision.
The *state* §3.3 exports is entered by the first refused reservation and left
by whichever comes first: a housekeeping pass that actually removed a
segment (`removed_segments > 0`) *and* left the unreclaimed total strictly
below the limit — a no-op pass cannot clear it, since a refusal can occur
with the total already below the limit — evaluated by the timer after each
pass under the same mutex (which is how the state leaves with no append
arriving), or an
append that *succeeds*. A reservation that passes its check but whose append
then fails on rotation or write I/O leaves the figure and the state
unchanged, so the preflight check is never the leave transition. The enter
and leave events fire on exactly those transitions, once each, and the gauge
follows them.

**It also needs the timer to be able to force a rotation, or it deadlocks on
its own.** Housekeeping never unlinks the *current* append segment, and a
crossed bound rejects the appends that would trigger size- or age-based
rotation. If an outage's whole backlog sits in that one segment — the normal
case for a low-volume node, whose segments roll on age — then every pass finds
nothing reclaimable, the bound never clears, and no append will ever arrive to
roll the segment. A livelock built out of two individually-correct rules.

So the housekeeping pass, through `CommitCoordinator::maintain` and its
journal mutex (RFC 0052 §3.7; no barrier exclusion is involved, since a
rotation is a WAL operation and not a cut), **rotates** when the refusing
state is set — a reservation has been refused,
which a per-request check can do while `unreclaimed_bytes` is still below
the limit, so the trigger is the latch and never `unreclaimed_bytes >=
limit` — the pass it has just run reported `removed_segments == 0` in its
`HousekeepingProgress` (RFC 0052 §3.7; temp-file cleanup and partial
failures do not count) — and it calls `rotate` even when the current
segment is empty: a post-rename directory-fsync failure installs an empty
segment whose pending fsync only `rotate` or `sync` can discharge, and
under backpressure no append reaches `sync`, so the empty-segment no-op
rule applies *after* the discharge, not instead of the call.
The rotation runs inside `CommitCoordinator::maintain`, and this RFC extends
`HousekeepingProgress` with `forced_rotation: Option<Result<(),
ReceiveError>>` so the outcome — retrying or terminal — surfaces through the
same call the trigger reads `removed_segments` from. That last
condition needs a state surface the inherited `ReclaimState` lacks —
`unflushed_bytes` resets on every sync, so it cannot tell a synced current
segment from an empty one — and this RFC adds one field to it:
`current_segment_frame_bytes`, maintained by the same append accounting and
seeded from the healed newest segment by the post-recovery remeasure (a
reopened WAL has frames in its current segment before any append). No
"current segment is the only holder" predicate is needed: rotating a
non-empty current segment while the bound is crossed and reclamation is
stalled is harmless, happens at most once per pass, and is exactly the move
that breaks the livelock when the current segment *is* the only holder.
Rotation is a WAL operation rather than an append, so backpressure does not
block it; the segment closes, the next pass can reclaim it, and the state
clears. Nothing is acked by that rotation, so the no-ack-on-refusal property
is untouched.

That needs an owner, and RFC 0052 §3.7 provides it: `Journal::rotate(&mut
self) -> Result<(), ReceiveError>`, the object-safe, append-independent
rotation that RFC introduces for its own idle rotation (`Wal::rotate` is
private today). This RFC only widens who calls it. It closes the current
segment and opens a fresh one through the same retried path RFC 0052 §3.3
specifies for an append-driven rotation, so a failure enters the same
bounded-retry state and the same terminal state, reported through the same
typed rotation-failure variant behind the `ReceiveError` boundary the
classifier already handles — not `ReclaimError`, whose variants are
reclamation's and stay so. Before anything else it discharges a
pending parent-directory fsync left in `dir_fsync_pending` by RFC 0052 §3.3's
last failure row — the obligation `sync` would otherwise retry on the next
append, which under backpressure never comes — honouring its origin: a
failed `Open`-origin discharge is an ordinary retryable sync failure outside
the budget, a failed `Rotation`-origin one draws on it, exactly as RFC 0052
§3.3 says for `sync` — and only then applies the
no-op rule: `Ok` without a new segment when the current one holds no frames,
so a timer that calls it unconditionally cannot manufacture empty segments.
The timer reaches it through the coordinator's journal mutex,
exactly as RFC 0052 §3.7 routes `checkpoint` and `housekeeping`, so the
single-writer position still has one owner.

A second way the state can persist, which is *not* a deadlock and is handled
differently: a stale tenant floor holds `min(checkpoint, floor)` down. RFC
0052 §3.2's unlink rule is per segment and per tenant, so this is not "the
whole WAL is ineligible": segments holding only *other* tenants' covered
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
horizon is `Min` with nonzero lag, and in both the bound stays crossed for
a batch of that size after the store returns, while the refusal latch
itself still follows §3.1 and may leave on a smaller successful append
(RFC0053.1) — and only the escalation policy is the §7 question.

### 3.2 Requeue on unwind

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

The duplicate this creates is a new class, and it is stated rather than
handed to recovery. Record and audit publishes write fresh `UUIDv7` object
keys, so a panic *after* the store accepted an object but *before* the publish
returned leaves that object in place and requeues its rows; the next publish
writes them again under a new key, and two query-visible objects carry the
same rows, in the same process. Recovery's replay suppression is a
restart-time mechanism over WAL frames and never sees this. This RFC accepts
it, bounded and counted: at most one extra object per panic, visible through
the `cadence_panic` counter beside the flushed-partition counters, and
asserted by RFC0053.2. The bound holds only because settlement is
**per partition**, stated below: a drained batch spans several record
partitions and audit groups, and requeueing the whole batch after a panic in
the third put would duplicate the two objects already accepted. So each
consuming call removes a partition from the recoverable batch only when its
put has **succeeded**, its requeue has **completed**, or the sink has
**permanently dropped** it under its own policy (the audit sink's
permanent and derivation-failure path, and the record sink's RFC 0025
§3.3 quarantine) — that drop is settlement too, at the same per-partition
boundary, so a later panic neither resurrects a dropped group through the
destructor nor loses it without a defined state. The record sink's
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
one possible duplicate. Making the publish idempotent — a drain-time object
key a requeued batch reuses — is a §7 question rather than part of this
design, because a requeued batch is re-drained together with whatever arrived
since and the key would have to survive that merge.

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
`cadence_failed` covered and this RFC retires. So the coordinator builds
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

**The encode workers are in the contract too.** Under RFC 0052 §3.1 a
worker performs no store I/O: `emit_concurrent` appends each record to the
buffers, and a size- or ceiling-detached partition is registered as in
flight and handed to the sink's off-lock publisher — the age sweep's
`write_ordered` path — while the worker moves to its next record, so
`quiesce` waits on encodes alone. What a worker panic can still drop is
the *unappended remainder* of its mined `Vec`, and RFC 0052 has
`BatchGuard` latch `cadence_failed` for exactly that; with the latch
retired here, the worker's batch takes the same recoverable shape instead —
held in a `RecoverableBatch` whose `Drop` requeues the remainder to the
record sink **before** `BatchGuard` decrements `pending`, so `quiesce` can
never observe idle while records sit in a panicking worker's dropped
iterator. The inner emit path has two transfer boundaries, both explicit:
the worker's guard owns a record only until `append_off_lock` **inserts**
it — the handoff happens inside that call, before its post-append trigger
step, so a panic in the trigger cannot requeue a record the sink already
holds; a permanent drop in `PartitionKey::derive`, which returns before
any insertion, comes back as an explicit `Settled::Dropped` result of the
handoff rather than something the guard has to guess at — and a detached
partition passes from the worker to the off-lock publisher's own
`RecoverableBatch` at the in-flight registration, before the worker
continues, so a panic in the worker after that point cannot touch it and a
panic inside the publisher settles it per partition exactly as the sweep's
batches are. The one-duplicate bound is claimed only under those two
transfers. RFC0053.2 asserts it with a panic inside a worker's encode loop,
and with a panic inside the off-lock publisher of a partition a worker
detached. Requeueing
the batch is worthless if the pool then swallows the next one, so both
halves of worker recovery are stated: a worker whose `emit_concurrent`
panics exits its OS thread today, and a **pool-owned supervisor**
respawns it: the supervisor, not the worker closures, holds the receiver,
so when a worker dies the batches still queued in the `sync_channel` are
drained and requeued and their `Pending` accounting settled rather than
dropped with the last worker's receiver; it respawns only when the join
result reports a panic — never on the normal exit a closed `tx` produces —
and it stops before teardown, so `Drop` can join; the panic is counted;
and `EncodePool::submit` on a **disconnected** pool is not a failure the
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

The unwind arm records an `unmined` entry for **any** panic inside that
loop — inside `ingest_mined`, or after it returned and before the inline
`emit` or the collected `submit` completed — with three things. **The
frame's span**, `FrameSpan { segment, start, len }`: RFC 0008 defines
`WalOffset` as the *post-frame* byte, so the offset `append_batch` returns
cannot locate the frame's start by itself, and the caller computes the
span from what it already holds — `start = offset.byte − (frame header +
encoded payload length)`, `len` the same sum — rather than adding a
per-frame index to the WAL. **The tenants whose records the frame holds.**
**And `clamp`**, below.

Settlement is a **rebuild, not a resume at an index**, because a panic
inside `ingest` leaves the tenant's tree in an unknown state: a leaf can
have been widened or created before the audit event that records it was
emitted, and resuming against that tree would match the widened leaf
cleanly and emit no event — a template version with no audit history,
which RFC 0017's versioned rendering cannot fold. So the **next ingest
turn**, before its own batch, rebuilds each of the entry's tenants: from
its last *installed* snapshot, replaying that tenant's frames from the
snapshot's mark through the end of the span; or, when the tenant has no
installed snapshot, from empty with a **full replay** of the tenant's
surviving WAL history — from its oldest surviving frame, which RFC 0052
§3.2's no-snapshot pin guarantees is still there, through the span — never
the span alone, which would rebuild a tree missing every template the
earlier frames defined. The replay is in WAL order, through the same
per-tenant restore-and-replay `recovery::recover` performs at startup,
factored to run for one tenant in-process; a snapshot that fails to
restore falls to the empty-plus-full-replay arm, as it does at startup.
`restore_tenant` refuses a tenant that already has live state
(`TenantAlreadyLive`), by design, so the rebuild goes through a new
`MinerCluster::replace_tenant(tenant_id, Option<&SnapshotState>)`: under
the miner lock it drops the tenant's live `TenantState` and installs the
restored (or empty) one as one operation, so no caller can observe the
tenant absent. The cluster-wide `template_id` allocator is not rewound —
ids are never reused — so a leaf the replay re-derives that had been
allocated after the snapshot gets a fresh id; rows already published under
the old id still render, since its audit events are durable, and the two
ids carry the same tokens: a duplicate template, which is the posture a
restart's replay already has. Per-tenant derived state — the RFC 0023
ceiling accounting, `template_count`, the per-tenant gauges — is recomputed
from the installed tree, and no other tenant's state is touched. Every
widening between the mark and the panic is re-derived *with* its event,
and every record in that range is re-emitted: the submitted prefix, a
record the salvage forwarded, and anything the tenant published since its
snapshot are duplicates at worst, never a loss — the same at-most-duplicate
posture every crash between a publish and its stamp already has, only
wider. It follows that the salvage in `ingest_mined` is kept as
belt-and-braces and its count is **never read** by this path: an earlier
draft resumed at `index + salvaged`, which was unsound twice over — against
the unknown tree, and because the count increments before the salvage's
own `emit` returns, so a panic inside that emit would report a record as
forwarded that no buffer holds. The frame's bytes are read through
`Journal::read_frames(from: WalOffset, to: FrameSpan) -> Result<Vec<Bytes>,
ReceiveError>`, added to the object-safe trait for this purpose, taking the
journal mutex only for the read and verifying every frame header and
checksum it returns; a span that does not end on a frame boundary is an
error that leaves the entry in place, since it means the entry, not the
WAL, is wrong.

**While an entry is unresolved the marks are clamped, by one rule at one
site.** The entry also records `clamp`: the value of `last_durable` at the
moment the frame was appended, which is the previous turn's offset — the
frame with the entry never advanced it, since the panic unwinds before
the write that would. From then until the entry settles, every write to
`last_durable` is `min(offset, clamp)` (with `None` staying `None`), so a
later successful append cannot carry the mark past the frame; the latest
successful offset is kept separately and becomes the mark again the
moment the entry settles. Every barrier, rotation, shutdown and
`flush_then_snapshot` mark reads `last_durable`, so they inherit the clamp
without a rule of their own. The clamp is necessary and not sufficient:
the snapshot serialises the *live* miner, so a barrier taken while an
entry is unresolved would capture the unknown tree under the clamped mark,
and a restart would restore the widened leaf and replay the frame as a
clean match with no event. So while an entry is unresolved **no snapshot
is installed for the entry's tenants** — the barrier skips them and
snapshots every other tenant as usual, since trees are scoped per tenant
(§3.7) and the panic touched only the frame's — and a restart in that
window restores those tenants from their previous snapshot and replays the
frame from the pre-frame state, which is the same rebuild the in-process
settlement performs. An `unmined` entry is removed only once the rebuild
has replayed through the span and its records have been submitted or
buffered — retained across a failure or a second panic — so an entry that
never settles holds the checkpoint, and those tenants' snapshots, visibly
rather than silently. RFC0053.2 covers a swallowed submit followed by a
successful append, a mining panic followed by a successful append, a clean
barrier taken while an entry is unresolved, a panic after `ingest_mined`
returned, and a panic between a widening and its audit event.

**What is requeued depends on where the panic lands.** The audit events are
requeued only while the audit write has not completed. Once `write_owned` has
returned `true` every event is *settled* — durable in the audit store, or
dropped by the sink's own permanent-failure policy, which is the fate a
non-panicking publish gives it too; `true` means nothing is retained for
retry, not that everything was written, and both non-retained outcomes are
final. A later panic inside the record publish therefore requeues the records
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
and RFC 0052 §3.1 adds the `cadence_failed` latch that keeps its timer from
stamping past a dropped batch. Both are retired here, and the transition
is explicit: with requeue-on-unwind a recovered panic sets nothing, so
`cadence_failed` ceases to exist as a barrier guard — RFC 0052's contract is
amended by this RFC to remove it: §3.1's latch and its `BatchGuard`
unwind site, the pre-cut and pre-stamp checks in `run_cut`, RFC0052.1's
latch clause, §3.5's receiver-exported item and RFC0052.7's assertion of
it all go together, so the two RFCs can be implemented together, and a latch set by a
pre-RFC 0053 process clears on the restart that deploys this — while the
`cadence_panic` counter #795 added stays,
now meaning "a step panicked and was retried" rather than "the cadence is
dead". Only a *panicking* `JoinError` continues the sweep; a cancelled one is
the runtime going away and still terminates it, exactly as today, so an
implementation that merely deleted the `break` — and let shutdown spin — would
not satisfy this section.

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
RFC to exclude the refusal latch explicitly — its "latch" was
`cadence_failed`, which this RFC retires — so there is one emitter per
transition and never two events for one. RFC0053.5 counts every transition.

The contract is enumerated here, as RFC 0009 §3.6 does, rather than
deferred to the registry bump; the names are proposals for the shared
`ourios-semconv` registry (one bump with RFC 0052's, through that
repository's review) and nothing is hand-written in the code:

| Signal | Instrument | Unit | Attributes |
|---|---|---|---|
| `ourios.wal.backpressure.refusing` | gauge (int) | `1` | — (`1` while the refusal latch is set) |
| `ourios.wal.backpressure.limit` | gauge | `By` | — |
| `ourios.wal.backpressure.last_refusal` | gauge | `By` | `ourios.wal.measurement` ∈ {`pre_reservation`, `projected`} |
| `ourios.wal.capacity_remaining` | gauge | `By` | — (saturating at zero) |
| `ourios.wal.sealed_segments` | gauge (int) | `{segment}` | — (sealed segments still on disk; the cap is `max_sealed_segments`) |
| `ourios.ingest.encode_fallback` | counter | `{batch}` | `error.type` ∈ {`encode_pool_disconnected`} |
| `ourios.wal.backpressure.entered` / `.left` | log events | — | `ourios.wal.limit` (By), `ourios.wal.unreclaimed` (By), `ourios.wal.measurement` |

`error.type` continues to carry the failure class on existing counters
rather than spawning per-error metrics; `encode_worker_panic` joins
`cadence_panic` as a value on the flush-error counter.

## 4. Alternatives considered

**Drop on unwind rather than requeue (§3.2).** Rejected: it prefers silent
loss of acknowledged data over a duplicate this RFC bounds to one object per
panic and counts. That is the wrong way round under `CLAUDE.md` §3.4.

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
> - **When** ingest continues until a reservation would exceed the bound
> - **Then** earlier batches were accepted and acked, and the rejecting batch
>   is refused with a reason naming the limit and both measurements (the
>   pre-reservation total and the projected total), on both transports, and
>   a `Retry-After` computed from the reclaim schedule §3.1 states —
>   asserted with the barrier and housekeeping cadences set apart: the
>   housekeeping cadence when eligible segments already exist, and the time
>   to the next barrier plus one housekeeping cadence when none do — **not**
>   a value derived from the limit, which yields no delta-seconds
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
> - **And** the rejection does **not** set the rotation-failure state
> - **And** when the store returns and no tenant pins the floor (RFC 0052
>   §3.2), ingest resumes **with no append and no restart** — the
>   timer-driven sequence reclaims, which is the only path that can clear a
>   state that rejects every append
> - **And** when a tenant pins the floor (no valid snapshot), or a valid
>   horizon lags so that the bytes cannot be reclaimed, the bound stays
>   crossed for a batch of that size after the store returns, by design; the
>   floor is reported `Pinned` in the first case and `Min` with its lag in
>   the second, rather than as an unexplained refusal, and the latch itself
>   still leaves on a smaller successful append as §3.1 defines
> - **And** when the whole backlog sits in the current append segment, the
>   timer's forced rotation lets the next pass reclaim it, so the state clears
>   without an append ever arriving
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
> - **And** under repeated rollback failures the frame-byte accounting stays
>   within the limit — the torn bytes count inside it — and the fixed
>   overhead outside it (segment headers, sidecars, seals) is capped
>   separately: the seal that would exceed `max_sealed_segments` puts the
>   WAL in the terminal rotation state, which the next append reports

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
>   off-lock publisher of a partition that worker detached settles it per
>   partition, and the worker's own guard never touches it
> - **And** a submit after that panic is either published by a respawned
>   worker or placed in the record sink's buffer and acknowledged; it is
>   never dropped behind a success, and a later successful append never
>   carries the mark past it
> - **And** the partially mined batch a panicking `mine_batch_ordered`
>   submits from its unwind arm reaches the sink before the panic resumes,
>   on the pool branch and the no-pool branch alike
> - **And** a barrier taken while an `unmined` entry is unresolved, with a
>   healthy store, stamps no mark past the entry's clamp and installs no
>   snapshot for the entry's tenants while installing the others'; a restart
>   from that state restores those tenants from their previous snapshot and
>   replays the frame; once the rebuild settles the next barrier stamps the
>   latest successful offset and snapshots them again
> - **And** a panic raised inside `ingest` after a leaf was widened and
>   before its audit event was emitted is settled by a rebuild from the
>   tenant's last installed snapshot: the widening is re-derived with its
>   event, the version's audit history is complete, and the records between
>   the mark and the frame are present at most twice, never absent; the
>   same panic on a tenant with **no** installed snapshot is settled by a
>   full replay from its oldest surviving frame, and a template defined
>   before the span is present in the rebuilt tree; `replace_tenant` leaves
>   every other tenant's tree and the allocator's next id unchanged
> - **And** the same holds for a panic raised after `ingest_mined` returned
>   — inside the no-pool branch's inline `emit`, or the pool branch's
>   `submit` — and for a panic inside the salvage's own `emit`: the record
>   is present at most twice, never absent, and the salvage count is not
>   consulted; `read_frames` on a span that does not end on a frame
>   boundary is an error that leaves the entry in place
> - **And** this holds for a panic raised **at each** point the publish can
>   reach it — before the audit write, inside it, and inside the record
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
> - **And** a restart whose remeasure fails does not come up: no coordinator,
>   no listener, and no batch admitted
> - **And** a kill after a seal's parent fsync and before its rotation
>   leaves the sealed tail in the newest segment: recovery heals it to the
>   seal's length, consumes the seal, and the post-recovery step — after
>   the remeasure, before the coordinator exists — performs the owed
>   rotation before the first append; a newest segment whose seal does not
>   verify halts; a `.wal.seal.partial` is swept and never consulted
> - **And** a kill after the heal's truncate and before the seal is consumed
>   leaves a verifying seal whose length equals the segment's: the next
>   start consumes it and continues, and the frames before it are delivered
> - **And** a sealed segment is unlinked with its seal, segment first, both
>   counted against the pass cap; a kill between the two leaves an orphan
>   seal that the next pass sweeps, and an orphan seal and a
>   `.wal.seal.partial` are swept on a pass with no checkpoint yet

> **Scenario RFC0053.5 — The backpressure state is observable**
> - **Given** a node that enters and then leaves the refusing state
> - **When** metrics are collected and logs are read across both transitions
> - **Then** the refusal-latch gauge, the limit and measurement at the last
>   refusal, and `capacity_remaining` — equal to `max(limit − unreclaimed,
>   0)` over the live unreclaimed figure at the moment of the read, not a
>   stale value — are present in the exported stream under registry names
> - **And** a forced disconnected-pool fallback increments
>   `ourios.ingest.encode_fallback` by exactly one datapoint
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
  livelock case. Those are the regression tests for #791's second half. A
  fourth leg is concurrent rather than sequential: a `proptest` driving many
  tasks that submit batches of arbitrary sizes at once against a small limit,
  asserting after every admission that the live unreclaimed figure —
  admitted minus reclaimed, since housekeeping legitimately lets a cumulative
  sum grow — never exceeds the limit — the only test a reservation taken outside the journal
  mutex fails, since the sequential flow passes it. A fifth leg fills the
  limit and submits an oversize payload, asserting `TooLarge` with no
  `reclaim_state()` read and no append — the reversed-check regression. The
  sequence is driven through **both** adapters: on HTTP the protobuf
  `Status`, `application/x-protobuf` and the exact `Retry-After`; on gRPC
  `UNAVAILABLE` with the `RetryInfo` detail carrying the same seconds. A test
  through one adapter leaves the other's contract unverified. A sixth leg
  pins a tenant's floor (a lagging or invalid horizon) and asserts that
  refusal persists after the store returns and the state is reported
  `Pinned`, which is the behaviour RFC0053.1 makes normative.
- **Configuration (RFC0053.1's precondition)** — resolver tests for an
  explicit value and an `${env:VAR}`-substituted one, plus a Helm render leg
  that the chart value reaches the config file; a missing field under
  `deny_unknown_fields` would otherwise leave the limit at its default while
  every scenario passed. The derived default at a 2 GiB segment and the
  invalid below-segment value are `WalConfig` validation tests, since the
  resolver has no segment-size input (`wal_config()` hardcodes it).
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
- **No loss (RFC0053.4)** — extends RFC 0052's `SIGKILL` crash-recovery
  extension rather than adding a parallel one, in **both** restart shapes —
  a clean replay and a torn newest tail, since the existing crash test has
  no torn tail and a remeasure hook placed only on the heal path would pass
  it — plus a third, the sealed closed segment: a fixture with a torn tail
  in a non-newest segment and a matching `.wal.seal`, asserting the frames
  before the seal are delivered, the remeasure excludes the torn bytes, and
  the same fixture *without* the seal halts as corruption — and a fourth,
  the sealed *newest* segment (the seal durable, the rotation not yet
  performed), asserting the heal to the seal's length, the consumed seal,
  the rotation the post-recovery step performs before the first append,
  the already-healed shape (seal length equal to the segment's) on a second
  start, and that a non-verifying seal on the newest segment halts — with a small
  backpressure
  bound configured so the kill lands in the refusing regime, and a
  post-restart append that asserts the rebuilt figure still refuses, and
  an exact-figure assertion against a fixture with several closed segments,
  a header-only current segment and a torn newest tail — a refused-stays-
  refused check alone would pass a remeasure that counted headers or torn
  bytes; a fault-injected leg makes the remeasure's listing fail and asserts
  startup refuses to construct the coordinator.
- **Telemetry (RFC0053.5)** — the in-memory metric exporter pattern used for
  the ingest instruments, driving one enter and one leave and asserting the
  latch gauge, the last-refusal figures, `capacity_remaining` against the
  live headroom, and exactly one event per transition — including a refusal
  and a smaller successful append inside one tick; plus the `weaver registry
  live-check` pass over those events, and an assertion that every injected
  cadence panic in RFC0053.2/.3 increments `cadence_panic` while every
  injected worker panic increments `error.type=encode_worker_panic` on the
  same counter — separately, since a respawned worker's panic is not a dead
  cadence and must not read as one.

Maturity, per `docs/rfcs/README.md`: `green` is RFC0053.1–.5 all passing in
CI with the unit, property and corpus suites green, and this RFC touches no thesis gate in `docs/benchmarks.md` §7. It does
**not** proceed to `validated` on those alone: §4 records that the sinks can
exhaust memory before the WAL bound fires, so a green run could mark a bound
validated that is never the operative limit. `validated` therefore also
requires the §7 sink decision to have landed and RFC0053.1's
unreachable-store leg, run under default configuration for long enough to
show the WAL bound refusing **before** either sink exceeds its ceiling. Until
that decision lands the RFC stops at `green`, and says so.

## 7. Open questions

- [ ] Whether the byte bound's default should scale with the volume rather
      than be the fixed 1 GiB §3.1 sets. The incident node held 42 MB over
      five days, so a default tuned for it would be far too small for a busy
      node; a fraction of the volume may be the honest default.
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
- [ ] Whether the publish should be made idempotent — a drain-time object key
      that a requeued batch reuses — so the §3.2 duplicate becomes a
      byte-identical overwrite instead of a second object. The cost is that a
      requeued batch would have to stay a unit through the next drain rather
      than merge with what arrived since.

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
  unacked batch; §3.1 takes `UNAVAILABLE` over its `RESOURCE_EXHAUSTED`
  option.
- RFC 0014 — the record sink and its flush triggers.
- `CLAUDE.md` §3.4 (WAL-before-ack), §6.3 (observability of ourselves).
- `docs/hazards.md` #3 (WAL durability versus latency), #4 (small files).
