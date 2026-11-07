---
rfc: 0068
title: WAL recovery from a zero-filled tail
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-11-04
supersedes: —
superseded-by: —
---

# RFC 0068 — WAL recovery from a zero-filled tail

> **Status: `drafted`.** Tracks #1165. This RFC **amends RFC 0008**
> (`accepted`). It amends the three-bucket rule of §5 RFC0008.2 and
> §8's testing text for it, and §5 RFC0008.4 and RFC0008.5, and §6.6
> steps 3 and 4. RFC 0008 gets a dated pointer here once this RFC is
> accepted. RFC 0065 (`red`) inherits the change, because its
> per-group WALs are RFC 0008 segments.

## 1. Summary

On the **newest** segment only, recovery treats a run of zero bytes
that starts at the last valid frame boundary and reaches end of file as
a torn write. It heals that run the way RFC0008.4 heals a partial
frame: `ftruncate(2)` to that boundary, fsync the segment, fsync the
directory. A crash-recovery bucket is added, and none is changed:
every complete frame whose CRC does not match stays RFC0008.5
corruption, any other damage stays corruption, and so does any damage
on a closed segment.

## 2. Motivation

AGENTS.md §3.4 requires that an ingester killed mid-batch recover
without losing acknowledged data. RFC 0008 meets the *without losing*
half, and RFC0008.4 heals a frame cut short at EOF. It does not heal the
other common shape a crash leaves. On ext4 with delayed allocation, and
on other filesystems that journal metadata before data, a power loss
during an append that had not yet been fsynced can leave the file
**longer than its data**: the size reached the journal, but the blocks
behind it were never written, so they read back as zeros. Today's
`replay_segment` reads 12 zero bytes as a header with `len = 0` and
`kind = 0x00`, and stops with `RecoveryError::Corrupt { UnknownKind }`.
`Wal::replay` then returns that error, the receiver does not start, and
the ingester stays down until an operator
intervenes (#1165).

Those bytes belong to an append whose `sync` never returned, so no
client was ever acknowledged for them. Refusing to start loses nothing,
but it turns an ordinary power loss into an outage. That is the H3
trade-off between durability and availability, and here it falls on
the wrong side for data that was never promised. RFC 0065's replicated
backend makes this more pressing: each replica runs one RFC 0008 WAL
per group, so a power loss across a rack could wedge many groups at
once. #1162's `votes.log` already heals the same shape and stays
fail-closed on any non-zero damage.

## 3. Proposed design

### 3.1 The zero-tail rule

Recovery scans the **newest** segment exactly as RFC 0008 §6.6 step 3
does, up to the first offset `B` where the bytes do not read as a
complete, valid frame. `B` is then the last valid frame boundary. Let
`E` be the segment's length. Recovery treats `[B, E)` as a **torn
tail**, the shapes RFC0008.4 heals, when either of these holds:

- **T1, a partial frame (today's RFC0008.4).** Between 1 and 11
  bytes remain at `B` (`0 < E - B < 12`), or the header at `B` is well-formed and its declared
  payload runs past `E`.
- **T2, a zero tail (new).** `E - B ≥ 12` and every byte in `[B, E)` is
  zero.

T2 is tested first. A zero header is otherwise read as T1's "header
whose payload runs past `E`" only by accident, and it fails the
well-formed check anyway, since kind `0x00` is unknown.

When `E = B` the tail is clean and nothing is healed.

A torn tail is healed by RFC 0008 §6.6 step 4, unchanged:
`ftruncate(2)` to `B`, `fdatasync` the segment, then `fsync` the parent
directory. The next append lands at `B`.

### 3.2 What stays corruption

Everything else at or after `B` on the newest segment remains RFC0008.5
corruption, on the all-segments-stop path, exactly as today:

- a complete frame whose CRC32-C does not match, whatever bytes it
  holds, zero or not. So RFC0008.2's "a CRC mismatch is corruption, not
  truncation" still holds without exception;
- an unknown kind, a non-zero pad, or an oversize `len`, unless every
  byte from `B` to `E` is zero (T2);
- a tail of 12 bytes or more holding a zero run that does not reach
  EOF, for example zeros followed by any non-zero byte. A shorter tail
  is T1 whatever its bytes;
- any torn or zero tail on a **closed** segment.

### 3.3 Operator visibility

A healed torn tail is a recovery event, never a silent one. Recovery
logs one `WARN` event, `ourios.wal.recovery.tail.truncated`, per healed
segment, and adds `E - B` to the counter
`ourios.wal.recovery.truncated.size` (unit `By`), recorded with the
same `ourios.wal.recovery.tail.shape` as the event. Both names are in the
shared `ourios-semconv` registry from v0.3.25. The event carries:

- `ourios.wal.recovery.tail.shape`: `partial_frame` (T1) or
  `zero_tail` (T2);
- `ourios.wal.recovery.tail.offset`: `B`, the segment's length after
  the cut;
- `file.path`: the segment, whose file name carries its UUIDv7;
- `file.size`: `E`, its length before the cut.

The event covers today's T1 heal too, so one signal reports both
shapes. No audit event is emitted, consistent with the RFC0008.5
audit event being deferred (RFC 0008 §5, 2026-06-13 amendment).

### 3.4 Why this cannot drop an acknowledged frame

A frame is acknowledged only after a `Wal::sync` covering it has
returned (RFC 0008 §6.3). By then its bytes are on stable storage, and
delayed allocation cannot later turn them into zeros, because it zeroes
only blocks that were never written.

Frames are packed, though, so an acknowledged frame can share a sector
with a later, unsynced append. Writing that append back rewrites the
shared sector. The argument therefore rests on a **storage guarantee**:
a sector write interrupted by power loss leaves that sector holding its
old contents or its new contents, never zeros or a mix. This is the
sector-atomicity guarantee that SCSI and SATA drives give, and that
NVMe states as its power-fail atomic write unit (AWUPF, at least one
logical block). It is also what RFC 0008 already relies on: without it,
a torn shared sector would damage an acknowledged frame today.
RFC 0008 would then fail closed with a CRC mismatch, whereas T2 would
cut the frame if the torn sector read back as zeros.

The storage Ourios targets documents the guarantee. AWS EBS prevents
torn writes for aligned 4, 8 and 16 KiB writes on Nitro instances. GCP
Hyperdisk writes 128 KB units atomically within 1 MB boundaries. NVMe
encodes AWUPF from zero, so every drive declares at least one block.
Consumer drives without power-loss protection give no such promise,
and on them RFC 0008 is not safe either, since they can lose data that
was already fsynced. The operations docs therefore state sector-write
atomicity as a deployment requirement, beside the existing
local-filesystem requirement.

Under that guarantee, a zero run a crash leaves lies wholly after the
last synced offset. T2 only removes bytes at or after `B`, the end of
the last complete, valid frame, so every acknowledged frame ends at or
before `B` and is replayed.

The residual risk is narrower than a damaged frame: a **device fault**
that zeroes every byte from the start of an acknowledged frame through
EOF on the newest segment would be healed instead of reported. A fault
that zeroes only part of a frame leaves a complete frame with a CRC
mismatch, or a non-zero byte inside the run, and both stay corruption.
This RFC accepts that risk. Closing it needs §4.3's synced offset,
whose measured cost is a second flush on every sync.

### 3.5 Amendments to RFC 0008

- **§5 RFC0008.2** and its **§8 testing text** sort a complete but
  unsynced frame into three buckets. A fourth is added: "**or healed as
  a zero-filled tail (RFC 0068 §3.1 T2)** when every byte from the last
  valid frame boundary to EOF on the newest segment is zero". The rule
  "a CRC mismatch is corruption, not truncation" is unchanged.
- **§5 RFC0008.4** gains T2: "a partial frame header, a valid header
  followed by a short payload, **or a run of zero bytes from the last
  valid frame boundary to EOF** (RFC 0068 §3.1)".
- **§5 RFC0008.5**'s "an unknown `kind` byte" is scoped by RFC 0068
  §3.2: on the newest segment, a zero header that starts a T2 tail is a
  torn tail, not corruption. Every other RFC0008.5 sub-case is
  unchanged.
- **§6.6 step 3**, on the newest segment, checks T2 before it reports an
  unknown kind, a non-zero pad or an oversize `len`. **Step 4** is
  unchanged.

## 4. Alternatives considered

### 4.1 Keep failing closed (the status quo)

This is the simplest option, and the strongest at detecting corruption:
nothing is cut except a partial frame. The cost is that an ordinary
power loss during ingest needs an operator to truncate the segment by
hand, and on the replicated backend that happens to every group that
was appending. It defends against a rare device fault by making the
common crash an outage, so this RFC rejects it.

### 4.2 Also heal a frame zeroed from a sector boundary

A delayed-allocation crash can also write a frame's header block but
not its payload block. The frame then ends in zeros from a 512-byte
boundary `A` inside it to EOF. Healing that shape, with "zeros from an
aligned `A` inside the first invalid frame to EOF", looks natural but
is not safe with RFC 0008's framing. The CRC covers the whole frame, so
a mismatch cannot locate the damage. An acknowledged frame whose payload
happens to end in zeros, damaged by a bit flip before `A`, would match
the rule and be cut. So would a frame ending exactly at `A` whose damage
lies wholly before the zeros. This RFC leaves that shape as corruption.
It can be healed safely only with evidence that the frame was never
synced, which is what §4.3 provides.

### 4.3 Record a synced offset

Make the synced offset durable as part of each `Wal::sync`, before it
returns, for example in the RFC 0052 reclaim sidecar or a small
per-segment trailer. If that write fails, the sync fails and its turns
are not acknowledged, so every acknowledged frame lies below a durable
offset. Persisting it only after `sync` returns would leave a crash
window in which a batch is acknowledged but the recorded offset is
still the previous one. Recovery could
then cut any damaged tail that starts past that offset, including §4.2's
shape, and fail closed on any damage before it. That would also remove
§3.4's residual risk.

A spike measured the price (branch `spike/rfc0068-synced-offset`, not
for merge). Each record is a segment UUID, an offset, a sequence number
and a CRC, written to one of two slots in turn, so a torn record write
damages only the slot of a batch not yet acknowledged. Three designs
were built:

| Design | Order of the writes | Never claims unsynced data | Heals §4.2 |
|---|---|---|---|
| Sequential sidecar | fdatasync the segment, then write and fdatasync a `SYNCED` file | yes | yes |
| Concurrent sidecar | both fdatasyncs at once | no | only if the record misses the crash |
| In-segment slot | the record shares the segment's one fdatasync | no | only if the record misses the crash |

Within one flush the device may persist blocks in any order, so a
record flushed with its data can reach disk while the data does not.
Recovery then cannot tell an unacknowledged batch from a device fault,
and must fail closed on it. Only the sequential design both covers
every acknowledged frame and never names data that was not synced. No
single-flush design does both: writing the previous batch's offset
instead never overclaims, but leaves the last acknowledged batch above
the record.

Per-sync latency, interleaved A/B runs, 4 KiB to 64 KiB batches:

| Platform | Without | Sequential | Concurrent | In-segment |
|---|---|---|---|---|
| Linux CI runner, fdatasync, p50 | 375–474 µs | +30–40% (about +140 µs) | +3–12% | +24–29% |
| macOS, `F_FULLFSYNC`, p50 | 4.0–5.0 ms | +80–100% (about +4 ms) | +80–100% | 0% |

The cost is per sync, so group commit only spreads it over more frames.
Each cheap design is cheap on one platform only, and neither is precise.
This RFC does not adopt §4.3. The replicated backend's fault tests
(RFC 0065) revisit it: there, a replica that silently loses an
acknowledged entry still votes under its old identity, so the loss can
reach the whole group.

### 4.4 Preallocate and zero every segment

WALs that preallocate their segments, such as PostgreSQL's, zero them
(or `fallocate` and zero) before use. The end of the log is then the
first header that reads as zero, and the file size never changes after
a crash. That removes the zero tail, but not the other shapes: a header
that reached disk ahead of its payload still leaves a complete frame
with a CRC mismatch, which is §4.2's case, and only atomic frame
persistence or §4.3 would heal it. Nor does it remove the ambiguity
between the end of the log and a zeroed frame. It also costs a
full segment write at rotation (128 MiB by default) and changes RFC
0008's segment layout, while §3.1 gets the same availability without a
format change.

### 4.5 An operator repair command

An `ourios wal repair` verb could report and truncate a damaged tail on
request. That keeps fail-closed as the default and documents the fix,
but the outage lasts until someone runs it, which is what #1165 asks to
remove. It may still be worth having for real corruption, separately.

## 5. Acceptance criteria

> Drafted ahead of `specified`, so review can check that they are
> testable.

> **Scenario RFC0068.1 — a zero tail at a frame boundary is healed `[H3]`**
> - **Given** a newest segment with valid frames ending at `B`, followed
>   by `n` zero bytes to EOF, for `n` in {12, 13, 511, 512, 4096,
>   4096 + 7}
> - **When** the WAL is opened and `Wal::replay` runs recovery
> - **Then** it opens, every frame before `B` is replayed, and the
>   segment is truncated to `B`
> - **And** the next append lands at `B` and survives a reopen

> **Scenario RFC0068.2 — every other tail shape keeps today's outcome `[H3]`**
> - **Given**, each in its own arm, a newest segment whose valid frames
>   end at `B`, followed by:
>   1–11 bytes of any value (healed, as RFC0008.4); a well-formed header
>   whose payload runs past EOF (healed, as RFC0008.4); a complete frame
>   with a CRC mismatch whose payload ends in zeros (corruption); a
>   frame whose header is intact and whose payload is zeroed from a
>   512-aligned offset to EOF (corruption, §4.2); a tail of 12 bytes or
>   more that is zeros followed by one non-zero byte (corruption); or
>   zeros followed by a valid frame (corruption)
> - **When** the WAL is opened and `Wal::replay` runs recovery
> - **Then** the healed arms open and truncate to `B`, and the
>   corruption arms fail with `RecoveryError::Corrupt` naming the
>   segment and offset, with no segment modified

> **Scenario RFC0068.3 — closed segments are unchanged `[H3]`**
> - **Given** a closed (non-newest) segment ending in a T2 zero tail
> - **When** the WAL is opened and `Wal::replay` runs recovery
> - **Then** it fails as RFC0008.5 corruption, exactly as today

> **Scenario RFC0068.4 — a crash test with a zero-extended tail `[H3]`**
> - **Given** the RFC0008.2 crash-recovery test
> - **When**, after the kill, the newest segment is cut back to the end
>   `B` of its last valid frame, which is at or after its last synced
>   offset, and every byte from `B` to a new end `E` is zero, with
>   `E - B` a whole number of 512-byte sectors in one run and a partial
>   sector in another
> - **Then** recovery opens and every acknowledged record is present, as
>   RFC0008.2 already requires
> - **And** the segment is truncated to `B`. This scenario does not
>   simulate a sector shared with a later append; RFC0068.6 does

> **Scenario RFC0068.6 — a sector shared with an unsynced append `[H3]`**
> - **Given** a newest segment whose last acknowledged frame ends at `X`
>   inside a 512-byte sector, followed by an unsynced append that starts
>   at `X` and runs past that sector
> - **When** the segment is forged as a power cut under §3.4's guarantee
>   leaves it: the shared sector holds its old contents (zeros after
>   `X`) or its new contents, and every later byte of the append is zero
> - **Then** recovery never truncates below `X`: it either opens with
>   the acknowledged frame replayed, or fails as RFC0008.5 corruption
>   with no segment modified
> - **And** the old-contents arm is healed as T2 at `X`

> **Scenario RFC0068.5 — a heal is reported**
> - **Given** a newest segment whose tail is torn as T1 or T2
> - **When** `Wal::replay` completes recovery
> - **Then** exactly one `WARN` event `ourios.wal.recovery.tail.truncated`
>   carries the segment's `file.path`, `B` as
>   `ourios.wal.recovery.tail.offset`, `E` as `file.size` and the shape
>   as `ourios.wal.recovery.tail.shape`
> - **And** `ourios.wal.recovery.truncated.size` grows by `E - B`, with
>   the same `ourios.wal.recovery.tail.shape`

## 6. Testing strategy

- **RFC0068.1–.3:** integration tests in `ourios-wal/tests/it/`, beside
  `rfc0008_4_*` and `rfc0008_5_corruption.rs`, reusing their
  forged-segment helpers.
- **A `proptest` over the tail after `B`:** it varies the length, the
  bytes and where any zeros start, and asserts the partition: healed
  exactly when T1 or T2 holds, corruption otherwise. T1 is today's
  RFC0008.4 rule, so the property also pins that it is unchanged.
- **RFC0068.6:** an integration test beside RFC0068.1, forging both
  sector outcomes with the same helpers.
- **RFC0068.4:** extend the existing RFC0008.2 crash test (AGENTS.md
  §6.2, the WAL crash-recovery test), which runs on every PR.
- **RFC0068.5:** a unit test on the recovery driver's event and
  counter.
- **Benchmarks:** recovery stays O(N) (RFC0008.3). The zero check only
  reads bytes the scan already reads, and only on the newest segment.

## 7. Open questions

- [x] **Require sector-write atomicity (§3.4)?** Yes, as a deployment
  requirement in the operations docs. EBS, Hyperdisk and NVMe document
  it, and RFC 0008 already depends on it (§3.4).
- [x] **Accept §3.4's residual risk?** Yes. Closing it costs 30–40% of
  sync latency on Linux and doubles it on macOS (§4.3), to guard
  against a device fault on hardware that already breaks the first
  answer's requirement.
- [x] **Heal §4.2's shape later?** Not in this RFC. Only §4.3's
  sequential design heals it, so it is revisited with §4.3 in RFC
  0065's fault tests.
- [x] **The counter's name and unit.** `ourios.wal.recovery.truncated.size`
  in `By`, with the event and attributes §3.3 names (`ourios-semconv`
  v0.3.25).

## 8. References

- RFC 0008 (`accepted`): §5 RFC0008.2, .4 and .5; §6.3, the fsync
  policy; §6.6, crash recovery; §8, testing. This RFC amends RFC0008.2
  and its §8 text, RFC0008.4 and .5, and §6.6 step 3.
- RFC 0052 (`green`): the reclaim sidecar, a possible home for §4.3's
  synced offset.
- RFC 0065 (`red`) §6.1, which runs one RFC 0008 WAL per group; #1162,
  for `votes.log`'s zero-tail handling.
- #1165, the issue this RFC answers.
- AGENTS.md §3.4 (WAL-before-ack) and §4 hazard 3 (WAL durability
  versus latency); `docs/hazards.md` H3.
- Sector-write atomicity: AWS EBS supported block sizes and torn write
  prevention (<https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/supported-block-sizes.html>);
  GCP Hyperdisk atomic writes
  (<https://cloud.google.com/compute/docs/disks/hyperdisks>); the NVMe
  base specification's AWUPF field; Zheng et al., "Understanding the
  Robustness of SSDs under Power Fault", FAST '13.
- ext4 delayed allocation and the zero-length-file-after-crash
  behaviour: `Documentation/admin-guide/ext4.rst`, the `auto_da_alloc`
  mount option.
