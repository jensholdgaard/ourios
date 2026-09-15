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
incident's shape with the permanent quiesce removed: the node degrades into a
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
because the `max_tenants` guard is **capacity**: no pass clears it, so it
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
§3.2's `SettlementInProgress` — both self-clearing — and absent for
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
this RFC adds `ReclaimState::reclaimable_now: bool` — true when the head of
§3.2's ordered structure satisfies the lazily evaluated predicate (empty
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
segments (closed, current and sealed alike), defaulting to
`unreclaimed_bytes_limit / segment_size_bytes + 16` so a bound's worth of
full segments always fits with slack for the near-empty ones, and a
reservation whose append would rotate, while the retained count is at the
ceiling, is refused under the same backpressure class with the `Segments`
cause. Whether the append would rotate is the WAL's question, not the
coordinator's, and the WAL's predicate is not size alone: `Wal::rotation_due`
also rotates a *fitting* frame once the current segment's age has passed
`segment_age_secs` (a header-only segment excepted), so a coordinator that
reserved on size would let an age rotation create a segment past the
ceiling, or block the forced rotation and keep the livelock. So `Journal`
exposes the complete predicate, `fn rotation_due(&self, framed_len: u64)
-> bool` — size or age, the same function `append` consults — and the
reservation reserves a segment slot exactly when it is true: `retained +
1 ≤ max_segments`, else refuse. **The predicate is evaluated once, and
the append acts on that answer**: asking twice is not equivalent, because
the age half is a function of wall time, so a segment can cross
`segment_age_secs` between the reservation and the write and rotate
without the slot the reservation refused — the exact case the ceiling
exists to prevent. So the decision travels as a token rather than being
recomputed: the reservation's evaluation produces `RotationDecision::{
Rotate, Reuse }` and `Journal::append_batch` takes it —
`fn append_batch(&mut self, payload: &[u8], rotation: RotationDecision)
-> Result<WalOffset, ReceiveError>`, an **amendment to RFC 0052 §3.7's
signature** — honouring it instead of consulting `rotation_due` again,
with the size half kept inside the WAL as a fail-closed assertion (a
frame that does not fit the current segment under a `Reuse` token is a
caller error, not a silent rotation, since size cannot change between the
two while the journal mutex is held). Both run under one hold of that
mutex, so no append interleaves between them either. `validate_config` rejects `max_segments < 2`:
the current segment always holds one slot and is never unlinked, so a
ceiling of one leaves no room for the segment a rotation must create —
every due rotation would be refused, the forced-rotation predicate's
"below the ceiling" leg could never hold, and the node would wedge with a
backlog it cannot close. Two is the smallest ceiling that admits one
closed segment beside the current one; the derived default is far above
it. RFC0053.1 asserts the validation. At the ceiling a due rotation is **refused,
never squeezed in**: the request path runs no housekeeping, since prepare,
file half and commit put fsyncs in front of every concurrent append; the
refusal sets the latch, the next `maintain` pass reclaims what it can, and
the client returns after the `Retry-After` §3.1 computes — so with a
reclaimable segment the request is refused once and admitted after the
pass, and with nothing reclaimable it stays refused, and no forced
rotation runs at the ceiling (below). A reservation that would not rotate
is unaffected, since it creates no header. **Every rotation takes the slot
check, not only the reserved one**: RFC 0052 has two append-independent
rotation callers — the barrier task's idle rotation (§3.2, rotate-before-
cut under the exclusion) and the post-recovery step's owed rotation — and
a rotation from either at `retained == max_segments` would create the
segment the reservation refuses. So the check lives in `Wal::rotate`
itself, under the journal mutex: at the ceiling it performs no rotation
and returns `Ok(RotationOutcome::RefusedAtSegmentCap)`. RFC 0052 §3.7
defines `Journal::rotate` as `Result<(), ReceiveError>`; this RFC amends
it to `Result<RotationOutcome, ReceiveError>` with `RotationOutcome::{
Rotated, RefusedAtSegmentCap }`, so that a refusal at the cap is a typed
outcome on the `Ok` arm rather than a rotation failure: it draws on no
retry budget. **What a caller makes of it depends on why it rotated**, and
the two classes are named here and at the seal site. A **discretionary**
rotation — append-driven, the barrier task's idle one, §3.1's forced one —
has no obligation to close the current segment, so a refusal is simply
"no rotation": the idle rotation is skipped and the tick captures its cut
as if the segment had not aged, with the mark `last_durable` as every
cut's mark is; the forced one reports it and the next pass retries; the
reservation path never reaches it, having refused first. An **owed**
rotation is one the current segment's state requires — after a seal, where
that segment must never take another frame, and the post-recovery step's
discharge of an owed rotation — and there a refusal is a durability fault,
not a shrug: the WAL enters `RotationState::Deferred { AtSegmentCap }`
(below) and admits nothing until a pass frees a slot. Header overhead is
therefore at most 24 B × `max_segments`, and the refusal leaves with the
same latch once a pass removes a segment. **A segment stays in it
until its unlink succeeds.** RFC 0052 §3.2 marks a popped ledger entry
*reclaiming* and keeps it in the byte and segment accounting until the
off-lock unlink has succeeded, so the bytes of a segment whose `RECLAIM`
write or unlink failed are still `unreclaimed` here and a refusal is never
computed against bytes that are still on disk. An **uncertain deletion**
holds the same way and for longer: when the pass's parent fsync fails
(`ReclaimOutcome::Unlinked { fsync_failed: true }`), every path it removed
may or may not survive a restart, so RFC 0052 keeps those entries
`uncertain` with their bytes counted until a later pass re-verifies them
gone. The bound therefore charges them too — conservative in exactly the
direction the bound needs, since the alternative is admitting against
bytes that may still be on disk. This is a **segment
admission bound, not a directory cap**: the `CHECKPOINT` and `RECLAIM`
sidecars, their temp files, the `.wal.seal` sidecars §3.1 introduces below
and their `.wal.seal.partial` temps, and rotation partials awaiting RFC
0052's sweep are all outside it, so `disk_bytes` can exceed the bound by
those and by the headers — each is either a small record or debris the
sweep removes: headers are capped by `max_segments`, the seal count is
capped below, and `RECLAIM` holds two kinds of entry, both bounded —
`reclaimed_through`, one offset per tenant, and `planned`, one entry per
*popped segment* carrying that segment's per-tenant last offsets, so the
record's size is O(tenants) plus O(segments popped by the outstanding
pass × tenants in them). The planned half lives only between a pass's two
halves and is capped by `max_unlinks_per_pass`; the tenant half is finite
and operator-defined under RFC 0047. Neither grows with ingest volume or
with the backlog. **It survives a
restart by being rebuilt, not persisted, in the same unit.** Once replay
and heal have settled the newest segment's tail — a torn frame there is
truncated by heal and must not be counted — the figure is initialised as
the sum over every surviving segment, closed and current, of its
**validated frame lengths**, so the rebuilt number is frame bytes like the
live one and matches what the reservation adds to it. The hook is RFC 0052
§3.7's `Wal::rebuild_ledger()` — the validated frame scan that fills the
ledger and the partial list — called after every successful replay and
after the heal when there was a torn tail to heal — not at `Wal::open`,
which runs before either and would count torn bytes, and not only on the
heal path, which a clean replay never enters — and the coordinator is
constructed after recovery, so the seed completes before any append is
admitted and a node restarted mid-outage resumes refusing at the same bound
rather than admitting from zero. The same scan seeds the current segment's
frame bytes (§3.1's rotation trigger) from the healed newest segment. And
it fails closed: a listing, read or frame-validation error from the scan
fails startup before the coordinator or any listener is constructed,
because a partial or zero seed is precisely a node that admits past its
bound.
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
  validation, then the terminal-rotation check, then the tenant check,
  then the bound** — so an oversize payload against a terminal WAL is
  `TooLarge`, a legal payload against a terminal WAL is the terminal
  classification without a `reclaim_state()` read, a legal payload for an
  unrecoverable tenant (§3.2) on a healthy WAL is that tenant's terminal
  classification, a legal payload for a *new* tenant at the `max_tenants`
  guard is a `TenantCapacity` refusal, and only a legal payload for a held,
  healthy tenant against a healthy WAL reaches the reservation. The tenant check has a source too:
  the receiver resolves the tenant out of band before `ingest` (RFC 0046
  §3.1 — `Pipeline::ingest` already takes the `TenantId`), the commit path
  carries it to the coordinator, and the coordinator consults its own
  `fn tenant_state(&self, tenant: &TenantId) -> TenantAdmission` (`Healthy`
  or `Unrecoverable`, set by §3.2's settlement), under the same mutex —
  the WAL-global `rotation_state()` cannot say it, since the journal has
  no tenant in view. The classification is the same server-terminal,
  client-retryable class, naming the tenant; other tenants are unaffected. The terminal check has a named source: `Journal`
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
the refusing state unchanged; that includes a truncate-back to the header
of a fresh segment, which leaves an empty *current* segment holding no
frame bytes and counting against `max_segments` — RFC 0052 never unlinks
the current segment, so it is reclaimable only once a later rotation (an
append's, the timer's idle one, or §3.1's forced one) closes it, and
housekeeping reclaims it on the pass after that. A failure after bytes reached disk whose
best-effort truncate-back *also* fails is not something the counter alone
can absorb: the frame's full framed length is added to the unreclaimed
figure at once — an over-count is safe, an under-count admits past the
bound — **and the ledger records that counted amount against the sealed
segment** — and, when the failure was the *first* frame of a fresh
segment, the seal's `last_good_length` is the header alone, leaving a
closed segment with **no valid frame**. RFC 0052 §3.2's predicate is
expressed over a segment's highest offset and its per-tenant last offsets,
neither of which exists there, so such a segment could never become
eligible: it would hold a `max_segments` slot for the life of the root and
strand the very owed rotation §3.1 defers on that slot. So the predicate
gains a base case, as an **amendment to RFC 0052 §3.2**: a *closed*
segment holding no valid frame is **unconditionally eligible** — no
tenant has a frame in it and no checkpoint comparison applies, because
nothing references it — and the pass pops it ahead of the horizon-driven
candidates, which costs nothing since it can never be retained by any
horizon. RFC0053.1 covers it. Where a frame does survive the seal, so when housekeeping unlinks it the accounting subtracts
exactly what was added, whatever the file held, and no artificial backlog
survives the unlink without a restart; the next `rebuild_ledger()`
corrects the figure to the bytes on disk in any case, and RFC0053.1
asserts the no-restart path — **and the segment
is sealed with a durable marker** — the WAL refuses further appends into
that segment, **fsyncs the segment's data first**, so the length the seal
names is a length that survives a crash (a seal naming bytes past the
surviving ones would verify and then halt recovery; a failed data fsync
joins the seal-write failure path below), then writes the seal to
`<uuid>.wal.seal.partial`, fsyncs it, renames it to `<uuid>.wal.seal` and
fsyncs the parent — the rename is what makes a seal all-or-nothing, so a
crash mid-write leaves a `.partial` that is debris, never a seal that
half-verifies — and only then rotates through
RFC 0052 §3.3's retry path into a fresh segment, since appending after a
partial frame would let recovery consume later bytes as part of it. The seal is **versioned and
segment-bound**: it carries a format version, the segment's own UUID, the
last good length, and a checksum of those three, so a stale or corrupted
seal cannot authorise anything. The rotation that follows the seal takes the slot check like any other,
but it is an **owed** rotation in §3.1's sense — the sealed segment must
never take another frame — so the refusal is a fault rather than a shrug:
with a slot it rotates; **without one the seal is written and the
rotation is deferred**. That state is *not* terminal, and calling it so
would break RFC 0052 §3.3's contract in both directions: terminal means a
node an operator must clear and carries no retry hint, while this one
clears itself the moment housekeeping frees a slot. So `RotationState`
gains a third variant beside `Healthy` and `Retrying`, **`Deferred {
AtSegmentCap }`** — an amendment to RFC 0052 §3.3 — and the admission
order reports it as **reclaimable backpressure**: `WalBackpressure` with
the `Segments` cause and the housekeeping delay §3.1 computes, not the
server-terminal class, so a client retries on the hint and no operator is
summoned. `maintain` retries the deferred rotation after a pass frees a
slot, on which the state returns to `Healthy` and admission resumes;
`Terminal` keeps its own meaning, the exhausted retry budget, with no hint
and a restart to clear it. RFC0053.1 covers both cases. If the seal write itself fails the WAL
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
**post-recovery step** — the same step that calls `rebuild_ledger()`,
after replay and heal and before the coordinator is constructed — not by
`Wal::open`, which `serve` runs *before* `recovery::recover` and which
therefore has neither the replay-validated boundaries nor the rebuilt
figure the rule depends on; so a restart never appends into a sealed
segment either.

**Seal removal is an amendment to RFC 0052 §3.7, stated here because that
section's sweep knows only `.wal.partial` and segments.** Two boundaries
with RFC 0052's `RECLAIM` record are stated first. A seal is not a
`planned` entry — `planned` is one entry per popped *segment* with its
per-tenant last offsets, and a sealed segment's entry carries nothing
about its seal — so a crash between the record write and the seal's
unlink leaves an orphan seal that `rebuild_ledger()`'s debris seed finds,
not open's reconciliation of `planned` against the directory; and the
heal touches only the sealed segment and its seal, never `CHECKPOINT`: a
`RECLAIM` with entries beside a missing `CHECKPOINT` is fail-closed at
open per RFC 0052 §3.2, and the heal runs after open, so it can neither
remove nor bypass that witness. A seal belongs
to its segment: the per-segment ledger entry (RFC 0052 §3.2) records that
the segment is sealed, live sealing sets it, and `rebuild_ledger()`
seeds it at recovery from the seals it finds — a seal whose segment is
gone is an orphan and is pushed onto the same seeded debris list as
partials, together with every `.wal.seal.partial`. When housekeeping
unlinks a sealed segment it does so in the pass's off-lock half in an
order no crash can invert: **unlink the segment, fsync the parent, then
unlink the seal, fsync the parent** — two fsyncs, because one fsync after
both unlinks would let a crash keep the seal's unlink and lose the
segment's, resurrecting a torn segment without its seal, an unmarked closed
tail that §3.1 says halts recovery. With the segment's unlink durable
first, the only state a crash can leave is an orphan seal, which the next
pass sweeps. Each unlink counts against the per-pass cap; orphan seals and
`.partial` seals sit on the same seeded debris list as partials and are
popped **first** within the cap, as RFC 0052 §3.7 pops partials, on every
pass regardless of the checkpoint precondition, so a restart with no
checkpoint yet still clears them.
RFC0053.4 covers the crash window, the orphan and the restart-before-
checkpoint case.

**Repeated rollback failures are bounded, in two units.** The torn bytes
are *inside* the bound for as long as they exist: the failed
frame's full framed length is added at the seal, and at a restart the heal
truncates them before `rebuild_ledger()` runs, so the rebuilt figure
excludes what is no longer on disk and no sequence of seals can grow the
frame bytes past the limit.
What sits *outside* it is a fixed-size seal per
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
and the Helm chart exposes it under the receiver's config block; RFC 0052
§3.7's `WalConfig::max_unlinks_per_pass` (proposed default 128, validated
at open to be at least `rotation_retry_attempts`) is exposed beside it as
`wal_max_unlinks_per_pass`, the same way, since the two together are what
an operator tunes when a backlog must clear under a bound. A
second knob rides with it, for a different growth: in open mode (RFC 0026
§3.1, no `auth` configured) tenant ids are client-chosen, so the per-tenant
`RECLAIM` entries and the `SnapshotLedger`'s retained states (§3.2) would
grow with traffic rather than with an operator's tenant set. `ReceiverSection`
gains **`max_tenants`** (default 1024, same `${env:VAR}` and Helm path):
admission of a tenant id the miner does not yet hold, when the guard is
reached, is refused as `TenantCapacity { count, limit }` naming the tenant
— ordered with §3.1's tenant check, after the terminal-rotation check and
before the bound — and existing tenants are unaffected. The refusal is
**capacity, not reclaimable backpressure**, and is its own error rather
than a `BackpressureCause`: a `Held` slot is never released by
reclamation, so no pass can clear the state and a `Retry-After` would
advertise a wait that ends nowhere. So `TenantCapacity` carries no
`Retry-After` and no `RetryInfo` — the client backs off exponentially, as
OTLP prescribes when none is sent — it does not enter the refusal latch,
it is counted on the ingest counter with `error.type = wal_tenant_cap`,
and `ourios.wal.tenants.usage` with no `free` left is the alert. The operator path is
to raise `max_tenants`; a verb that releases a tenant is deferred to §7,
with the note that under RFC 0047/0048 tenants are an out-of-band
decision and the guard is not the place to make it. **What the guard is
and is not.** In open mode a client can fill it with chosen ids and deny
a later, legitimate tenant its first write. That is not a hole the guard
introduces and not one it can close: open mode has no tenant isolation at
all — any client that can reach a listener may write to, and read from,
any tenant, which is exactly what `warn_if_open_mode` says at startup
(RFC 0026 §3.1) — so cross-tenant availability is not a property open
mode offers, and the cap exists to bound the node's disk and memory, not
to arbitrate fairness. **Open mode is not a production posture**; the
warning exists so that running without `auth` is a visible choice. In an
authenticated deployment RFC 0047 §3.1 binds tenant ids to principals and
RFC 0048 authorises them, so a client can exhaust only the tenants it is
entitled to, and the guard is the backstop it was meant to be. No
eviction, rate limit or per-principal quota is added here; the admin
release verb is the §7 follow-up. **Idle
tenants are not evicted in this stage**: a `Held` slot lasts for the
process lifetime, and a restart rebuilds the table from what recovery
restored. A check before the append is not enough on its own, since
concurrent first writes for different new ids would all observe the count
below the guard; so the coordinator keeps a tenant admission table under
the same mutex the reservation takes, with two states: a new id takes a
**`Reserved`** slot there before its append — refused when `Reserved +
Held` is at the guard — and the slot is **reference-counted per in-flight
first write**, so concurrent first writes for one id share it; it moves to
**`Held`** when any of them has its sync return `Ok` and the tenant is
installed in the miner, and it is released only when the *last* in-flight
first write for that id has settled without a success — append failure,
sync failure or an unwind before installation — **and that id has no
unresolved `unmined` entry**. The exception matters because settlement
installs a tenant outside any admission path: a first write that appends,
panics in mining and unwinds would otherwise release its slot, another id
could take it and become `Held`, and the rebuild would then install the
first tenant past the guard. So an unwind that leaves an `unmined` entry
**retains** the reservation, in a `Reserved { pending_settlement }` form
that the guard counts like any other; the entry's settlement converts it
to `Held` when it installs the tenant, and releases it when the entry
resolves without installing one. No reacquisition is needed, because the
slot was never given up. `ourios.wal.tenants.usage` carries `held` and
`reserved` as separate states, since admission consumes both. **Recovery seeds the table**: after restore and
replay, and before any listener is constructed, every tenant the miner
holds is entered as `Held`, so a restarted node neither admits past the
guard nor refuses a tenant it already serves; RFC0053.4 asserts it.
Authenticated deployments bound the set out of band (RFC 0047 §3.1's
binding, RFC 0048's authorisation), so the guard is the open-mode
backstop, not the tenancy model; the held and reserved counts are exported
(`ourios.wal.tenants.usage`, §3.3) and RFC0053.1 asserts the refusal.
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
pre-append check reports that terminal classification — RFC 0052 §3.3's
**server-terminal, client-retryable** class, RFC 0018 §3.2's third: the
client keeps its data and backs off exponentially, and neither
`Retry-After` nor `RetryInfo` is sent, since the server cannot predict when
an operator clears the node — before it consults the bound at all.
Backpressure never masks a state no delay can clear.

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

So the housekeeping pass, through `CommitCoordinator::maintain` (RFC 0052
§3.7: the journal guard is taken for `housekeeping_prepare`, released for
the file half, and taken again for `housekeeping_commit`; no barrier
exclusion is involved, since a rotation is a WAL operation and not a cut),
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
the trigger requires a pass that planned segments. RFC 0052 §3.2 gates
only *segment planning* on the witness, so a skipped pass still sweeps
partials: `removed_partials > 0` beside `removed_segments == 0` is exactly
the migration window and is not progress for this trigger or for §3.1's
latch, both of which read segments
(temp-file cleanup and partial failures do not count; RFC 0052's
`horizon_remaining` and `unlink_remaining` say whether the pass merely ran
out of budget, and a pass that did is not a stalled one — the trigger
requires both at zero) and `reclaimable_now` is false, `current_segment_frame_bytes` equals the whole
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
**before** it evaluates the forced-rotation predicate: `maintain`, under
the guard it already holds for `housekeeping_commit`, calls `rotate`,
whose first step is that discharge (§3.7's definition), whenever
`ReclaimState` reports the obligation outstanding — a discharge, not a new
segment, since the empty-segment no-op rule applies once the fsync is
owed no more. A failed discharge draws on the same rotation retry budget
it does on the append path and can reach the terminal state, which the
next request reports; a successful one lets the next append ack. The
predicate then runs as stated, on a WAL that owes nothing. RFC0053.1
asserts the wedge: a forced rotation whose parent fsync fails is
discharged by a later pass with no append arriving. The rotation itself runs
**under the guard `maintain` still holds for the commit**, so no append
interleaves between the commit's verdict and the rotation.
`maintain` keeps RFC 0052's signature, and this RFC has it stamp
`forced_rotation: Option<Result<RotationOutcome, ReceiveError>>` into the
`HousekeepingProgress` it returns — the coordinator's field, set after
`housekeeping_commit`, not a journal field — so the outcome — retrying or
terminal — surfaces through the same return the trigger read
`removed_segments` from. That last
condition needs a state surface the inherited `ReclaimState` lacks —
`unflushed_bytes` resets on every sync, so it cannot tell a synced current
segment from an empty one — and this RFC adds one field to it:
`current_segment_frame_bytes`, maintained by the same append accounting and
seeded from the healed newest segment by the post-recovery ledger rebuild (a
reopened WAL has frames in its current segment before any append). An
earlier draft rotated on the latch alone and argued a spare rotation was
harmless; it is not at the ceiling, and it is pointless when the batch
still fits or closed segments hold the backlog, so the predicate is the
only-holder case exactly. Rotation is a WAL operation rather than an
append, so backpressure does not
block it; the segment closes, the next pass can reclaim it, and the state
clears. Nothing is acked by that rotation, so the no-ack-on-refusal property
is untouched.

That needs an owner, and RFC 0052 §3.7 provides it: `Journal::rotate(&mut
self) -> Result<RotationOutcome, ReceiveError>` as §3.1 amends it, the
object-safe, append-independent
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
differently: a stale tenant horizon keeps the segments holding that
tenant's uncovered frames ineligible. RFC 0052 §3.2's unlink rule is per
segment and per tenant — there is no global `min(checkpoint, floor)`
bound; `RetainFloor` only reports the minimum — so this is not "the whole
WAL is ineligible": segments holding only *other* tenants' covered
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
horizon is `Min` with `lag_bytes` reported beside it — `Min` is the
reported minimum and carries no comparison of its own; the per-segment
rule is inclusive at the checkpoint and at a durably installed horizon
and strict at a pin — and in both the bound stays crossed for
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
publisher_panic`, and continues. RFC 0052 §3.1 already defines what a
dying thread does — it drains every batch still queued back into the
buffers as `ready` partitions, releasing each guard as its records land,
and the coordinator respawns it on the next enqueue — and this RFC changes
only the failing batch's own arm, from latched-and-settled to requeued
through its `RecoverableBatch`; and a worker
whose enqueue finds the queue **disconnected** (the publisher gone for
shutdown, or between death and respawn) follows RFC 0052 §3.1's park
rather than a settle: the failed send returns the item to the worker,
which puts the batch back into the buffers as a **`ready` partition**
under the sink lock *before* releasing its guard — **recording the current
`barrier_epoch` on the parked partition exactly as RFC 0052 §3.1 has a
requeue record it**, so a cut captured before the park fails its
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
which RFC 0017's versioned rendering cannot fold. So settlement rebuilds each of the entry's tenants: from
its last *installed* snapshot, replaying that tenant's frames from the
snapshot's mark through the end of the span; or, when the tenant has no
installed snapshot, from empty with a **full replay** of the tenant's
surviving WAL history. **The installed snapshot is never re-read from the
directory.** A visible `.snap` is not an authority in-process: its rename
can precede the parent-directory fsync, and after a failed write it can
be newer than the durable horizon RFC 0052 §3.2's `SnapshotLedger` holds.
So the ledger, which already records per tenant the horizon of the last
*durably installed* snapshot — renamed and parent-fsynced, recorded at
the ledger's horizon update — retains beside that horizon the decoded
`SnapshotState` it was installed from, and settlement rebuilds from that
retained copy. The cost is one decoded state per tenant, at most the size
of the tenant's live tree, itself bounded by RFC 0023's per-tenant
ceiling; the copy is replaced when a newer install completes and dropped
with the tenant. A tenant restored at startup has its copy from
`load_all_durable()`, the one directory read RFC 0052 permits — from its oldest surviving frame, which RFC 0052
§3.2's no-snapshot pin guarantees is still there, through the span — never
the span alone, which would rebuild a tree missing every template the
earlier frames defined. The replay is in WAL order, through the same
per-tenant restore-and-replay `recovery::recover` performs at startup,
factored to run for one tenant in-process; a snapshot that fails to
restore falls to the empty-plus-full-replay arm, as it does at startup.
**Settlement has two triggers, so the byte bound cannot starve it, and
one claim protocol, so they cannot both settle.** The
next ingest turn settles before its own batch, under the ingest gate, so
later turns do not overtake the gap — but a frame that fills the bound is
followed by refusals, not turns, and an entry that waited for an admitted
turn would never settle while the bytes it holds are never reclaimed. So
the barrier task settles every unresolved entry at the start of each tick,
before it captures its cut. Settlement never touches the ingest gate: RFC
0052 §3.1 is explicit that `ingest_gate` is a watch counter of append
sequences, that a timer has none to reserve, and that inventing one would
interleave with real turns — so both triggers take only the **barrier
exclusion, then the miner lock**, in that fixed order; an admitted turn
already holds both when it finds an entry for its own tenant, and the
tick takes them as it does for a cut. The claim makes the serialisation
explicit and lets the later one observe it: an `unmined` entry has a
state, `Unresolved` or `Settling`, and the settler moves it to `Settling`
under the miner lock before it touches the tenant. The claim alone does
not serialise the loser with the rebuild, because the winner releases the
locks for the bounded reads (below) and takes them again only for the
final `replace_tenant`; so the entry also carries a completion signal, and
the protocol is: the winner holds the exclusion and the miner lock for the
claim only, performs the reads with no pipeline lock held, re-takes the
two in order for `replace_tenant`, removes the entry under the miner lock
and signals. **A panic inside the settlement has its own arm**, because
the replay runs the same mining path that panicked in the first place and
`replace_tenant` runs under the miner lock: without one, the barrier
tick's `catch_unwind` would resume the task with the entry still
`Settling`, its waiters parked and the tenant refused for the life of the
process. So the settlement runs under its own `catch_unwind` inside the
tick's: an unwind returns the entry to `Unresolved` — or marks the tenant
unrecoverable, when the rebuild had already reached §3.2's
`reclaimed_through` rule — **and signals the waiters** before the panic is
counted and the tick resumes, so the next trigger claims the entry and a
waiting request is refused with a fresh hint rather than held. The miner
lock's poisoning is recovered the way the pipeline already recovers it
(`lock_miner` takes `PoisonError::into_inner`), and the tenant's tree is
whatever the failed rebuild left, which the next settlement replaces
wholesale. RFC0053.2 drives a panic in the replay and in
`replace_tenant`. A turn for a settling tenant must not wait *inside* the gate,
where it would hold every other tenant's sequence behind it; instead the
coordinator keeps the **settling set** under its admission mutex, and a
request for a `Settling` tenant is **refused at admission, before it takes
a sequence**. The check and the append are **one critical section**,
not two: the admission mutex covers reading the settling set, the tenant
slots and the byte and segment reservations *and* is held across the
`append_batch` that follows, so no request can pass the check and then
append behind a settler that has already claimed the entry, and no settler
can insert between a check and its append. **The commit sequence is not
reserved there**, and that is deliberate: `CommitCoordinator::append` today
takes the journal lock, appends, and only *then* allocates `seq` from
`FlushState`, returning `CommitOutcome { seq: None }` when the append
failed — no sequence is consumed, so the gate never waits on one that was
never used and `FlushState` has no gap to tolerate. Reserving a sequence
before the append would introduce exactly that hole, needing a no-op
completion on every failure path to keep the gate advancing. So sequence
allocation stays where it is, after a successful append, inside the same
admission hold; the gate itself is untouched, awaited outside the mutex as
today.

