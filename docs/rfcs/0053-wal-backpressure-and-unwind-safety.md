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
> §3.4 throughout. It **amends accepted RFC 0046**: its replay validation
> and criterion RFC0046.11 reject a tenant length above 256, which
> contradicts that RFC's own resolved-questions note recording RFC 0048
> §3.1's 1–128-byte grammar, so both are amended from 256 to 128 — making
> the text consistent with a decision RFC 0046 already records, not a new
> one — alongside the frame codec §3.2 lowers to the same bound. It also
> **amends accepted RFC 0005's audit-sink durability
> clause** (§7, "The writer guarantees no audit event is lost across
> crashes…"): a permanent audit write failure stops being a silent drop
> that reports success and becomes a third outcome that refuses the
> dependent record publish and makes the tenant terminal, per §3.2.

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
**closed** segments — sealed ones among them, since a sealed segment is
closed; the *current* segment is outside the cap, for the reason §3.1
gives below — defaulting to
`unreclaimed_bytes_limit / segment_size_bytes + 16` so a bound's worth of
full closed segments always fits with slack for the near-empty ones, and a
reservation whose append would rotate, while `closed_retained` is at the
ceiling, is refused under the same backpressure class with the `Segments`
cause. Whether the append would rotate is the WAL's question, not the
coordinator's, and the WAL's predicate is not size alone: `Wal::rotation_due`
also rotates a *fitting* frame once the current segment's age has passed
`segment_age_secs` (a header-only segment excepted), so a coordinator that
reserved on size would let an age rotation create a segment past the
ceiling, or block the forced rotation and keep the livelock. So `Journal`
exposes the complete predicate, `fn rotation_due(&self, framed_len: u64)
-> bool` — size or age, the same function `append` consults — and the
reservation reserves a segment slot exactly when it is true:
`closed_retained + 1 ≤ max_segments`, else refuse. **The cap counts
*closed* retained segments, and the current segment is outside it**, which
is what keeps an owed rotation from deadlocking: housekeeping never
unlinks the current segment, so a cap that counted it could be reached in
a state no pass can relieve — every older segment pinned or ineligible
while the sealed current segment's own frames are checkpoint-covered — and
the deferred owed rotation would wait for a slot that never comes, with
the WAL unable to accept anything at all. Capacity is therefore reserved
rather than hoped for. The invariant, stated once: **an owed rotation
always has room.** The segment it creates becomes the new current segment
and is outside the cap; the segment it closes was outside the cap and
enters it, which is the one admission the cap does not gate, since
refusing it would mean refusing to close a segment that must never take
another frame. A *discretionary* rotation is gated as before, so the cap
still bounds retained header overhead and still refuses an append that
would grow it. **The predicate is evaluated once, and
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
mutex, so no append interleaves between them either. `validate_config` rejects `max_segments < 1`:
a ceiling of zero admits no retained closed segment at all, so the first
closed segment would refuse every later discretionary rotation and the
node would stall on any frame needing one. One is the smallest ceiling
that admits a closed segment beside the current one, and the derived
default is far above it. (An earlier draft required two, because the cap
then counted the current segment; with the current segment outside it the
floor is one, and the two rules move together.) RFC0053.1 asserts the
validation. At the ceiling a **discretionary** due rotation is **refused,
never squeezed in** — an *owed* rotation is the stated exception and
proceeds regardless, since closing the current segment necessarily adds a
closed one and refusing that is the deadlock §3.1 removes; the exception
is why the ceiling reaches no rotation-failure state at all, and why the
`Deferred { AtSegmentCap }` variant an earlier draft proposed is
withdrawn. For the discretionary case: the request path runs no housekeeping,
since prepare, file half and commit put fsyncs in front of every
concurrent append; the refusal sets the latch, the next `maintain` pass
reclaims what it can, and the client returns after the `Retry-After` §3.1
computes — so with a reclaimable segment the request is refused once and
admitted after the pass, and with nothing reclaimable it stays refused,
and no forced rotation runs at the ceiling (below). A reservation that would not rotate
is unaffected, since it creates no header. **Every rotation takes the slot
check, not only the reserved one**: RFC 0052 has two append-independent
rotation callers — the barrier task's idle rotation (§3.2, rotate-before-
cut under the exclusion) and the post-recovery step's owed rotation — and
a *discretionary* rotation from either at `closed_retained ==
max_segments` would create the closed segment the reservation refuses. So the check lives in `Wal::rotate`
itself, under the journal mutex — **and the call says which kind of
rotation it is**, because the WAL cannot infer the owed exception from a
bare request and an implementation given no reason would defer the sealed
and recovery-required cases too. `Journal::rotate` therefore takes it:
`fn rotate(&mut self, kind: RotationKind) -> Result<RotationOutcome,
ReceiveError>` with `RotationKind::{ Discretionary, Owed }`, the cap
**checked only for `Discretionary`**; an `Owed` call rotates whatever the
count, which is the invariant §3.1 states. At the ceiling a
`Discretionary` call performs no rotation and returns
`Ok(RotationOutcome::RefusedAtSegmentCap)`. RFC 0052 §3.7
defines `Journal::rotate` as `fn rotate(&mut self) -> Result<(),
ReceiveError>`; this RFC amends it to `fn rotate(&mut self, kind:
RotationKind) -> Result<RotationOutcome, ReceiveError>` — the argument
naming the exception and the return naming the outcome, `RotationOutcome::{
Rotated, RefusedAtSegmentCap }` — so that a refusal at the cap is a typed
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
discharge of an owed rotation — and there the cap does not apply at all: an owed rotation **always has
room**, because the cap counts closed segments and the one it creates
becomes the current segment (§3.1's invariant). There is no refusal to
classify, which is why the `Deferred { AtSegmentCap }` state an earlier
draft proposed is withdrawn and `RotationState` keeps `Healthy`,
`Retrying` and `Terminal` alone. Header overhead is therefore at
most 24 B × (`max_segments` + 2) — the retained closed segments, the
current one, and the one an owed rotation may create — and a discretionary
refusal leaves with the same latch once a pass removes a segment. **A segment stays in it
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
sidecars and the `CHECKPOINT` temp — RFC 0052 §3.2's `RECLAIM` is
preallocated at open and rewritten in place, so it has none, and
`PUBLISHED` takes the same shape — the `.wal.seal` sidecars §3.1 introduces below
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
  gives the coordinator the number **at construction** rather than on the
  path: `CommitCoordinator::new` takes `max_frame_bytes` beside the
  batch window, the segment size and the byte limit it already receives,
  and the coordinator rejects `payload_len > max_frame_bytes` as
  `TooLarge` under the admission mutex, before any journal lock is taken.
  It is configuration, not mutable journal state — `MAX_FRAME_BYTES` is a
  constant of the format — so reading it through `Journal` would have
  forced the oversize check under the journal mutex and made the stated
  precedence unimplementable; the WAL's own check remains as the backstop.
  The full admission order is one sequence, stated once, and it spans the
  two locks §3.2 fixes rather than living in either alone: under the
  **admission mutex**, *max-frame validation, then the tenant checks* —
  the unrecoverable state and the `max_tenants` guard — and the settling
  set; then, under the **journal mutex** and in the same hold as
  everything else that reads journal state, *the terminal-rotation check,
  then the bound*, then the rotation decision, the reservation and the
  append. So an oversize payload against a terminal WAL is `TooLarge`, a
  legal payload against a terminal WAL is the terminal classification
  without a `reclaim_state()` read, a legal payload for an unrecoverable
  tenant (§3.2) is that tenant's terminal classification, a legal payload
  for a *new* tenant at the `max_tenants` guard is a `TenantCapacity`
  refusal, and only a legal payload for a held, healthy tenant against a
  healthy WAL reaches the reservation. **Terminal still precedes
  backpressure**, which is the precedence that carries weight: both are
  read in the same journal hold, terminal first, so a terminal WAL is
  never reported as a bound. What moved is the tenant-versus-terminal
  order — a tenant refusal now wins over a WAL-terminal one, where an
  earlier draft had it the other way — and that is harmless by
  construction: `TenantCapacity` and both terminal classifications render
  the same `503` / `UNAVAILABLE` with no retry hint, so a client's remedy
  is identical and only the named reason differs, where the tenant is the
  more specific of the two. The tenant check has a source too:
  the receiver resolves the tenant out of band before `ingest` (RFC 0046
  §3.1 — `Pipeline::ingest` already takes the `TenantId`), the commit path
  carries it to the coordinator, and the coordinator consults its own
  `fn tenant_state(&self, tenant: &TenantId) -> TenantAdmission` (`Healthy`
  or `Unrecoverable`, set by §3.2's settlement) under the **admission**
  mutex, with the rest of its policy — the WAL-global `rotation_state()`
  cannot say it, since the journal has no tenant in view, and the
  coordinator's own map is not journal state. The classification is the same server-terminal,
  client-retryable class, naming the tenant; other tenants are unaffected. The terminal check has a named source: `Journal`
  gains `fn rotation_state(&self) -> RotationState` (`Healthy`, `Retrying {
  attempts }`, `Terminal`), a cheap categorical read with no snapshot
  struct on the append path, taken **under the journal mutex, in the same
  hold as the reservation and the append** — one acquisition, stated here
  and in §3.2 the same way, because the alternatives both fail: read under
  the admission mutex the state can change before the reservation, and
  read under a separate journal acquisition the hold is no longer one, so it can neither race outside the mutex nor be
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
segment-bound**, and it is written against RFC 0052 §3.3's bumped
`SEGMENT_VERSION` — 1 → 2, both accepted on read — so a seal names a
version-2 segment and the frameless base case above sits on that same
version: it carries a format version, the segment's own UUID, the
last good length, and a checksum of those three, so a stale or corrupted
seal cannot authorise anything. The rotation that follows the seal obeys RFC 0052 §3.3's two standing
rules for every rotation path: it **discharges a pending `Rotation` fsync
obligation before starting another rotation**, refusing against the retry
budget if that discharge fails, and it creates **no version-2 segment
before the `RECLAIM` record is durable**, so a sealed segment never becomes
the predecessor of a segment the record cannot account for. With those
satisfied it takes the slot check like any other,
but it is an **owed** rotation in §3.1's sense — the sealed segment must
never take another frame — so the refusal is a fault rather than a shrug:
and it **rotates whether or not a slot is free**, because it is an `Owed`
rotation and §3.1's exemption is what removed the deadlock: the cap
counts closed retained segments, the segment an owed rotation creates
becomes the *current* one and is outside the cap, and refusing to close a
segment that must never take another frame is the wedge this design
exists to prevent. The overrun it may leave is bounded — one segment,
until the next pass reclaims — and §3.3's gauge carries it as `over_cap`.
An earlier draft deferred the rotation here under a `RotationState::
Deferred { AtSegmentCap }`; that variant is **withdrawn**, since with the
exemption there is no state to represent: the cap can no longer stop an
owed rotation, so nothing reaches it through the ceiling. `RotationState`
therefore keeps `Healthy`, `Retrying` and `Terminal`, and only the
exhausted retry budget reaches the last. RFC0053.1 asserts the exemption
rather than a deferral. If the seal write itself fails the WAL
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
gains **`max_tenants`** (default 1024, same `${env:VAR}` and Helm path) —
a knob that **sizes two persisted layouts**, not just an admission rule:
RFC 0052 §3.2's `RECLAIM` slots are sized from it and
`max_unlinks_per_pass` (5,295,136 B a slot, 10,590,304 B the file at the
defaults), and `PUBLISHED` below is sized from it alone, both fixed for
the life of the file. Neither file can grow, so **the admission guard is
what prevents record overflow**: a new tenant id at the cap is refused
here rather than overrunning a slot there, which is why the two RFCs land
as a pair and why lowering the knob is an open-time decision (below):
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
installed in the miner — and **the transition has a stated home**, because
both of those happen outside the admission mutex and the lock order
forbids reaching back for it while the miner lock is held. So the turn
records the transition *after* it releases the miner lock, **reacquiring
the admission mutex alone**, which the order permits precisely because
nothing else is then held; the same reacquisition carries the rollback,
releasing the reservation when the sync failed or the install did not
happen. The table and the exported count cannot diverge because they are
one write: `ourios.wal.tenants.usage`'s `held` and `reserved` states are
derived from the table under that same hold, never counted separately, so
a reader sees `Reserved` or `Held` for a given id and never a total that
disagrees with the table it came from. The slot is released only when the
*last* in-flight first write for that id has settled without a success
**and left no frame behind** — **and that id has no unresolved `unmined` entry**. The
frame clause matters because the two failure shapes differ: an append that
failed never reached the segment, so nothing survives and the slot is
released; a *sync* that failed leaves the frame in the segment
unacknowledged, and replay after a restart re-mines it and seeds that
tenant `Held`, so releasing the slot on a sync failure could let another
id take it and put the recovered set over `max_tenants`. So a sync
failure keeps the reservation for as long as its frame can be replayed —
until the frame is reclaimed or a later write for that id succeeds and
converts the reservation to `Held` — which is exactly the retain rule the
entry case below uses. The recovered set can therefore never exceed the
cap by this path, and the other way it could — an operator lowering
`max_tenants` between runs — is **refused at `Wal::open`**, the single
rule §3.2's sidecar states: the file is sized from the knob, so a recorded
set larger than the new limit cannot be represented at all, and with no
eviction in this stage it could never drain. The node does not start; the
operator raises the limit back or starts a fresh root. An earlier draft
said the set was seeded whole and only new ids refused, which contradicted
that and left an over-limit set with no way down; the open-time rejection
is the rule everywhere — admission, sidecar and the
`ourios.wal.tenants.usage` states alike, which can therefore never report
`held` above `limit`. The exception matters because settlement
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
discharged by a later pass with no append arriving. The rotation itself runs **under the journal mutex alone** — the guard
`maintain` still holds for the commit — with the **admission mutex
released**, matching the invariant §3.2 states: `rotate` does directory
work, so a lock above the journal mutex must not span it, and holding
admission there would queue every request behind the rotation's fsync.
The journal hold is what keeps an append from interleaving between the
commit's verdict and the rotation; admission is not needed for that and
is not taken.
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
no-op rule — **for a `Discretionary` call only**: `Ok` without a new
segment when the current one holds no frames, so a timer that calls it
unconditionally cannot manufacture empty segments. An **`Owed`** call has
the opposite need and takes the opposite answer: the segment a seal has
closed must never take another frame *whether or not it holds one*, and a
frameless sealed segment is exactly what a rollback to the header leaves,
so an unconditional no-op would refuse the rotation that seal requires and
leave the WAL writing into a segment it has just forbidden. So `Owed`
rotates regardless of the frame count, and the frameless segment it leaves
behind is the one §3.1 makes unconditionally eligible.
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
drop there is a real gap against RFC 0005 §7's no-loss contract, and it
is **out of this RFC's scope** — its subject is the ordering between
records and the events that describe them, not the denial stream, which
has no records to order against. The gap is named here rather than papered
over, and §7 carries it as a follow-up: denial events need either their
own durable path or an explicit exemption in RFC 0005's contract, and
that is RFC 0026's ground to settle.
The per-partition settlement below reads that third outcome as *not*
settled. RFC0053.2 asserts a permanent audit failure leaving the records
unpublished and requeued and the tenant terminal.

The duplicate this creates is a new class, and it is stated rather than
handed to recovery. Record and audit publishes write fresh `UUIDv7` object
keys, so a panic *after* the store accepted an object but *before* the publish
returned leaves that object in place and requeues its rows; the next publish
writes them again under a new key, and two query-visible objects carry the
same rows, in the same process. Recovery's replay suppression is a
restart-time mechanism over WAL frames and never sees this. That is bounded within a process, but a **crash** after the ambiguous PUT
turns it unbounded: the rows are requeued, a normal drain republishes them
under a third key, and a restart replays the frame and publishes a fourth
— the intent-and-frontier protocol §3.2 defines covers an `unmined`
settlement and reaches none of this. So the **deterministic key covers
every ambiguous retry**, not settlements alone: when a publish is
requeued after its put may have been accepted — the ambiguous arm, which
the `RecoverableBatch` already distinguishes from a clean failure — the
retry names its object by a **name-based (RFC 4122 v5) UUID**, rather
than a fresh `UUIDv7`, derived over the **subspan, not the frame range**:
the tenant, the partition key, the frame range *and the record-index
range within the first and last frames*. Two implementations must derive
the same name from the same rows, so the derivation is normative rather
than descriptive — a sketch would let two builds, or a rebuilt client,
produce different names for one span and defeat the whole point. The
**namespace UUID** is a constant of this RFC,
`6f757269-6f73-5055-424c-000000000001`, fixed for the life of the format
and never derived from configuration. Its bytes spell the name: `6f 75 72
69` and `6f 73` are `ourios` across the first two fields, `50 55` and `42
4c` are `PUBL` across the third and fourth. The **name** is the canonical serialisation below,
hashed as RFC 4122 v5 prescribes; integers are **little-endian**, as
everywhere else in this stack, and every variable-length part carries its
own `u16` length before its bytes so no two field sequences can alias:

| # | Field | Bytes |
|---|---|---|
| 1 | tenant id | `u16 len` then `len` bytes, the id verbatim |
| 2 | partition key | `u16 len` then `len` bytes, the key's canonical string form |
| 3 | first frame | 16 B segment UUID (RFC 4122 byte order) then `u64` byte offset |
| 4 | first record index | `u32` |
| 5 | last frame | 16 B segment UUID then `u64` byte offset |
| 6 | last record index | `u32` |

Fields are concatenated in that order with no padding and no separators,
the lengths making the encoding unambiguous. Nothing else enters the
name — not a timestamp, not a retry count, not the record contents —
because a retry must reproduce it exactly. The index range is what makes the identity stable, because a
frame can be partially published: §3.2's `emit_from` means a retry may
carry records `[emit_from, end]` of a frame whose earlier records went
out in another object, and a key over the frame range alone would derive
one name for two different sets of rows — the store would overwrite one
with the other rather than collapse a duplicate, which is worse than the
duplicate it was meant to prevent. **It survives replay** because the
index range is reconstructed, not remembered: `emit_from` is recorded in
the `unmined` entry and the replay emits from exactly that index, so a
restart re-derives the same first index, the same last index and the same
name for the same rows. Every later attempt at those rows writes that same name.

**Three limits on that claim are stated here rather than discovered
later, because each of them bounds what the key can buy.**

*It bounds keys, not objects.* Whether two PUTs under one key leave one
object is the backend's business, and the accepted storage contract does
not promise it: RFC 0013's conditional PUT is specified for
*create-if-absent* (`If-None-Match: *`) and its compare-and-swap half for
manifest generations, while the plain `Store::put` documents no overwrite
semantics at all. So the bound this RFC claims is **one object per
distinct key, with every retry of the same rows producing the same key** —
on a backend whose same-key PUT overwrites, that is one object; on one
that does not, it is one object per attempt but never a *different* set
of rows under a name already used. The RFC does not require overwrite
semantics of a backend, and does not test for them.

*It is a records bound.* The identity is a record subspan, and audit
groups have no record indexes to range over, so the key does not apply to
them. Audit retries are bounded differently and separately: an audit
group is regenerated per frame by the miner (RFC 0052's regeneration-only
replay), and the per-tenant audit frontier §3.2 defines is what stops a
restart re-emitting a group already durable. Within a process an
ambiguous audit write costs at most one duplicate group, which the sink's
own permanent-drop counters make visible.

*It is an in-process bound, and the cross-restart half is dropped.* The
ambiguity lives in the `RecoverableBatch` — it knows the put may have been
accepted — and a crash takes that knowledge with it, so a retry after a
restart could only re-derive the same name if something on disk recorded
which subspan was ambiguous. Recording it would mean a sidecar write
**before every potentially ambiguous PUT**, which is every PUT: a cost on
the normal publish path, paid always, to bound a case that arises after a
panic or a store timeout. That trade is not worth making, so this RFC does
not make it, and it does not claim what it cannot back — **the key bounds
retries within a process; across a restart the bound is the per-tenant
publication frontier**, which is durable and which §3.2 already relies on:
a frame at or below it is not re-emitted, and one above it may be
published again, at most once per restart that replays it. The one place a
cross-restart identity *is* durable is the settlement's own intent span,
where `publish_marks` writes the marks before the rebuild publishes — that
path keeps its stronger claim precisely because it already pays for it. Only a publish that has
never been ambiguous keeps its `UUIDv7`. This RFC accepts the one object,
counted through the `cadence_panic` counter beside the flushed-partition
counters, and asserted by RFC0053.2. The bound holds only because settlement is
**per partition**, stated below: a drained batch spans several record
partitions and audit groups, and requeueing the whole batch after a panic in
the third put would duplicate the two objects already accepted. **Two settlements are meant by that word, and this RFC keeps them apart.**
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
RFC0053.2 covers the derive failure beside the write failure. The record sink's
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

**An ambiguous requeue keeps its own identity**, which the deterministic
key of §3.2 depends on: that key is derived over the partition's frame
range, so a requeue prepended into the live buffer would take whatever
arrived during the in-flight PUT along on the retry, changing the range,
changing the key, and leaving both objects behind — the very duplicate the
key exists to collapse. So a batch requeued from the **ambiguous** arm is
held as its own unit against that partition rather than merged into it,
and the next publish writes that unit first and alone, under the key its
original range derives; records that arrived meanwhile stay behind it and
publish as their own batch after. Only the ambiguous arm needs this — a
clean failure wrote nothing, so its records may merge freely. Making the publish idempotent — a drain-time object
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
a sequence**. **One protocol, stated once and the same in both sections.** The work
splits by which state it reads, not by convenience. The **admission
mutex** covers the coordinator's own policy — max-frame validation, the
tenant checks, the settling set, and the tenant slot — and is released
before the journal is touched. The **journal mutex** covers everything
that reads or mutates journal state, and that includes the
**terminal-rotation check**, which an earlier draft put under admission:
`rotation_state()` reads the journal, so reading it under the admission
mutex leaves the state free to change before the reservation. So the
journal hold is the terminal check, the rotation-due evaluation, the byte
and segment reservation taken from `reclaim_state()`, and the
`append_batch` that consumes the resulting `RotationDecision`, all in
**one hold**, which is what §3.1 requires and
what makes the token sound. The binding is therefore atomic where it must
be — a reservation cannot be made against a journal another append has
moved — and the rollback is local to that hold: an append that fails,
whether on rotation or on write, releases its byte and segment
reservation before the mutex is dropped, and a rotation inside the append
does its create, rename and parent fsync under the journal mutex alone.
That is not an exception to the rule but the reason for its wording: the
journal mutex **is** the WAL's single-writer lock, so a rotation
necessarily holds it while it touches the directory, and no arrangement
of locks can change that. What the rule forbids is a lock *above* it
being held across that work, and the admission mutex is released before
the append for exactly that reason — so an fsync inside a rotation
delays that one append and not every other request's admission.
That keeps the admission mutex clear of directory I/O, which this RFC's
lock order requires, and keeps reservation and append inseparable, which
§3.1 requires; the earlier draft that held the admission mutex across the
append satisfied the second at the cost of the first. **The claim that carries the correctness is the one taken under the
barrier exclusion and the miner lock**, not the admission-set insertion.
Publishing `Settling` under the admission mutex refuses *later* requests
early, which is worth doing, but it cannot order a turn that is already
past that point: such a turn could mine after the claim and have its work
discarded by `replace_tenant`. What closes that is RFC 0052 §3.1's own
span — an ingest turn holds the exclusion from the miner work through
`pool.submit` and the `last_durable` update — so a settler that takes the
**exclusion and then the miner lock** to claim the entry cannot interleave
with any mining span at all: no turn is mid-mine while it holds them, and
no turn can start one until it releases. A turn that appended before the
claim but had not yet mined waits at the exclusion and mines afterwards,
correctly, because the rebuild replays only to the entry's own span and
that turn's frame lies above it. So the mining span is excluded whole, the
admission-time refusal is an optimisation with no correctness weight, and
post-claim appends need no special handling. **The commit sequence is not
reserved there**, and that is deliberate: `CommitCoordinator::append` today
takes the journal lock, appends, and only *then* allocates `seq` from
`FlushState`, returning `CommitOutcome { seq: None }` when the append
failed — no sequence is consumed, so the gate never waits on one that was
never used and `FlushState` has no gap to tolerate. Reserving a sequence
before the append would introduce exactly that hole, needing a no-op
completion on every failure path to keep the gate advancing. So sequence
allocation stays where it is, after a successful append and under the
journal mutex; the gate itself is untouched, awaited outside both locks as
today. RFC0053.1 drives the concurrency: a settler claiming a tenant while
a turn for it is between its admission check and its append, asserting the
turn's frame is appended, waits at the exclusion, and is mined against the
rebuilt tree afterwards — nothing mined into a tree about to be replaced,
and nothing replayed twice, since its frame lies above the rebuild's
span.

**One lock order covers all four locks**, since this RFC adds the first of
them to the three RFC 0052 §3.1 fixes. Top to bottom: **admission mutex →
barrier exclusion → miner lock → `last_durable`**, with the **journal
mutex** taken below the admission mutex and never above it — an ingest
turn takes the admission mutex for the coordinator's policy checks
(max-frame, tenant state, tenant slot, settling set) and **releases
it**, then takes the journal mutex alone for the terminal check, the
rotation decision, the byte and segment reservation and `append_batch` in
one hold — whose
success allocates the sequence, and inside which a rotation does its
directory work under that mutex only — then follows
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
hierarchy **above the journal mutex** is held across store or directory
I/O — not the admission
mutex over the file half, not the barrier exclusion over a cut's flush
(§3.2), not the miner lock over a snapshot write; and a
settler **publishes its claim under the admission mutex — adding the
tenant to the settling set — before it takes the barrier exclusion**, so
the claim is visible to admission from the instant it exists and no
request can be admitted for a tenant whose rebuild has begun. The miner
lock still carries the `Unresolved`/`Settling` transition on the entry
itself, which is what makes exactly one trigger the settler; the
admission-mutex insertion is what makes that decision visible to requests.
**And every completion path removes it again**, under the admission mutex
alone, in the same reacquisition pattern the tenant table uses: a rebuild
that **succeeded** removes the tenant from the settling set after it has
removed the entry, so the next request is admitted; a rebuild that found the tenant
**unrecoverable** removes it as well, because that tenant is refused by
its own terminal state from then on and two refusal reasons for one tenant
is one too many. A rebuild that **unwound** is the exception, and
deliberately so: the entry goes back to `Unresolved` but the tenant
**stays refused**, because the tree is half-mutated — the panic left it
between the widening and its audit event, which is the state the rebuild
exists to repair — and admitting a request into that window is exactly the
defect the claim prevents. So admission is refused for as long as the
tenant has an `unmined` entry at all, `Unresolved` or `Settling` alike,
and the set is really "tenants with an unresolved entry" rather than
"tenants a settler is holding". The **retry is the barrier tick**: it
settles every unresolved entry at the start of each tick (§3.2), so the
next tick claims the entry afresh, and no ingest path has to coordinate a
retry it cannot perform while refused. So
`ourios.wal.settling_tenants` returns to zero when a rebuild succeeds or
the tenant goes terminal, and stays nonzero — visibly — while a panicked
rebuild awaits its next tick. RFC0053.2 asserts all three paths.
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
client-retryable class naming the tenant — **and a turn already admitted
re-tests that before it mines**, since no replacement tree is installed in
this branch and a turn that passed admission before the settler ran would
otherwise mine and publish against the poisoned tree once the exclusion is
released. The check sits at the top of the mining span, immediately after
the turn takes the miner lock and before its first `ingest_mined` — and
it reads an **atomic**, not the admission table, since the lock order
forbids reaching for the admission mutex while the miner lock is held.
The coordinator publishes each tenant's admission state as a per-tenant
`AtomicU8` the settler stores with `Release` **under the admission mutex
before it releases the barrier exclusion**, and the turn loads with
`Acquire` after taking the miner lock. That linearises without a second
lock: a turn holds the exclusion across its mining span, so the settler's
store precedes its release of the exclusion, which precedes the turn's
acquisition of it, which precedes the load — a turn either ran wholly
before the settler claimed the tenant or sees `Refused`. Finding it, the
turn abandons the batch with the same terminal error rather than mining
it. The frame is
already durable and unacknowledged, so abandoning loses nothing a restart
will not replay — and a restart is what clears the state. Its entry leaves
the clamp set for a per-tenant `Refused` state, so `last_durable` and the node-wide
checkpoint advance for everyone else; its frames are retained instead by
RFC 0052's own pin — the receiver drops the tenant from the
`SnapshotHorizons` it hands to `maintain`, so the WAL pins the tenant at
its oldest surviving frame (`RetainFloor::Pinned`, strict) and reclaims
only segments holding no frame of it. **Retaining the frames is not
enough on its own**, and this is the half an earlier draft missed: the
pin governs which WAL segments survive, while what a restart *replays* is
governed by the Parquet-side suppression horizon, and letting the
node-wide checkpoint advance past this tenant's unpublished frames would
make a restart suppress records that never reached Parquet — acknowledged
data lost, which no isolation is worth. So the suppression horizon is
**per tenant**, and the `PUBLISHED` sidecar is what makes that cheap: a
tenant's replay gate is `max(S, records watermark)` from its own entry,
falling back to the checkpoint `X` only for a tenant with no entry, so a
refused tenant whose records never published has a watermark that stays
behind and its frames are replayed in full. The node-wide checkpoint may
then advance for everyone else without loss — which is the point of the
isolation, and holding `X` back for every tenant instead would
reintroduce exactly the node-wide stall this paragraph exists to
prevent; no snapshot is installed for it;
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
every frame below it is settled — **and only a durably published record
counts**: a record the sink quarantined under RFC 0025 §3.3 is settled for
*ownership* but is not in Parquet, so counting it would let recovery
suppress a frame whose row never landed. Quarantine has its own
disposition instead: such a frame advances the watermark once the
quarantined record's `record_quarantined` event is **durable in the audit
store**, because that event is the durable record of the row's fate and
replaying the frame would only re-quarantine it — at most a duplicate
quarantine event, never a lost row — and until then the watermark stops
below that frame — a frame holds several records, the
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
records: WalOffset, audit: WalOffset }` names the pair the rules need, and
the WAL writes it to a `PUBLISHED` sidecar beside `CHECKPOINT`, **after**
`CHECKPOINT`'s own write in the same call — the order every statement of
it in this RFC now gives, including the amendment §8 stages for RFC 0052,
which an earlier draft left saying the opposite. The order is the other way
round from an earlier draft, and the reason is a crash window that draft
bricked: writing `PUBLISHED` first leaves, on a power loss before the
rename, a `PUBLISHED` beside a still-version-1 `CHECKPOINT` — an ordinary
crash that the matrix would have read as a lost checkpoint and failed
closed on. Writing it second makes `PUBLISHED`-without-a-version-2-
`CHECKPOINT` reachable only by deleting the checkpoint, which *is* the
fault the matrix should catch, while the new window — a version-2
`CHECKPOINT` with `PUBLISHED` absent or stale — is exactly what
`published_seeded` already governs, and the recovery is conditional on
that witness rather than universal: with the flag **armed but
unconfirmed** the sidecar was never durably written, so seeding from `X`
is correct; with it **confirmed** a sidecar that is now missing is a lost
file and fails closed, as the matrix says. Only the first branch seeds
from `X`, and a *stale* sidecar under either flag is not a fault at all:
its entries still **govern** for the tenants they name, per the
authoritative-entry rule below, which replays a little more rather than
suppressing what was never published.
Neither order is unsafe for the seed itself — a present, valid entry
governs its tenant either way, and the node-wide horizon reaches only the
tenants without one — so the order is chosen for which crash window it
leaves, not for which file wins. The sidecar takes RFC 0052 §3.2's
`RECLAIM` shape rather than a temp-and-rename: a fixed byte layout with
its own magic, two slots written alternately under a generation counter
with a CRC32-C per slot, **preallocated at open and rewritten in place**,
so a torn write is caught by the slot's checksum and the older slot still
reads. The layout is `RECLAIM`'s **shape**, not its bytes: the two files hold
different things, so claiming identity would be false. What they share is
stated, and then `PUBLISHED`'s own layout is pinned in full. **Shared:**
the two-slot protocol with a `u64 generation` and the higher live, a
CRC32-C per slot over its own preceding bytes, preallocation and in-place
rewrite with a `.new` only for a resize, pinned offsets, little-endian
integers, RFC 4122 byte order for UUIDs, and every reserved byte zero and
checked on read. **Different:** `PUBLISHED` has no planned area, so its
`slot_len` derives from `max_tenants` alone and its trailer sits at an
offset a reader computes from that; its entry carries two frontiers and
two validity bits where `RECLAIM`'s carries one `occupied` bit; and its
file header records only the capacity that sizes it.

| Structure | Offset | Size | Field |
|---|---|---|---|
| File header | 0 | 4 | magic `b"OWPB"` |
| | 4 | 2 | `u16` version (`= 1`) |
| | 6 | 2 | reserved, zero |
| | 8 | 8 | `u64 slot_len` |
| | 16 | 4 | `u32 max_tenants` |
| | 20 | 4 | reserved, zero |
| | 24 | 4 | `u32` CRC32-C over bytes `[0..24)` |
| | 28 | 4 | reserved, zero |
| Slot header | 0 | 8 | `u64 generation` |
| | 8 | 4 | `u32 entry_count` (reported, not a length) |
| | 12 | 4 | `u32 next_slot_id` — the id high-water mark (below) |
| | 16 | 2 | `u16 header_flags`, reserved and zero (the `published_seeded` witness lives in `RECLAIM`) |
| | 18 | 2 | reserved, zero |
| | 20 | 4 | reserved, zero |
| Dictionary record (array of `max_tenants`) | 0 | 2 | `u16 len` |
| | 2 | 128 | key bytes, `len` significant, remainder zero |
| | 130 | 2 | `u16` flags: bit 0 `tombstoned`, bits 1–15 zero |
| Entry (array of `max_tenants`, entry `i` = dictionary record `i`) | 0 | 2 | `u16` flags, per the table below |
| | 2 | 2 | reserved, zero |
| | 4 | 4 | `u32 generation` of the write that last changed these marks |
| | 8 | 24 | records `WalOffset`: 16 B segment UUID then `u64` byte |
| | 32 | 24 | audit `WalOffset` |
| Slot trailer | 0 | 4 | `u32` CRC32-C over `[0 .. slot_len - 8)` of the slot |
| | 4 | 4 | reserved, zero |

A slot is therefore `24 + 132 × max_tenants + 56 × max_tenants + 8` bytes
and the file `32 + 2 × slot_len`. The entry carries no slot-id field,
since its position is its id. A 32-byte file header, then two slots; each slot a
24-byte header, a **fixed entry area of exactly `max_tenants` entries**,
and an 8-byte trailer whose checksum is a `u32` followed by four reserved
bytes and covers **only the bytes preceding it**. The entry area is fixed,
not `entry_count` long: a variable area would put the trailer at a
variable offset, so a reader would have to trust `entry_count` — a field
inside the bytes the checksum protects — to find the checksum that
validates it, which is not self-delimiting and is the shape `RECLAIM`
avoids for the same reason. So **`entry_count` is a count, not a
length**, and not an index either: it reports how many entries are live
and no reader walks by it, since an entry's position is its slot id and
its occupancy is its own flags (below). Unused positions are **zeroed**,
and the trailer sits at a fixed offset a reader computes from the
header's `max_tenants` alone (Castagnoli, as everywhere in the WAL). The
encoding rules are RFC 0052 §3.2's, adopted verbatim so one reader serves
both files: **byte offsets are pinned** for every structure rather than
implied by field order, integers are **little-endian** (matching the
existing checkpoint and segment encoders), UUIDs are in RFC 4122 byte
order, and **every reserved byte is zero and is checked on read** — a
non-zero reserved byte is a format error and `Wal::open` refuses the file
rather than ignoring the field, which is what keeps a later version from
colliding with sloppy writers. **Tenants are referenced through a per-slot dictionary, not inline**, the
shape RFC 0052 §3.2 settled on and for the same reason: `TenantId` is a
validated string, not a UUID, so any fixed-width *digest* of it either
truncates or collides, and a collision would silently merge two tenants'
marks. So each slot opens with a **dictionary** of `max_tenants` fixed
records, `[u16 len][128 B key bytes][u16 flags]` (132 B, the layout RFC
0052 §3.2 pins at those offsets), `len` naming how many of the key bytes
are live and an unused record carrying `len = 0`. The trailing field is
**flags, not reserved space**: **bit 0 `tombstoned`**, bits 1–15
reserved, zero and checked on read like every other reserved field — the
tombstone below needs an encoded home, and this is it, typed identically
to `RECLAIM`'s record so one reader serves both files. every entry then references a tenant by a **`u16` slot id** into it.
The mapping is injective by construction and round-trips exactly — the
key is the tenant id itself, not a derivation.

**The two files share one id space, and that needs saying rather than
assuming.** Each carries its own copy of the dictionary — both must be
readable alone — but a copy is not agreement: if each compacted its own
tenant list on write, the same tenant would take different ids in the two
files and every cross-reading of them would be wrong. So the id space has
an owner and a lifetime. **`RECLAIM` owns it**: the WAL assigns a tenant
its slot id when that tenant is first recorded in either sidecar, in
`RECLAIM`'s dictionary. **The table open seeds from is the union of both
dictionaries, not `RECLAIM`'s alone**, and that matters rather than being
tidy: `publish_marks` writes `PUBLISHED` without touching `RECLAIM`, so a
tenant can be assigned an id and recorded only there. Seeding from
`RECLAIM` alone would lose that id at the next open and hand it to another
tenant, crossing two tenants' marks in the file that still names the
first. So the table is the union, and a record in either file — live or
tombstoned — reserves its id. **A high-water mark backs it**: each slot
header carries `u32 next_slot_id`, the lowest id never yet assigned,
written by whichever file is written and taken at open as the **maximum**
of the two, so allocation never goes backwards even if one file lags the
other by a write. That is what makes the ordinary one-file-alone case
safe rather than merely usually safe. **Ids are never renumbered and never reused while the tenant is
recorded in either file** — neither file may compact on its own, and a
tenant that leaves one but remains in the other keeps its id. `PUBLISHED`
is written from that same in-memory table, so its dictionary is that
dictionary, at the same ids, by construction rather than by convention.
**No operation reassigns an id**, not even a rebuild: a resize copies each
dictionary record into the wider stride at the same index, so ids survive
it unchanged. That is what makes a crash between the two files' renames
recoverable (§3.2), and it is why a tombstoned id is retired rather than
reclaimed — renumbering across two files cannot be made crash-safe
without a third witness, and this design would rather spend ids than
invent one.

**A tenant introduced by one file alone is the ordinary case, not an
error**, and the rule says which disagreement is which. The two files are
written at different moments — `publish_marks` touches `PUBLISHED` without
`RECLAIM`, a pass touches `RECLAIM` without `PUBLISHED` — so at any
instant one may name an id the other does not. That is benign: the file
lacking it simply has no entry for that tenant, reads as "no frontier" or
"nothing reclaimed" accordingly, and the next write of that file adds it.
What is **fail-closed** is narrower and unambiguous: an id that **both**
dictionaries name with **different keys**, which no correct writer can
produce and which silently crosses two tenants' marks if read past —
`OpenError::Corrupt`, naming both files and the id. A crash between the
two writes therefore leaves a readable pair, never a halt.

**The key is 128 bytes, which is what the governing spec says.** RFC 0046
§3.1 caps a normalised tenant id at 256 bytes, but it is not the last word:
**RFC 0048 §3.1 — accepted, and titled "Tenant id grammar (amends RFC 0046
§3.1)" — narrows the grammar to 1–128 bytes of ASCII graphic characters**,
which is why `ourios-core`'s `MAX_TENANT_BYTES = 128` matches the spec
rather than being stricter than it. Sizing the record at 128 is therefore
sizing it to the accepted bound, not to an implementation detail. (An
earlier round of this RFC read RFC 0046 alone and sized the record at
260 B; reading an amended section without following its amendment is the
mistake, and the `rfc-check` skill's status routing exists to catch it.)

**The frame codec has to be brought to the same bound, and this RFC
requires it.** `ourios-wal`'s `TenantBatch::MAX_TENANT_BYTES` is **256**
and rejects only above that, so a frame can carry a 129–256-byte tenant
that the 132-byte dictionary cannot represent — a real exposure, not a
theoretical one, since the constant predates RFC 0048 and its doc comment
still cites RFC 0046 §3.1 as its authority. The fix is to bound the
source rather than widen the layout, because widening it would double
both sidecars to carry ids no accepted grammar admits: this RFC
**amends the frame codec** (RFC 0046's `TenantOtlpBatch` payload, whose
constant RFC 0048 §3.1 silently superseded) to lower it to **128 on
encode and decode**, matching the grammar and `ourios-core`'s own
`MAX_TENANT_BYTES` — the same amendment RFC 0052 §3.2 states, worded to
match so the two cannot drift. **The amendment reaches RFC 0046's own
text, not only the constant**: its replay clause and its criterion
RFC0046.11 still reject a length above *256*, which contradicts that
RFC's own resolved-questions note recording that RFC 0048 §3.1 pinned the
grammar at 1–128 bytes as the one tenant grammar every boundary applies
at. So this RFC amends RFC 0046's replay validation and RFC0046.11 from
**256 to 128**, which makes that document consistent with a decision it
already records rather than making a new one. A root whose replay yields a tenant longer than that
**fails closed at open**, naming the offending frame's offset and the
length it carried, rather than being truncated into a dictionary that
cannot hold it. That is the pre-production posture — no migration tooling
— and it is only reachable on a root written by a build whose codec
predated this change. RFC0053.4 asserts both halves: the codec refuses a
129-byte tenant on encode, and a fixture root carrying one fails open
naming the offset.

An entry is then `[u16 slot id][u16 flags][u32 generation][24 B records
WalOffset][24 B audit WalOffset]` — 56 B, since it names a tenant by slot
id rather than by key — where a `WalOffset` is its
16 B segment UUID plus its 8 B byte offset, the pinned 24 B, and `flags`
carries four bits, assigned once here so no other paragraph has to
enumerate them:

| Bit | Name | Meaning |
|---|---|---|
| 0 | `records_valid` | the records frontier in this entry is present |
| 1 | `audit_valid` | the audit frontier in this entry is present |
| 2 | `publishing` | an intent is outstanding for this tenant (§3.2) |
| 3 | `terminal` | the tenant is in the server-terminal class (§3.2) |

Bits 4–15 are **reserved, zero, and checked on read**, a non-zero one
failing the slot's parse as every other reserved field does. The
validity bits exist because the two frontiers advance independently and a
frame can produce records with no audit event at all, so "no mark yet" is
a real state that an unconditional offset cannot express — a zeroed
`WalOffset` is a legitimate position, not an absence. A frontier whose
bit is clear is **absent**, and recovery treats that side of the entry as
though the tenant had no entry at all, falling back to the node-wide
horizon for it while still honouring the other side — **except for a
tenant the entry marks terminal**, where the fallback would be unsafe.
The flags' `terminal` bit (bit 3 above) is set
when §3.2 puts a tenant in the server-terminal class, **and a restart
clears it rather than honouring it forever** — persisting a refusal with
no exit would be a durable outage. The transition is stated so it stays
safe: at open the bit is read **for the horizon and not for admission**.
It is what tells recovery that a clear validity bit means "nothing of
that side was ever published" rather than "consult `X`", so the tenant's
frames are replayed in full; and then the tenant is admitted normally,
because the condition that made it terminal — an audit store rejecting
writes, a clock that would not resolve — is re-evaluated by the first
publish after the restart rather than assumed to persist. If it does
persist, that publish re-enters the terminal class and re-sets the bit,
visibly, on the same alert. The persisted bit is cleared on the first
`publish_marks` write that follows a successful publish for that tenant,
so the flag on disk tracks the last *known* state rather than the last
bad one. Publication stays safe throughout, because the horizon reading
happens before any admission and does not depend on the clear. That bit
is set and a terminal
tenant with a clear validity bit means *nothing of that side was ever
published*, not "consult the node-wide horizon": its events were never
written, so `X` — advanced by every healthy tenant's progress — would
suppress exactly what must be replayed. Its horizon is its oldest
surviving frame, the same position RFC 0052's pin holds it at, so a
restart re-mines everything that tenant has. Both bits sit inside
the entry area, so the slot's trailer checksum covers them like every
other byte before it: a flipped validity bit fails the slot's CRC and the
other slot is read, which is what keeps "absent" from being forgeable by
a torn write. The `u32 generation` is §3.2's staleness witness, below. **Recovery maps
through the dictionary**, not through a live tenant set: a slot id reads
its key, the key is a tenant id verbatim, and that tenant takes its marks;
an id naming a dictionary record with `len = 0` is a corrupt slot and the
other slot is read. A tenant with no entry seeds from `X`, as one with no
record always has. **An entry's position *is* its slot id**, which is what lets ids be
stable: entry `i` belongs to slot id `i`, occupancy is the entry's own
`records_valid` / `audit_valid` bits rather than its position, and
`entry_count` is a **count for reporting only**, never a length to walk.
An earlier draft made the live entries a dense prefix `[0, entry_count)`,
which cannot survive a stable id: removing a tenant would renumber every
id above it. A **tombstone** covers removal instead — the dictionary
record keeps its key with its `tombstoned` bit set (bit 0 of the record's
flags field, at offset 130 within the record), the entry is zeroed, and
the id is **retired, not freed**: no later tenant ever takes it, since
nothing in this design renumbers — a resize preserves ids and there is no
compaction pass. A tombstone therefore holds its id for the life of the
root, and an operator who exhausts the space raises `max_tenants` or
starts a fresh root. A write rewrites the whole slot — no free list, no position to leak, which
is what keeps the geometry fixed. The file's size is therefore decided at
open by `max_tenants` alone: a slot is `24 + (132 + 56) × max_tenants + 8`
bytes and the file `32 + 2 × slot`, which at the default 1024 tenants is
192,544 B a slot and 385,120 B — about 376 KiB — for the file, small
beside `RECLAIM` and a 128 MiB segment. **Changing `max_tenants` therefore changes
a persisted layout**, and two rules follow, both mirroring `RECLAIM`'s.
**The file describes its own geometry**: the 32-byte file header carries a
`u32 max_tenants` (the value it was preallocated for) beside the magic and
version, so a reader never infers the shape from configuration, and
`Wal::open` reads that value rather than assuming the configured one, and
**raising the knob is supported by a copy at open, not by a refusal**: a
refusal would strand every existing root the day an operator needs more
tenants, and the copy is crash-safe with the same primitives the WAL
already uses. `Wal::open` compares **both dimensions**, not one: `max_tenants`, which
sizes this sidecar and `RECLAIM` alike, and **`max_unlinks_per_pass`**,
which sizes `RECLAIM`'s planned list. Checking only the first was this
RFC's gap rather than a format one — RFC 0052's header already records
both — and it mattered: raising the unlink cap on an existing root would
let a pass plan more entries than the fixed file can hold, with nothing
detecting it. So the recorded pair is compared against the configured
pair, a configured value **above** either recorded one triggers the
rebuild and a value **below** the recorded `entry_count` is refused as
before. Finding either dimension raised, open rebuilds **both sidecars as
one operation**, because `max_tenants` sizes them both and resizing one
alone would let admission accept tenants reclamation cannot persist — a
root that starts and then cannot record what it reclaimed. So the two rebuilds run
together, each writing its new geometry under the temp name its own RFC
reserves — RFC 0052's `RECLAIM.new` and, in parallel, `PUBLISHED.new`, a
name the sweep selector already covers — with the old dictionary and
entries copied in, fsyncing, renaming over the old name and fsyncing the
parent, and **the node refuses to start if either rebuild fails**, naming
the file that failed.

**A crash between the two renames is the state that needs a rule**, and
it has one, because each file's header records the geometry it was built
for. A crash there leaves the pair at *different* geometries — one
rebuilt, one not — which open detects by comparing the two recorded
capacities rather than by trusting a temp to still exist. The reconciling
rule is **complete, never roll back**: open rebuilds whichever file is at
the smaller geometry, from its own contents, at the larger one, then
renames and fsyncs as the first rebuild did; the pair is then consistent
and the configured capacity is in force. Completion is always safe
because **a resize never renumbers**: it copies each dictionary record
into the wider stride at the same index, so slot ids are identical before
and after and the half-rebuilt pair still agrees on every id it names.
That is also why ids are never reassigned at all in this design —
reclaiming a tombstoned id would mean renumbering, which cannot be made
crash-safe across two files without a third witness, so a tombstone holds
its id for the life of the root and an operator who exhausts the space
raises `max_tenants` or starts a fresh root. A leftover `.new` from
either file is truncated by the next rebuild and swept. RFC0053.4
asserts the mixed-geometry restart: a pair crashed between renames comes
up at the larger geometry with every id unchanged. This is the one
supported procedure; the file still never grows *in place*, which is what
the two-slot protocol depends on. **And `max_tenants` has a format ceiling**, because a
slot id is a `u16`: `validate_config` rejects `max_tenants > 65_536` with
that reason. The bound is 65,536 rather than 65,535 because **slot id 0 is
usable** — a position is marked unused by its dictionary record's `len =
0`, not by a reserved id — so every id in `0..=65_535` names a real
position. **The recovered tenant set is validated too, not just the
recorded count**: recovery seeds `Held` from every tenant restored from
snapshots and replay, which can exceed a lowered `max_tenants` while
`entry_count` alone passes — a sidecar records only the tenants that have
a frontier, and a root can hold more tenants than that. So before the
coordinator is constructed, open counts the tenants recovery restored and
**refuses to start** when that count exceeds the configured cap, naming
the recovered count and the cap, on the same fail-closed posture as every
other geometry mismatch; the operator raises the knob back or starts a
fresh root. **Lowering is checked against the id space, not the live count.** A
tombstoned id persists, and ids are never reused, so `entry_count` — live
entries only — can sit far below the highest id in use: a root whose only
assigned slot is id 900 has an `entry_count` of one and would pass a cap
of two, then address entry 900 in a file sized for two. So the check is
against **the highest assigned or tombstoned slot plus one, taken across
both files** — which is exactly `next_slot_id`, the high-water mark each
slot header carries — and a configured `max_tenants` below that is
refused at `Wal::open`, naming both numbers. Lowering the knob below the recorded `entry_count` is
**refused at `Wal::open`** too, as the same error, naming both numbers, since the marks of the
tenants that no longer fit cannot be dropped without losing their
frontiers; raising it needs a **new file, written at open before the first
pass**, carrying the old entries into the larger geometry — never an
extension of the live file, whose second slot would move under a crash — which also makes the settlement write below cheap, one in-place
write and an fsync rather than a file creation. The file is outside the byte bound like the other sidecars, and that
exclusion has a stated limit: preallocation can fail. A volume full at
open, or full when tenant growth needs the file grown, blocks the write —
and with it reclamation, since RFC 0052's pass writes its record before it
unlinks. So the byte limit promises bounded *growth*, not recovery from a
full volume: with no room for a sidecar write, a crossed bound stays
crossed and the refusal stands until an operator frees space, which §3.3
makes visible through the floor and the unreclaimed figure rather than
leaving it as an unexplained stall. **Settlement needs to write it without
a checkpoint**, so the surface gains one more call, another **amendment to
RFC 0052 §3.7's list**: `fn publish_marks(&mut self, published:
&HashMap<TenantId, PublishedMarks>) -> Result<(), ReclaimError>`, which
writes and fsyncs the sidecar alone and advances no checkpoint. Its
crash-ordering contract is what the marks need and nothing more: the write
is durable before it returns, the marks are monotonic per tenant, and a
crash mid-write leaves the previous slot readable, so a start reads either
the old marks or the new and never a mixture. The migration start below
uses the same call. **Audit has its own watermark in the same record**, because the record one
does not cover it: an audit group can be durable in the audit store while
the record publish of the same batch panics, and a crash before the next
checkpoint would replay the frame and regenerate an event the store
already holds. **Both marks are frame offsets**, which is what RFC 0052's replay model
requires: audit replay there is **regeneration-only** — the miner
regenerates a frame's events as it re-mines it, and stored `AuditEvent`
frames are never a source — so an event's identity comes from the frame
that produced it, and a gate expressed in audit-stream positions would
have nothing to compare a regenerated event against. RFC 0052 §3.1's
`audit_durable_through` is the sink's own in-flight cursor over *events*,
so the coordinator maps it to frames rather than storing it: emission
records the source frame beside each event in the same per-frame ledger
the records use (in-process only — no change to the audit row schema), and
the audit mark advances to a frame when **every** event that frame
produced is covered by `audit_durable_through`. A partially settled group
therefore advances nothing, exactly as a partially settled frame does on
the record side. So the audit sink's settlement raises a per-tenant
`audit` offset the way the record sink raises `records` — highest fully
settled frame, monotonic, under the sink lock, advanced only from
`audit_durable_through`, never from an emptied buffer, since a concurrent
drain can hold exactly those events in an unfinished write — both are
written in the one `PUBLISHED` write, and **audit replay is gated the same
per-tenant way the record side is**: a tenant with a `PUBLISHED` entry is
gated on `max(S, audit watermark)` from its own entry — the entry
governing, never maxed against `X` — and only a tenant without one, or a
frontier whose validity bit is clear, falls back to the node-wide `X`. The node-wide form alone
would carry the defect §3.2 fixed for records one round earlier — once
`X` advances for everyone else, a lagging or unrecoverable tenant's
regeneration would be suppressed for events that were never written, and
a template event lost that way is `CLAUDE.md` §3.1's silent merge. The
record gate is `max(S, records watermark)` per tenant on the same rule. The §3.2 claim that a settled audit group is never requeued
therefore holds across a restart too, not only in-process. **A present entry is authoritative for its tenant, and is not maxed
against the node-wide horizon.** Taking `max(X, PUBLISHED)` would let a
checkpoint advanced for everyone else override a lagging tenant's stale
entry — precisely the suppression this sidecar exists to prevent, and
reachable by an ordinary crash, since `CHECKPOINT` is written first and a
power loss before the `PUBLISHED` write leaves exactly that state. The
rule is therefore the other way: **where the entry is present and its
validity bit set, it governs**, and the node-wide horizon applies only to
a tenant with no entry, or per frontier to a side whose validity bit is
clear. The direction is the safe one — a stale entry replays frames that
may already be published, the at-most-twice case the rest of §3.2 bounds,
where an over-advanced horizon would suppress frames that never published,
which is loss. Staleness is legible rather than guessed at: each entry
carries a **`u32 generation`**, set to the slot's generation at the write
that last changed that entry's marks, so recovery can tell an entry never
written (generation zero, validity bits clear) from one written and
unchanged since, and can report how far behind a tenant's frontier is
instead of silently papering over it. With that, recovery seeds both
in-memory watermarks per tenant from the entry where it governs, and from
what the Parquet-side suppression horizon `X` implies only where it does
not — the read
sitting behind RFC 0052's startup fsync of the WAL root, which precedes
any sidecar read, so a sidecar whose rename or creation was not durable
is not read as present — and a missing
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
seeds from `X` again, correctly. **A crash after the write but before the
flag is confirmed** — armed *and* present — is the row the state machine
owed and now states: the sidecar is authoritative, the seed has already
happened and is durable, so recovery **confirms the flag** and proceeds
from the written marks rather than re-seeding from `X`, which would
discard a finer horizon the file already holds. A crash after the confirm
is an ordinary start. A
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
`PUBLISHED` present with no version-2 `CHECKPOINT` is read against the
current write order, **checkpoint first**, which changes what it can mean.
Under that order the sidecar is never written ahead of the checkpoint by
`Journal::checkpoint`, so a present sidecar beside no version-2 checkpoint
has exactly two sources: a **`publish_marks` write**, which §3.2's
settlement and the accepting start both make without touching the
checkpoint, or a **deleted checkpoint**. The first is ordinary and the
second is a fault, and `checkpoint_seen` is what separates them: with the
flag **set** the root has checkpointed and lost it, so open fails closed
naming both files; with it **unset** no checkpoint has ever succeeded
there, so the sidecar can only have come from a `publish_marks` write on a
root that has not yet checkpointed, and it is kept and seeded from rather
than discarded — an earlier draft discarded it, which was right only under
the superseded sidecar-first order, where such a file was always a torn
first checkpoint. It reads against `seen`, never
`armed`, which is what keeps RFC 0052 §3.2's two non-fault `armed` rows
non-faults here as well: `armed` without `seen` beside a **version-1**
`CHECKPOINT` is that RFC's migration retry state — nothing can have been
reclaimed, since housekeeping is a no-op until the witness exists — and
`armed` without `seen` with `CHECKPOINT` **absent** is its crash window
before the rename. A `PUBLISHED` written moments earlier in that same
first `checkpoint` call is what either looks like **on a root with no
segments** — genuinely fresh, nothing published under a horizon anything
reads — so there the record is discarded and the next attempt rewrites it,
as when neither flag is set. **With segments present it is likewise retained**:
RFC 0052 §3.2 reads armed-and-unseen beside segments as a retained
migration state, so the marks are *kept* and seeded from — discarding them
would re-open the coarse window on a root that already has frames to
replay. Under checkpoint-first ordering the two readings agree, which is
why this matrix has one rule rather than a fresh-root exception:
`checkpoint_seen` decides the fault, and everything short of it keeps the
marks. Only `seen` makes its absence a fault. One
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
to it rather than a difference between the two documents. The bound between checkpoints is then stated honestly, and made a bound
rather than a per-crash cost. A panic costs at most one duplicate per
record in the ambiguous span. A crash before the watermark is durable
costs at most **one more** for the records published since the last
durable watermark — but "one more per crash" is not a bound if the window
stays open, so **the settlement forces the write**: after a rebuild
settles, the coordinator writes `PUBLISHED` with the raised marks on the
same durable path a checkpoint uses, before it removes the entry. Each
window therefore closes with a durable frontier at the moment the
ambiguity is resolved, not at whatever checkpoint happens next, and
repeated crashes cannot accumulate copies of the same records: each
replays only what lies above the last written frontier. **The write is in two steps, because a crash between the publish and the
write would otherwise lose the frontier.** A rebuild that has already
handed its batch to the store and then dies before `publish_marks` leaves
nothing on disk saying so, and the next start replays the same span again
— and again on the next crash, with no ceiling. So the marks are written
**before** the publish as an *intent* and confirmed after it: the entry
format's `u16 flags` carries a `publishing` bit, `publish_marks` writes
the intended marks with that bit set, the rebuild publishes, and a second
`publish_marks` clears the bit. Recovery reads a set bit as "this span was
attempted": it seeds the watermark from the **previous** durable marks, so
nothing that may not have landed is ever suppressed, and it re-publishes
**only the intent span**, which is fixed and recorded rather than growing
with each attempt. Repeated crashes replay the same bounded span, and **the copies are
bounded too, because the republish is idempotent**: a normal publish names
its object with a fresh `UUIDv7`, so a replay would add a copy each time,
but the intent span's republish derives its key deterministically instead
— the same derivation §3.2 makes normative below — so every attempt at
the same span writes the *same* object name. Each partition of the span
has its own such key, so a first attempt that published some partitions
and died reuses those names rather than inventing new ones. The claim is
the **keys-not-objects** one §3.2 states: repeated crashes produce **one
distinct key per partition of the span**, however many times they replay
it, and never a different set of rows under a name already used — whether
the store leaves one object or one per attempt is the backend's business,
which this RFC does not require. The first confirm closes the window, and
the guarantee is claimed only for the span under an intent, since a
publish outside one keeps its `UUIDv7`. **A failed write is a fail-closed park**: the entry **stays**,
the tenant is refused under the server-terminal, client-retryable class
until the write succeeds, and the coordinator retries the same marks,
which is idempotent because they are already recorded and `publish_marks`
is monotonic per tenant, so a retry writes the same frontier and a crash
in between leaves the previous slot readable. RFC0053.2 covers both the
intent-set restart and the failed write. The write is one
sidecar write per settlement, which is a rare path by construction — it
follows a panic — and the checkpoint's own write is unchanged. It follows that the salvage in `ingest_mined` is kept as
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
rather than silently.

**The replay is authoritative, and the tree it replaces takes its
in-memory products with it.** `replace_tenant` discards the tree the
panic left, but records mined from that tree can still be sitting in the
sink — forwarded by `ingest_mined`'s salvage, or requeued by an unwind —
and they carry that tree's `template_id`s while the replay allocates
fresh ones from the never-rewound allocator. Publishing them later would
put a row in Parquet whose template has no audit history to fold (RFC
0017's versioned rendering), or a stale row beside the regenerated one
for the same line. So the settlement has a handoff, stated in order:
**before `replace_tenant` runs**, it quiesces the publisher for that
tenant — waiting on the partitions already detached for it, which settle
durable or requeue into the buffers — and then, **under the sink lock and
in the same section as the tree swap**, drops every buffered record for
that tenant from both sinks, including anything the quiesce just
requeued. Only then is the tree replaced and the entry removed. Nothing
is un-published, and nothing needs to be: a partition that settled
*durable* is at or below the tenant's publication frontier by
construction — publication settlement requires its audit events first —
which is exactly the range the rebuild suppresses re-emission for. So
after a rebuild the replay is the single source for everything above that
frontier, and the discarded tree contributes nothing. RFC0053.2 asserts
it: a salvaged record buffered before the rebuild is not published under
its old `template_id`, and the regenerated row appears exactly once. RFC0053.2 covers a swallowed submit followed by a
successful append, a mining panic followed by a successful append, a clean
barrier taken while an entry is unresolved, a panic after `ingest_mined`
returned, and a panic between a widening and its audit event.

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
| `ourios.wal.segments.usage` | gauge (int) | `{segment}` | `ourios.wal.segment.state` ∈ {`retained`, `free`, `over_cap`} — `retained + free = limit + over_cap`, with `free` saturating at zero and `over_cap` carrying the overrun an owed rotation may create |
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

**The decision is not open, though — it is unimplemented.** RFC 0014 §3.4
is accepted and already specifies exactly this: the sink tracks total
buffered bytes against a *hard* ceiling, force-flushes the largest or
oldest partitions under soft pressure, and when early flush cannot keep up
`emit` **blocks** until an in-flight flush frees memory, so the buffer can
never exceed the ceiling. What the code does today — retaining past the
ceiling when a flush fails — is a gap against that contract, not a gap in
the design. So this RFC does not decide block-versus-spill-versus-drop,
because RFC 0014 §3.4 decided it; it names that section as the governing
contract and its implementation as a **prerequisite** for the end-to-end
bounded-ingest claim. What this RFC states instead is the scope of
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
>   the rotation owed after a seal proceeds whether or not a slot is free,
>   leaving at most a one-segment overrun the gauge reports as `over_cap`
>   and the next pass reclaims — no rotation-failure state is entered from
>   the ceiling, and admission continues throughout
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
> - **And** `max_segments < 1` is rejected at config validation, naming the
>   reason: a ceiling of zero admits no retained closed segment, so the
>   first closed segment refuses every later discretionary rotation
> - **And** the segment gauge stays consistent across the overrun an owed
>   rotation creates: `free` saturates at zero, `over_cap` carries the
>   excess, and `retained + free = limit + over_cap` holds while the
>   rotation is outstanding and after the next pass reclaims
> - **And** an owed rotation proceeds at the cap: with every older segment
>   pinned or ineligible and the sealed current segment's own frames
>   checkpoint-covered — the state no pass can relieve — the rotation still
>   creates its segment even though closing the current one takes
>   `closed_retained` past the ceiling, the WAL keeps accepting, and only
>   *discretionary* rotations are refused while the pins hold
> - **And** no rotation-failure state is entered from the ceiling at all:
>   `Terminal` is reached only by an exhausted rotation retry budget, and
>   no `Deferred` state exists
> - **And** the kind travels in the call: `rotate(Discretionary)` at the
>   ceiling returns `RefusedAtSegmentCap` and creates nothing, while
>   `rotate(Owed)` at the same count rotates — the seal path and the
>   post-recovery discharge both passing `Owed`
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
> - **And** the rebuild drops the discarded tree's buffered records: the
>   publisher is quiesced for that tenant, its detached partitions settle
>   or requeue, every buffered record for it is dropped under the sink
>   lock in the same section as `replace_tenant`, and no row is published
>   under a `template_id` the replay did not allocate
> - **And** a panic raised inside `ingest` after a leaf was widened and
>   before its audit event was emitted is settled by a rebuild from the
>   tenant's last installed snapshot: the widening is re-derived with its
>   event, the version's audit history is complete, the records at or
>   below the tenant's publication watermark — and, in the entry's own
>   frame, below `emit_from` — are present exactly once, the gate being
>   that tenant's own `max(S, watermark)` for records and for audit alike
>   and never the node-wide `X`, so a tenant whose frontier lags has its
>   events regenerated rather than suppressed — a crash after `CHECKPOINT`
>   and before the `PUBLISHED` write leaving a stale entry that still
>   governs, its generation saying how far behind it is, while a cleared
>   validity bit falls back to `X` for that frontier alone — and those in the
>   ambiguous span are present at most twice, never absent; a frame with
>   one record durable and a later one lost does not advance the
>   watermark; a crash before the next checkpoint after a retried panic
>   yields at most one more copy of the records published since the last
>   durable watermark, and none once the settlement's forced `PUBLISHED`
>   write has landed — repeated crashes replay only above the last written
>   frontier and add no further copies; the
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
> - **And** the frame codec refuses a tenant longer than 128 bytes on
>   encode and on decode, and a fixture root whose replay yields one fails
>   closed at open naming the frame's offset and the length it carried
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
- [ ] Where RFC 0026's binding-denial events go when the audit store
      rejects writes permanently. §3.2's three-way outcome and its terminal
      rule cover the record-dependent stream only, because a denial is
      emitted before any frame exists and replay cannot regenerate it — so
      a permanent drop there is an open gap against RFC 0005 §7. It needs
      either a durable path of its own or an explicit exemption in that
      contract; either is RFC 0026's to settle, not this RFC's.
- [ ] Confirm RFC 0014 §3.4's hard ceiling is implemented — the blocking
      `emit` that makes buffered bytes never exceed the ceiling. It is not a
      design question: §3.4 is accepted and decided it, and today's sinks
      retain past their ceilings when a flush fails, which is a gap against
      that contract. §4 states that §3.1's disk bound is only the operative
      limit once memory growth is bounded by §3.4, and §6 makes it a gate on
      `validated`, so it must land before this RFC can be more than `green`.
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
- RFC 0046 §3.1 (tenant selector) and criterion RFC0046.11 — **amended by
  §3.2 of this RFC** from a 256-byte replay bound to **128**, the grammar
  RFC 0048 §3.1 pinned and which RFC 0046's own resolved-questions note
  records as "the one tenant grammar every boundary applies at". The
  amendment is a consistency fix rather than a new decision: the replay
  clause and RFC0046.11 were left at the superseded number, and the frame
  codec was left with them.
- RFC 0005 §7 (audit files and their durability clause) — **amended by §3.2
  of this RFC**: the audit sink's permanent-failure path reports a third
  outcome rather than success, `write_ordered` refuses the dependent record
  publish on it, and the tenant becomes terminal until a **restart** clears
  it — restart alone, per §3.2, because the dropped events are out of the
  sink's buffer and only recovery's re-mining regenerates them. Without the amendment a dropped event is reported as
  fully durable and its records publish anyway, which breaks both that
  clause and `CLAUDE.md` §3.1.
- RFC 0014 §3.4 (sink memory ceiling) — the governing contract for the
  memory half of the bound; §4 and §6 make its implementation a
  prerequisite for this RFC's end-to-end claim.
- RFC 0025 §3.3 (permanent-encode quarantine) — the disposition for a
  poisoned *data record*; §3.2 states why it cannot serve an audit-write
  failure, and §3.2's watermark rule states how a quarantined record is
  covered.
- RFC 0014 — the record sink and its flush triggers.
- `CLAUDE.md` §3.4 (WAL-before-ack), §6.3 (observability of ourselves).
- `docs/hazards.md` #3 (WAL durability versus latency), #4 (small files).
