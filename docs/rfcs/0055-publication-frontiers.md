---
rfc: 0055
title: Publication frontiers and tenant settlement
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-09-15
supersedes: —
superseded-by: —
---

# RFC 0055 — Publication frontiers and tenant settlement

> **Status note.** `drafted`. Split out of RFC 0053. The reviewed prose
> is the non-ownership half of §3.2 on
> [`#802`](https://github.com/jensholdgaard/ourios/pull/802) @ `30a21f80`
> (`PUBLISHED`, frontiers, settlement, tenant slots, object-key intent,
> sidecar geometry). Extract. Do not grow 0053.

## 1. Summary

Unwind safety (RFC 0054) is an in-process property. Across a restart,
recovery must know which record and audit frames already reached the
store, and a mining panic must not leave a tenant tree between a
widening and its audit event.

This RFC specifies:

- a durable per-tenant publication frontier (`PUBLISHED`)
- settlement that rebuilds a panicked tenant from its last installed
  snapshot
- tenant-slot admission (`max_tenants`) as the open-mode backstop

It does not specify the WAL byte bound (RFC 0053) or the unwind
invariant (RFC 0054).

## 2. Motivation

RFC 0054's "one extra object per panic" claim does not survive a
pre-checkpoint crash: recovery replays frames and can regenerate a
durable audit event. Frontiers are the missing watermark. Settlement
is hazard #5 (template-state recovery) for the in-process panic.

## 3. Proposed design

Extract from the 0053 draft, then stop:

- `PUBLISHED` sidecar: per-tenant `{records, audit}` offsets, one
  durable write, geometry sized from `max_tenants`, never grown in
  place; raise-capacity rebuilds both sidecars through `.new` temps.
- Record replay gated on the record watermark; audit replay gated on
  `max(checkpoint, audit watermark)`.
- Settlement: rebuild from last installed snapshot; replay frames;
  discarded tree's buffered records dropped under the sink lock in the
  same section as the swap.
- Tenant slots: `Reserved` / `Held`; count `Held + Reserved` against
  `max_tenants`; seed `Held` from recovered tenants before admit.
- Object-key intent as specified in the last commits on #802
  (normative in-process; cross-restart claim already dropped).
- Tombstone a removed tenant's dictionary slot.

**Amendments this RFC makes: none.** The 128-byte tenant key its
dictionary is sized to rests on the RFC 0046 / RFC0046.11 length
amendment (256→128, matching RFC 0048 §3.1, already recorded as a
resolved question in 0046) — and that amendment is **RFC 0052's**, stated
normatively in its §8. This RFC **cites** it and must not restate it: one
clause amended by two documents is the contradiction this split exists to
prevent. §9.7's extracted prose predates the split and reads as though
this RFC makes it; it does not.

## 4. Alternatives considered

- Rely on recovery's WAL suppression horizon alone. Rejected: it does
  not see in-process duplicates or durable-but-unmarked audit objects.
- Keep this in RFC 0053. Rejected: it is why 0053 cannot converge.

## 5. Acceptance criteria

Write `Given / When / Then` when extracting. Minimum:

- RFC0055.1 restart does not republish a settled audit group
- RFC0055.2 mixed-geometry sidecar restart comes up at the larger
  geometry with ids unchanged
- RFC0055.3 settlement rebuild does not publish under a `template_id`
  the replay did not allocate
- RFC0055.4 `max_tenants` holds across unwind and restart

## 6. Testing strategy

Sidecar fixtures (including half-resized pair and tombstones) plus a
settlement race with two triggers. Details in the quarry draft §6.

## 7. Open questions

- Idempotent object keys across restart (explicitly dropped on #802;
  reopen here or leave closed).
- Whether settlement blocks the global ingest gate or needs a
  per-tenant gate (RFC 0052). Pick one; do not claim both.

## 8. References

- RFC 0005, 0048, 0053, 0054
- RFC 0046 — **not amended here.** The RFC0046.11 length amendment
  (256→128) is RFC 0052's, in its §8; this RFC's 128-byte key cites it.
- RFC 0052 — owns that amendment, the `RECLAIM` record this RFC's
  `PUBLISHED` shares rules with, and the tenant id space.
- Source quarry: #802 @ `30a21f80`

## 9. Extracted wording (RFC 0053 draft, `30a21f80`)

Moved here unedited from the RFC 0053 quarry so the reviewed sentences
survive the split rather than being rewritten from memory. §3 above is
the shape of this RFC and §5 above governs acceptance; the section
numbers inside these paragraphs ("§3.1", "§3.2") and the `RFC0053.n`
ids are the draft's own.

### 9.1 The tenant-slot guard (draft §3.1)

A second knob rides with `unreclaimed_bytes_limit` (RFC 0053 §3.1), for a different growth: in open mode (RFC 0026
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

### 9.2 The deterministic key and the publication frontier

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
the third put would duplicate the two objects already accepted.

### 9.3 An ambiguous requeue keeps its own identity

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

### 9.4 `unmined` entries, settlement, `PUBLISHED`, lock order

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

### 9.5 Draft acceptance legs (source for §5)

> - **And** in open mode a batch for a tenant id the miner does not hold,
>   with `max_tenants` held, is refused naming the
>   tenant, as `TenantCapacity` — `503` / `UNAVAILABLE` with a protobuf
>   `Status` body and no `Retry-After` and no `RetryInfo` — while a batch
>   for a held tenant is admitted; the refusal is counted with `error.type
>   = wal_tenant_cap`, sets no refusal latch, and
>   `ourios.wal.tenants.usage` over its states sums to
>   `ourios.wal.tenants.limit` with `free` at zero; two concurrent first writes for one new id share a slot and
>   a burst of new ids never overshoots the guard
> - **And** when the frame that crossed the bound is the one whose mining
>   panicked, the barrier task settles its entry on the next tick with no
>   append admitted, the checkpoint then advances past it, and the bound
>   clears by the timer alone — no restart
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
> - **And** a first write that appends, panics in mining and unwinds keeps
>   its tenant reservation: another new id cannot take that slot, and the
>   entry's settlement converts the reservation to `Held` when it installs
>   the tenant
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
> - **And** the frame codec refuses a tenant longer than 128 bytes on
>   encode and on decode, and a fixture root whose replay yields one fails
>   closed at open naming the frame's offset and the length it carried
> - **And** a restart seeds the tenant admission table from the tenants
>   recovery restored, before any listener is constructed: a held tenant
>   is admitted at once and a new id at the guard is refused, with no
>   first-write race window at startup

### 9.6 Draft open questions (source for §7)

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

### 9.7 The RFC 0046 amendment (draft §8)

- RFC 0046 §3.1 (tenant selector) and criterion RFC0046.11 — **amended by
  §3.2 of this RFC** from a 256-byte replay bound to **128**, the grammar
  RFC 0048 §3.1 pinned and which RFC 0046's own resolved-questions note
  records as "the one tenant grammar every boundary applies at". The
  amendment is a consistency fix rather than a new decision: the replay
  clause and RFC0046.11 were left at the superseded number, and the frame
  codec was left with them.