**One lock order covers all four locks**, since this RFC adds the first of
them to the three RFC 0052 §3.1 fixes. Top to bottom: **admission mutex →
barrier exclusion → miner lock → `last_durable`**, with the **journal
mutex** taken below the admission mutex and never above it — an ingest
turn takes the admission mutex for its checks and reservations (max-frame,
terminal state, tenant, settling set, byte and segment reservation), takes
the journal mutex under it for `append_batch` — whose success allocates
the sequence — releases both, then follows
RFC 0052's order for the mining span; `maintain` takes the admission mutex
and then the journal mutex for its **ledger** halves, in that order, which
is why the forced rotation and the leave transition can be evaluated
together — and **releases both across the file half**, which is the point
of RFC 0052 §3.7's prepare/commit split: the `RECLAIM` write, its fsync
and rename, the unlinks and the parent fsync all run with no admission
mutex and no journal mutex held, so a reservation never queues behind an
fsync. Holding the admission mutex there would put every ingest request
behind the pass's disk I/O and reintroduce exactly the stall the split
exists to remove. The rule, stated as part of the order: **no lock in this
hierarchy is held across store or directory I/O** — not the admission
mutex over the file half, not the barrier exclusion over a cut's flush
(§3.2), not the miner lock over a snapshot write; and a
settler **publishes its claim under the admission mutex — adding the
tenant to the settling set — before it takes the barrier exclusion**, so
the claim is visible to admission from the instant it exists and no
request can be admitted for a tenant whose rebuild has begun. The miner
lock still carries the `Unresolved`/`Settling` transition on the entry
itself, which is what makes exactly one trigger the settler; the
admission-mutex insertion is what makes that decision visible to requests.
No path takes the admission mutex while holding any of the other three,
which is what makes the order a total one. The
refusal is its own error, `ReceiveError::SettlementInProgress { tenant }`,
mapped by `IngestFailure::classify` to a `Settling` outcome: `503` /
`UNAVAILABLE` with the protobuf `Status` body naming the tenant, and —
unlike the tenant cap — **with** `Retry-After` on HTTP and a `RetryInfo`
detail on gRPC, because a settlement is short and self-clearing, carrying
the settlement's expected remainder, the barrier cadence at most and one
second at least, as a non-binding hint; it is counted on the ingest
counter with `error.type = wal_settling`; the tenant's live tree is
unreachable to ingest until the rebuild has installed it, turns for other
tenants take their sequences as usual, and no sequence is ever taken on
the tenant's behalf. The losing trigger waits on the signal
and only then proceeds to its own work, so it never runs against a
half-rebuilt tenant. A failed rebuild returns the entry to `Unresolved`,
signals, and the next trigger claims it. RFC0053.1 asserts the bound-crossed
case settles on the timer with no append and no restart, and RFC0053.2
races the two triggers on one entry and asserts exactly one rebuild. **The fallback reuses RFC 0052's reclaimed-through
check, so an unrecoverable tenant stays blocked in-process as it would at
startup.** Before rebuilding, settlement compares the tenant's restorable
horizon — its installed snapshot's mark, or none — with
`ReclaimState::reclaimed_through` — the map RFC 0052 §3.2 exposes after
open has reconciled `planned` against the directory, the only proof of
loss — **per entry, by the retention mode that entry was written under**,
since RFC 0052 §3.2 records it: an entry written under
`SnapshotHorizons::Known` was unlinked because every tenant's snapshot
covered it, so a tenant whose horizon is below it — including one with an
entry and no restorable snapshot — has state the surviving frames cannot
rebuild, and the full replay would silently produce a tree missing what
was reclaimed; an entry written under `NoConsumer` is checkpoint-covered,
which is sound *because* RFC 0052 §3.2 makes that mode a **precondition**
— it means the caller held no miner state at all — so no snapshot was ever
expected and that tenant rebuilds from empty and pins at its oldest
surviving frame, exactly as recovery treats it at startup. This RFC adds
no path that passes `NoConsumer`: the receiver always holds a
`MinerCluster`, so it always passes `Known` and expresses an unsnapshotted
tenant as the `Pinned` floor, and the WAL refuses a `NoConsumer` pass on a
root holding any `Known` entry. A `NoConsumer` entry can therefore only
predate a miner on that root, which is why reading the mode classifies
nothing a running node did as loss.
That tenant is marked **unrecoverable**, and isolated rather than left to
pin the node: its appends are refused under the server-terminal,
client-retryable class naming the tenant; its entry leaves the clamp set
for a per-tenant `Refused` state, so `last_durable` and the node-wide
checkpoint advance for everyone else; its frames are retained instead by
RFC 0052's own pin — the receiver drops the tenant from the
`SnapshotHorizons` it hands to `maintain`, so the WAL pins the tenant at
its oldest surviving frame (`RetainFloor::Pinned`, strict) and reclaims
only segments holding no frame of it; no snapshot is installed for it;
the state is exported and alerted (`ourios.wal.tenant_unrecoverable`,
§3.3, with the pin visible in the floor); and the operator path is a
restart, which halts naming the tenant per RFC 0052 and is where the
tenant's state is decided. Every other tenant continues, checkpoints and
reclaims.
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
widening between the mark and the panic is re-derived *with* its event —
but not every record is re-emitted, because the rebuild separates
**state-only replay from publication**. The coordinator holds a per-tenant
**publication watermark**, `published_through`. It is **frame-granular
and advances only on a fully settled frame**: each mined record carries
its frame's offset *and its index within the frame* from the turn that
mined it; a publish that settles durable marks those records settled in
the coordinator's per-frame ledger (under the sink lock), and the
watermark advances to a frame only when every record of that frame and of
every frame below it is settled — a frame holds several records, the
no-pool path publishes them one at a time, and a frame with some records
durable and a later one lost must not be counted as published. Progress
inside a partially published frame is kept in the `unmined` entry instead:
at settlement the entry records `emit_from`, the index of the first record
of its frame not yet settled, read from the same ledger. Replay into the
miner mutates the tree for every frame in the range but suppresses
emission for frames at or below the watermark — their records are all
durable — emits every record of the frames above it, the **ambiguous
span**, whose records were buffered, in flight, or never mined, and within
the entry's own frame emits only records at or after `emit_from`. So the
submitted prefix and a record the salvage forwarded are re-emitted only if
they lie in the ambiguous span, and what the tenant published since its
snapshot is not re-emitted at all. The at-most-twice claim is therefore
narrowed: a record is present at most twice, never absent; a record at or
below the watermark, or below `emit_from` in its frame, is present exactly
once; and only a record in the ambiguous span — a frame above the
watermark, or a record at or after `emit_from` — can be present twice,
which is the same at-most-duplicate posture every crash between a publish
and its stamp already has.

