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

So the checkpoint advances there and nowhere else:

```text
if flush_then_snapshot(..., high_water) {
    wal.checkpoint(high_water)          // §6.7, monotonic
}
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
The caller is the receiver role, on its own interval, and it passes the
latest durable snapshot's high-water mark as `retain_floor`:

```text
every housekeeping_secs:
    wal.housekeeping(latest_durable_snapshot_high_water)
```

The floor is load-bearing and is the reason §3.1 can tolerate a failed
snapshot write. `housekeeping` truncates below `min(checkpoint, floor)`,
so a stale floor makes truncation conservative — it retains frames a
snapshot has not captured, which degrades the next start to a fuller
replay and never to loss (hazard #5's retain rule, RFC 0001 §6.9).
`None` is passed only where no snapshot consumer exists.

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
were lost fails the next `Wal::open`'s header read, turning a benign
crash into `OpenError::Corrupt`. The retry therefore unlinks the orphan
*before* re-attempting, and treats a failed unlink as a failed retry
rather than proceeding.

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

### 3.4 Backpressure becomes explicit

Today the only limit on local accumulation is the volume. That is an
implicit limit with an undefined failure mode, which is what turned an
outage into a wedge. The WAL gains a declared local bound — bytes
retained below the checkpoint, and the age of the oldest unreclaimed
frame — and crossing it is a *stated* rejection: `IngestFailure` already
distinguishes a transient unavailability from a wedge (#794), and
backpressure becomes a third outcome with its own reason text and a
`Retry-After` derived from the limit rather than a fixed second.

The bound is configuration with a conservative default, and the
rejection is the contract: ingest keeps accepting while the object store
is unreachable until the declared limit, then refuses with a reason that
names the limit it hit. Crucially, crossing the limit does **not** set
the quiesce latch — it is a pressure state that clears itself the moment
the checkpoint advances.

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

With that settled, the age sweep can survive a panic and keep sweeping
(#795 deliberately stops, because without this it would repeat the loss
every tick).

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
unreclaimed WAL bytes — and would stall ingest on a condition that does
not threaten durability. The memory-growth problem is real and is
tracked separately; it is not the backpressure signal.

## 5. Acceptance criteria

> **Scenario RFC0052.1 — The checkpoint advances only behind a proven
> publication barrier**
> - **Given** a WAL with acknowledged frames and a record sink whose
>   store is healthy
> - **When** the publication barrier completes with both sinks fully
>   drained
> - **Then** `Wal::checkpoint` is advanced to the barrier's high-water
>   mark
> - **And** when either sink retains anything, no checkpoint is
>   attempted and `last_checkpoint()` is unchanged

> **Scenario RFC0052.2 — Segments are reclaimed, and never past the
> snapshot floor**
> - **Given** a checkpoint advanced past several closed segments and a
>   durable snapshot whose high-water mark is *below* that checkpoint
> - **When** housekeeping runs
> - **Then** only segments wholly below `min(checkpoint, floor)` are
>   unlinked, the current append segment survives, and the WAL's
>   segment count falls
> - **And** a frame above the snapshot floor is still present after the
>   pass, so a restart re-mines it rather than losing it

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
> - **And** no orphaned segment remains that a subsequent `Wal::open`
>   would read as corrupt

> **Scenario RFC0052.5 — A persistent rotation failure gives up
> distinguishably, and never acks**
> - **Given** a WAL whose rotation fails on every attempt
> - **When** appends continue past the bounded retry count
> - **Then** every append is refused, no batch is acknowledged, and the
>   refusal is reported as the terminal state rather than as a transient
>   one
> - **And** the first underlying I/O error is still recoverable from the
>   reported state, not replaced by a generic "quiesced" message

> **Scenario RFC0052.6 — Backpressure is a stated limit, and clears
> itself**
> - **Given** an unreachable object store and a configured local
>   retention bound
> - **When** ingest continues until the bound is crossed
> - **Then** earlier batches were accepted and acked, and the rejecting
>   batch is refused with a reason naming the limit and a `Retry-After`
>   derived from it
> - **And** the rejection does **not** set the rotation-failure state
> - **And** when the store returns and the checkpoint advances, ingest
>   resumes with no restart

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
- **Bounded growth (RFC0052.3)** — the `ourios-bench` soak harness on
  its synthetic clock, asserting the WAL's byte total and segment count
  stay bounded over a long run. This is the one criterion a unit test
  cannot express, because the defect is the *absence* of a periodic
  call; only elapsed cadence reveals it.
- **Rotation retry (RFC0052.4, RFC0052.5)** — fault injection at each of
  the four rotation steps, once-failing and always-failing, asserting
  recovery in the first case and a distinguishable terminal refusal in
  the second, plus `Wal::open` succeeding afterwards in both (the
  orphan-unlink property). A `proptest` over which step fails and how
  many times keeps the four sites from being tested only one way.
- **Backpressure (RFC0052.6)** — an integration test with an
  unreachable store asserting the accept-then-refuse-then-resume
  sequence, the reason text, the `Retry-After`, and that the
  rotation-failure state was never entered. The resume leg is the
  regression test for #791 itself.
- **Telemetry (RFC0052.7)** — the in-memory metric exporter pattern
  already used for the ingest/sink instruments, asserting every name is
  in the exported stream; plus a `weaver registry live-check` pass over
  the new log events, since an event the tests never emit is an event
  the live-check never checks (that is how #795's un-named event passed
  CI).
- **Unwind safety (RFC0052.8, RFC0052.9)** — a publish double that
  panics on demand, asserting the buffers are repopulated, the barrier
  refuses to stamp, and a later healthy barrier publishes. RFC0052.9
  drives many consecutive panicking ticks and asserts the record count
  is conserved.
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