**The watermark is persisted, or a crash would republish what a panic
already retried.** With the watermark in memory only, a partition object
`O1` accepted, a panic, a retry `O2`, and a crash before the next
checkpoint replays from `X` and yields `O3` — three objects. So the
watermark is written durably on the checkpoint's own path, as an
**amendment to RFC 0052's sidecar layout**: `Journal::checkpoint` gains
the per-tenant map, `fn checkpoint(&mut self, durable_to: WalOffset,
published: &HashMap<TenantId, PublishedMarks>)` where `PublishedMarks {
records: WalOffset, audit: WalOffset }` names the pair the rules need, and the WAL writes it to a
`PUBLISHED` sidecar beside `CHECKPOINT` — a versioned, checksummed record
of `(tenant, PublishedMarks)` entries, written to `PUBLISHED.tmp`, fsynced, renamed,
parent fsynced, **before** `CHECKPOINT`'s own write in the same call, so a
`CHECKPOINT` never exists without a `PUBLISHED` at least as new; `*.tmp`
stays the sidecar namespace RFC 0052 §3.7 reserves, and the file is
outside the byte bound like the other sidecars. **Audit has its own watermark in the same record**, because the record one
does not cover it: an audit group can be durable in the audit store while
the record publish of the same batch panics, and a crash before the next
checkpoint would replay the frame and regenerate an event the store
already holds. So the audit sink's settlement raises a per-tenant `audit`
offset the way the record sink raises `records` — highest fully settled
frame, monotonic, under the sink lock, and advanced only from RFC 0052
§3.1's `audit_durable_through`, the position a store write has actually
returned success for, never from an emptied buffer, since a concurrent
drain can hold exactly those events in an unfinished write — both are
written in the one `PUBLISHED` write, and **audit replay is gated on
`max(X, audit watermark)`** while record replay is gated on the record
watermark. The §3.2 claim that a settled audit group is never requeued
therefore holds across a restart too, not only in-process. Recovery seeds both
in-memory watermarks per tenant as the greater of the `PUBLISHED` entry
and what the Parquet-side suppression horizon `X` implies, and a missing
`PUBLISHED` beside a version-2 `CHECKPOINT` is fail-closed like a missing
`RECLAIM` — RFC 0052 §3.2 names `PUBLISHED` in that matrix on the same
footing — with **one stated exception, which is this RFC's own migration
case**: a node that ran RFC 0052 alone has a version-2 `CHECKPOINT` and a
`RECLAIM` carrying `checkpoint_seen`, and no `PUBLISHED`, because nothing
wrote one. Failing closed there would refuse to start every node that
upgrades. So `PUBLISHED` absent beside a version-2 `CHECKPOINT` **and** a
`RECLAIM` whose `checkpoint_seen` is set is **accepted**: the watermarks
seed from `X`, the checkpoint horizon, which is what RFC 0052 already
guarantees replay suppresses below, and the sidecar is created by the next
checkpoint. "Once" needs a witness of its own, though, because the
`CHECKPOINT`/`RECLAIM` pair cannot supply it: a crash before that next
checkpoint leaves exactly the state the accepting start read, so every
restart would take the same coarse-`X` branch and repeat its window. So
the record carries one more flag, on the same two-state pattern its
checkpoint witness already uses and as an **amendment to RFC 0052 §3.2's
record header**: **`published_seeded`**, written durably on the accepting
start *before* any batch is admitted, and confirmed — a second durable
write, at the next record write after the first `PUBLISHED` write
succeeded — exactly as `checkpoint_armed` and `checkpoint_seen` are. And the
window is closed by a **write**, not by the next checkpoint: waiting for
one would leave the flag armed-but-unconfirmed across every crash in
between, and an armed flag proves nothing about what reached Parquet above
`X`, so repeated crashes could each add a copy. So the accepting start
**writes `PUBLISHED` itself, carrying the seeded marks, before it admits
the first batch** — one extra sidecar write at startup, on the same
durable path a checkpoint uses — and confirms the flag at the next record
write. A crash *before* that write leaves the flag armed with `PUBLISHED`
absent and nothing published under a finer horizon, so the next start
seeds from `X` again, correctly; a crash *after* it finds a `PUBLISHED`
whose marks are the seed and proceeds from there like any other start. A
root whose flag is **confirmed** with `PUBLISHED` absent is a lost sidecar
and fails closed, exactly as a lost `RECLAIM` does. The coarse window is
therefore bounded by that single write rather than by a checkpoint that
may never come, and the consequence is stated rather than hidden: within
it the watermarks are as coarse as `X`, so records published between the
last pre-upgrade checkpoint and the seeding write are re-emitted if a
panic or crash makes the rebuild replay them — the at-most-twice bound
holds, the exactly-once refinement does not, once per upgrade. Per the pre-production layout policy there is no migration
tooling; the read path is the whole migration, and the implementing PR
carries the `!` marker. A **version-1 root** carries neither sidecar: RFC 0052 opens it
without creating a `RECLAIM`, and `PUBLISHED` follows the same rule, both
being created on the path that rewrites the sidecar at version 2 — the
first checkpoint, including the case RFC 0052 §3.2 calls out where that
checkpoint's mark *equals* the one on disk, since an idle node must still
be able to upgrade — so a restart inside that migration window stays on the
legacy branch, with no watermark to seed and nothing published under a
horizon anything reads. The converse is reachable, since `PUBLISHED` is written first,
and takes RFC 0052 §3.2's two-state witness rather than a rule of its own:
`PUBLISHED` present with no version-2 `CHECKPOINT` fails closed — naming
both files — exactly when `RECLAIM` carries `checkpoint_seen`, because
that root has checkpointed and lost it. It reads against `seen`, never
`armed`, which is what keeps RFC 0052 §3.2's two non-fault `armed` rows
non-faults here as well: `armed` without `seen` beside a **version-1**
`CHECKPOINT` is that RFC's migration retry state — nothing can have been
reclaimed, since housekeeping is a no-op until the witness exists — and
`armed` without `seen` with `CHECKPOINT` **absent** is its crash window
before the rename. A `PUBLISHED` written moments earlier in that same
first `checkpoint` call is exactly what either looks like — the root is
fresh, no rows were published under a horizon anything reads — so the
record is discarded and the next attempt rewrites it, as when neither
flag is set. Only `seen` makes its absence a fault. One
witness governs both sidecars, which is why this RFC adds no second flag
beyond the migration marker above. Beside these rows sits RFC 0052 §3.2's
own, with one scoping this RFC states because `PUBLISHED` would otherwise
inherit it too widely: **segments with neither sidecar** is a fault only
on a root that carries a **post-RFC witness** — a version-2 `CHECKPOINT`,
or a `RECLAIM` that is armed or seen — and has lost its companion, where
frames exist that no surviving witness accounts for. A root with segments
and *no* post-RFC witness at all is the ordinary live pre-RFC node —
`Wal::checkpoint` is unreachable from the pipeline today (#793), so a
running root has segments and no checkpoint — and failing it closed would
block exactly the upgrade this RFC's migration case exists to allow. It
stays on the legacy branch until its first version-2 checkpoint creates
both sidecars. `PUBLISHED` adds nothing to that judgement and inherits
whichever verdict the witness gives, because a root that cannot say what
it reclaimed certainly cannot say what it published. RFC 0052's own
criterion needs the same scoping, and this is stated as an **amendment**
to it rather than a difference between the two documents. The bound between checkpoints is then stated honestly: a panic
costs at most one duplicate per record in the ambiguous span, and a crash
before the next checkpoint costs at most **one more** for the records
published since the last durable watermark, since replay from `X` cannot
see them; the next checkpoint closes that window. It follows that the salvage in `ingest_mined` is kept as
belt-and-braces and its count is **never read** by this path: an earlier
draft resumed at `index + salvaged`, which was unsound twice over — against
the unknown tree, and because the count increments before the salvage's
own `emit` returns, so a panic inside that emit would report a record as
forwarded that no buffer holds. The frame's bytes are read through an **owned cursor, not a borrowed
iterator**: the coordinator's journal is `Mutex<Box<dyn Journal>>`, so any
`Box<dyn Iterator + '_>` borrowing the journal would hold that guard for
the whole replay — the very thing a bounded read exists to avoid. `Journal`
gains `fn read_frames_from(&self, cursor: FrameCursor, to: FrameSpan,
tenant: &TenantId, limit: usize) -> Result<(Vec<(WalOffset, Bytes)>,
Option<FrameCursor>), ReceiveError>`, where `FrameCursor { segment, byte }`
is an owned position — the start of the next frame to read — and the
returned cursor is `None` once the `to` frame has been yielded. The
coordinator takes the guard for one call, releases it, and re-takes it for
the next, so appends and rotations interleave between calls; a call
returns at most `limit` frames and never crosses a segment boundary,
whichever comes first, so the window is bounded by one segment or `limit`
frames; segments below the cursor are skipped from the ledger, never
scanned. The tenant filter is applied at the frame's `TenantOtlpBatch`
prefix, the same field the ledger reads, so other tenants' frames are
neither decoded nor returned; every returned frame's header and checksum
are verified. The range is **`(from, to]`**: the first cursor is
`FrameCursor::after(from)` for a post-frame offset — the snapshot's mark,
the horizon frame itself already folded — so the first frame returned is
the one *after* it, or `Journal::oldest_frame(tenant)` for the full-replay
arm; `to` is the panicking frame's span, returned last. A span that does
not end on a frame boundary is an error that leaves the entry in place,
since it means the entry, not the WAL, is wrong. RFC0053.2 tests both ends
of the range and the per-call bound.

**While an entry is unresolved the marks are clamped, by one rule at one
site.** The entry also records `clamp`: the value of `last_durable` at the
moment the frame was appended, which is the previous turn's offset — the
frame with the entry never advanced it, since the panic unwinds before
the write that would. From then until the entry settles, every write to
`last_durable` is `min(offset, clamp)` (with `None` staying `None`), so a
later successful append cannot carry the mark past the frame. Entries form
a set, not a slot: with several unresolved, the effective clamp is the
**minimum over every remaining entry's clamp**, settling one entry
re-evaluates that minimum over those that remain, and the latest successful
offset — kept separately throughout — becomes the mark again only when the
set is empty. Every barrier, rotation, shutdown and
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
drain and clear. The clear value stays the all-ones epoch half, the
generation half being ignored while the epoch half is clear. Overflow is
stated rather than assumed: the generation half wraps at 2^32 and wrapping
is harmless, since it is only ever compared for equality inside one
capture-to-clear window and a wrap would need four billion reports inside
it; the epoch half is the barrier's own counter, which at one cut per
`barrier_secs` cannot reach 2^32 in any deployment's lifetime, and an
implementation that needs either half wider moves to a 128-bit word or a
short mutex rather than splitting the pair again. RFC0053.3's leg drives a
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
carrying; the **epoch latch** is RFC 0052 §3.1's `failed_epoch`, set by a
panicking guard and cleared by the cut that drains its records, and it is
untouched here except for the clear §3.2 defines. So there is one emitter per
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
| `ourios.wal.segments.usage` | gauge (int) | `{segment}` | `ourios.wal.segment.state` ∈ {`retained`, `free`} (sums to the limit) |
| `ourios.wal.segments.limit` | gauge (int) | `{segment}` | — (`max_segments`) |
| `ourios.wal.tenants.usage` | gauge (int) | `{tenant}` | `ourios.wal.tenant.state` ∈ {`held`, `reserved`, `free`} (sums to the limit; admission consumes `held + reserved`) |
| `ourios.wal.tenants.limit` | gauge (int) | `{tenant}` | — (`max_tenants`) |
| `ourios.wal.sealed_segments` | gauge (int) | `{segment}` | — (sealed segments still on disk; the cap is `max_sealed_segments`) |
| `ourios.wal.tenant_unrecoverable` | gauge (int) | `{tenant}` | `ourios.tenant` (tenants an in-process settlement found below their `reclaimed_through`; alert on nonzero) |
| `ourios.ingest.encode_fallback` | counter | `{batch}` | `error.type` ∈ {`encode_pool_disconnected`} |
| `ourios.wal.settling_tenants` | gauge (int) | `{tenant}` | — (tenants with a `Settling` entry; refusals carry `error.type = wal_settling`) |
| `ourios.wal.backpressure.entered` / `.left` | log events | — | `ourios.wal.backpressure.cause`, plus the cause's measurements: `ourios.wal.limit` (By), `ourios.wal.unreclaimed` (By), `ourios.wal.measurement` for `bytes`; the count and limit for `segments` |

Those two pairs follow the OTel instrument-naming rule for a measured
amount out of a known total — `entity.usage` with a `state` attribute
whose values sum to `entity.limit`, beside `entity.limit` itself, as
`system.memory.usage` / `.limit` do — rather than carrying the limit as an
attribute of the usage gauge, which an earlier draft did and which reads
as a dimension rather than a total. The `reserved` state is why the pair
matters here: admission consumes `held + reserved`, so a dashboard reading
`held` alone would show headroom that does not exist. Names go through
`ourios-semconv` with the rest.

`error.type` continues to carry the failure class on existing counters
rather than spawning per-error metrics: the refused batch is counted on
the ingest counter with `error.type` ∈ {`wal_backpressure_bytes`,
`wal_backpressure_segments`, `wal_tenant_cap`} — the last a capacity
refusal, not backpressure, per §3.1 — `publisher_panic` joins the
flush-error counter's values, and
`encode_worker_panic` joins `cadence_panic` as a value on the flush-error
counter. The names follow the registry's rules — `ourios.*` is the system
namespace, dotted, snake_case — and go through `ourios-semconv` like the
rest.

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
a full sink do — block, spill, or drop, and under whose invariant) and that
decision is a maintainer's, not this RFC's; it belongs to a **separate RFC**,
and §7 carries the question. What this RFC states instead is the scope of
its own claim, narrowed to what it can deliver: **it bounds local disk, not
process memory.** §3.1's bound is the operative limit for the WAL directory
and for nothing else, the OOM path during a long outage remains open, and
the incident's end-to-end property — ingest that stays bounded and keeps
refusing rather than dying — needs both this RFC and that one. So the sink
decision is named here as the **blocking follow-up for the end-to-end
claim**, §6 keeps it as the gate on `validated`, and §5 asserts only the
disk bound; RFC0053.1's unreachable-store leg should still be run long
enough to show which limit is hit first, which is what makes the follow-up
concrete rather than theoretical.

**Backpressure as a rotation-failure state.** Rejected: it would reuse RFC
0052's terminal-state reporting for a condition that is not a fault and clears
on its own, telling clients a node is broken when it is merely full. The two
are kept distinct so that the remedy each advertises is the true one.

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
> - **And** the timer's idle rotation at the ceiling performs no rotation
>   and the tick still captures its cut with `last_durable` as the mark;
>   the rotation owed after a seal with a slot free proceeds, and without
>   one is deferred — `RotationState::Deferred`, reported as backpressure
>   with the housekeeping delay and never as the terminal class — until a
>   pass frees a slot, after which it runs and admission resumes without a
>   restart
> - **And** in open mode a batch for a tenant id the miner does not hold,
>   with `max_tenants` held, is refused naming the
>   tenant, as `TenantCapacity` — `503` / `UNAVAILABLE` with a protobuf
>   `Status` body and no `Retry-After` and no `RetryInfo` — while a batch
>   for a held tenant is admitted; the refusal is counted with `error.type
>   = wal_tenant_cap`, sets no refusal latch, and
>   `ourios.wal.tenants.usage` over its states sums to
>   `ourios.wal.tenants.limit` with `free` at zero; two concurrent first writes for one new id share a slot and
>   a burst of new ids never overshoots the guard
> - **And** a sealed segment's unlink subtracts exactly the amount the seal
>   added, so a node that sealed and then reclaimed shows the same
>   `unreclaimed` as one that never sealed, with no restart
> - **And** when the frame that crossed the bound is the one whose mining
>   panicked, the barrier task settles its entry on the next tick with no
>   append admitted, the checkpoint then advances past it, and the bound
>   clears by the timer alone — no restart
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
> - **And** a deferred rotation is reported as backpressure with the
>   housekeeping delay, never as the server-terminal class, and returns to
>   `Healthy` once a pass frees a slot — while an exhausted retry budget is
>   still `Terminal`, with no hint
> - **And** a node upgrading from RFC 0052 — version-2 `CHECKPOINT`,
>   `RECLAIM` with `checkpoint_seen`, no `PUBLISHED` — starts, arms
>   `published_seeded`, seeds its watermarks from `X` and writes
>   `PUBLISHED` with those marks **before admitting the first batch**,
>   confirming the flag at the next record write; a crash before that write
>   seeds from `X` again on the next start, a crash after it proceeds from
>   the written marks, and a start with the flag confirmed and the sidecar
>   missing fails closed — so repeated crashes add no further copies
> - **And** a tenant whose `reclaimed_through` entry was written under
>   `NoConsumer` is not marked unrecoverable when its snapshot is missing:
>   it rebuilds from empty and pins, while the same shape under `Known`
>   is refused; no ingest path passes `NoConsumer`, the receiver always
>   passing `Known` with unsnapshotted tenants expressed as `Pinned`
> - **And** a housekeeping pass skipped for a missing version-2 witness
>   forces no rotation: a legacy root that has not yet upgraded is left
>   alone
> - **And** `max_segments < 2` is rejected at config validation, naming the
>   reason: the current segment holds one slot and is never unlinked, so a
>   ceiling of one admits no rotation at all
> - **And** a forced rotation whose post-rename parent fsync fails does not
>   wedge the node: a later pass discharges the pending `Rotation`-origin
>   fsync before it evaluates the predicate, with no append arriving, and
>   the next append acks
> - **And** a first write that appends, panics in mining and unwinds keeps
>   its tenant reservation: another new id cannot take that slot, and the
>   entry's settlement converts the reservation to `Held` when it installs
>   the tenant
> - **And** under repeated rollback failures the byte accounting stays
>   within the limit — the torn bytes count inside it — and the fixed
>   overhead outside it is capped separately: headers by `max_segments`,
>   under which a reservation needing a rotation at the ceiling is refused
>   as backpressure naming the ceiling while one that fits the current
>   segment is admitted; and seals by `max_sealed_segments`, where the seal
>   that would exceed it puts the WAL in the terminal rotation state, which
>   the next append reports

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
> - **And** a barrier taken while an `unmined` entry is unresolved, with a
>   healthy store, stamps no mark past the entry's clamp and installs no
>   snapshot for the entry's tenants while installing the others'; a restart
>   from that state restores those tenants from their previous snapshot and
>   replays the frame; once the rebuild settles the next barrier stamps the
>   latest successful offset and snapshots them again
> - **And** with two entries unresolved the mark is the lower clamp;
>   settling the lower entry alone moves the mark up to the remaining
>   entry's clamp and no further, and only settling both restores the latest
>   successful offset
> - **And** an entry whose tenant's restorable horizon is below its
>   `reclaimed_through` is not rebuilt: the tenant's appends are refused
>   through `tenant_state`, after the terminal-rotation check and before
>   the reservation — an oversize payload for it is still `TooLarge` and a
>   terminal WAL still reports the WAL's state — under the
>   server-terminal, client-retryable class naming it, its entry moves to
>   `Refused` and out of the clamp set, its frames are held by the floor's
>   pin rather than the clamp, the node-wide checkpoint advances past other
>   tenants' frames, no snapshot is installed for it, the state is
>   exported, and other tenants ingest, snapshot and reclaim normally
> - **And** `read_frames_from` started at `FrameCursor::after(from)`
>   returns the frame after `from` first and the `to` frame last, only that
>   tenant's frames; started at `oldest_frame(tenant)` it begins at the
>   tenant's oldest surviving frame; each call returns at most `limit`
>   frames within one segment and the guard is released between calls — an
>   append issued mid-replay lands between two of its calls
> - **And** a panic inside the settlement — in the bounded replay or in
>   `replace_tenant` — returns the entry to `Unresolved` (or marks the
>   tenant unrecoverable) and signals the waiters before the tick resumes,
>   so the next trigger settles it and a waiting request is refused with a
>   fresh hint rather than held forever
> - **And** the barrier tick and an admitted ingest turn racing on one
>   `unmined` entry produce exactly one rebuild; the loser observes the
>   entry `Settling` and runs only after the rebuild has installed the
>   tenant and the entry is gone; a request for that tenant issued
>   mid-rebuild is refused at admission as `SettlementInProgress` — `503` /
>   `UNAVAILABLE`, protobuf `Status` naming the tenant, `Retry-After` and
>   `RetryInfo` carrying the settlement remainder, `error.type =
>   wal_settling` — with the settling set read and the sequence reserved
>   under one hold of the admission mutex, so a settler cannot interleave
>   between them, and admitted after settlement; a turn for another tenant
>   takes its sequence and is not held; no settlement path takes the gate
> - **And** settlement rebuilds from the `SnapshotLedger`'s retained state,
>   not the directory: a `.snap` renamed but not yet parent-fsynced, or left
>   by a failed write, is never read
> - **And** a panic raised inside `ingest` after a leaf was widened and
>   before its audit event was emitted is settled by a rebuild from the
>   tenant's last installed snapshot: the widening is re-derived with its
>   event, the version's audit history is complete, the records at or
>   below the tenant's publication watermark — and, in the entry's own
>   frame, below `emit_from` — are present exactly once, and those in the
>   ambiguous span are present at most twice, never absent; a frame with
>   one record durable and a later one lost does not advance the
>   watermark; a crash before the next checkpoint after a retried panic
>   yields at most one more copy of the records published since the last
>   durable watermark, and none once `PUBLISHED` has been written; the
>   same panic on a tenant with **no** installed snapshot is settled by a
>   full replay from its oldest surviving frame, and a template defined
>   before the span is present in the rebuilt tree; `replace_tenant` leaves
>   every other tenant's tree and the allocator's next id unchanged
> - **And** the same holds for a panic raised after `ingest_mined` returned
>   — inside the no-pool branch's inline `emit`, or the pool branch's
>   `submit` — and for a panic inside the salvage's own `emit`: the record
>   is present at most twice, never absent, and the salvage count is not
>   consulted; `read_frames_from` on a span that does not end on a frame
>   boundary is an error that leaves the entry in place
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
>   installs no snapshot, advances no checkpoint, is counted, and the task
>   captures the next tick's cut on schedule
> - **And** the same holds for the publisher thread: a panic inside a
>   batch requeues it and the thread continues; a dead publisher is
>   respawned and settles the batches still queued; a worker enqueuing on
>   a disconnected queue buffers its batch instead, and `quiesce_publishes`
>   and shutdown return
> - **And** the epoch latch clears without a restart: a worker panic bumps
>   `failure_generation` and then lowers `failed_epoch`, in that order, the cut that
>   drains the requeued records stamps and clears by a CAS on the pair it
>   read at its capture, and a panic raised during that cut — at the same
>   epoch or a later one — changes the packed word so the CAS fails and
>   the failure survives for the next cut; a **same-epoch** report between
>   a capture and its clear is caught, which a two-atomic pair would miss

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
> - **And** a restart seeds the tenant admission table from the tenants
>   recovery restored, before any listener is constructed: a held tenant
>   is admitted at once and a new id at the guard is refused, with no
>   first-write race window at startup
> - **And** a kill after a seal's parent fsync and before its rotation
>   leaves the sealed tail in the newest segment: recovery heals it to the
>   seal's length, consumes the seal, and the post-recovery step — after
>   the ledger rebuild, before the coordinator exists — performs the owed
>   rotation before the first append; a newest segment whose seal does not
>   verify halts; a `.wal.seal.partial` is swept and never consulted
> - **And** a kill after the heal's truncate and before the seal is consumed
>   leaves a verifying seal whose length equals the segment's: the next
>   start consumes it and continues, and the frames before it are delivered
> - **And** a sealed segment is unlinked with its seal — segment, parent
>   fsync, seal, parent fsync — both counted against the pass cap; a kill
>   after the segment's fsync and before the seal's leaves an orphan seal
>   that the next pass sweeps, a kill after the seal's unlink and before its
>   fsync cannot bring the segment back, and an orphan seal and a
>   `.wal.seal.partial` are swept on a pass with no checkpoint yet

> **Scenario RFC0053.5 — The backpressure state is observable**
> - **Given** a node that enters and then leaves the refusing state
> - **When** metrics are collected and logs are read across both transitions
> - **Then** the refusal-latch gauge with its cause, the limit and
>   measurement at the last refusal per cause (bytes with its measurement,
>   the retained count for segments), the held count at the last
>   `TenantCapacity` refusal on its own gauge, and `capacity_remaining` — equal to `max(limit − unreclaimed,
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
  no torn tail and a rebuild hook placed only on the heal path would pass
  it — plus a third, the sealed closed segment: a fixture with a torn tail
  in a non-newest segment and a matching `.wal.seal`, asserting the frames
  before the seal are delivered, the rebuild excludes the torn bytes, and
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
  a header-only current segment (contributing nothing) and a torn newest
  tail — a refused-stays-refused check alone would pass a rebuild that
  counted headers or torn bytes; a fault-injected leg makes the rebuild's scan fail and asserts
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

- **An admin verb that releases a `Held` tenant slot.** This stage never
  evicts an idle tenant and the only operator path at the `max_tenants`
  guard is to raise it; in open mode, where ids are client-chosen, that is
  also the only answer to a client that filled the guard (§3.1 states why
  no fairness mechanism belongs there). An admin release verb is the
  follow-up. A release verb would need a definition of "idle" the WAL,
  the snapshot ledger and the `RECLAIM` record all agree on, and under RFC
  0047/0048 tenants are an out-of-band decision, so the guard is the wrong
  place to make it; deferred.

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
