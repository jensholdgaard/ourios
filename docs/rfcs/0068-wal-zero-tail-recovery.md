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
> (`accepted`): §5's RFC0008.4 and RFC0008.5, and §6.6 steps 3 and 4.
> RFC 0008 gets a dated pointer here once this RFC is accepted. RFC 0065
> (`red`) inherits the change, because its per-group WALs are RFC 0008
> segments.

## 1. Summary

On the **newest** segment only, recovery treats a tail of zero bytes
that runs to end of file as a torn write, and heals it the way RFC0008.4
heals a partial frame: `ftruncate(2)` to the last valid frame boundary,
fsync the segment, fsync the directory. Two zero patterns qualify:
zeros from the last valid frame boundary to EOF, and zeros from a
512-byte-aligned file offset inside the first invalid frame to EOF. Any
other damage on the newest segment, and any damage at all on a closed
segment, stays RFC0008.5 corruption, which stops recovery for every
segment.

## 2. Motivation

AGENTS.md §3.4 requires that an ingester killed mid-batch recover
without losing acknowledged data. RFC 0008 meets the *without losing*
half, and RFC0008.4 heals a frame cut short at EOF. It does not heal the
other common shape a crash leaves. On ext4 with delayed allocation, and
on other filesystems that journal metadata before data, a power loss
during an append that had not yet been fsynced can leave the file
**longer than its data**: the size reached the journal, but the blocks
behind it were never written, so they read back as zeros. Today's
`replay_segment` reads 12 zero bytes as a header with `len = 0`,
`kind = 0x00`, and stops with `RecoveryError::Corrupt { UnknownKind }`.
A frame whose header was written but whose payload was zeroed from a
block boundary fails with `CrcMismatch`. Either way, `Wal::open` fails
and the ingester stays down until an operator intervenes (#1165).

Those bytes belong to an append whose `sync` never returned, so no
client was ever acknowledged for them. Refusing to start loses nothing,
but it turns an ordinary power loss into an outage. That is the
H3 trade-off between durability and availability, and the wrong side
of it for data that was never promised. RFC 0065's replicated backend
makes this more pressing. Each replica runs one RFC 0008 WAL per group,
so a power loss across a rack could wedge many groups at once. #1162's
`votes.log` already heals the same shape and stays fail-closed on any
non-zero damage.

## 3. Proposed design

### 3.1 The zero-tail rule

Recovery scans the **newest** segment exactly as RFC 0008 §6.6 step 3
does, up to the first frame that does not read as complete and valid.
Call that frame's offset `B`, the last valid frame boundary, and the
segment length `E`. Recovery treats the bytes `[B, E)` as a **torn
tail** when either of these holds:

- **Z1, a zero tail at a frame boundary.** Every byte in `[B, E)` is
  zero. This includes `E - B ≥ 12`, which is today's `UnknownKind` case.
- **Z2, zeros from a sector boundary.** There is an offset `A` with
  `B < A < E` and `A % 512 == 0`, every byte in `[A, E)` is zero, and
  the frame at `B` is incomplete or fails its checks only because of
  bytes at or after `A`. In other words, its header in `[B, B + 12)`,
  when it lies wholly before `A`, is well-formed (known kind, zero pad,
  `len ≤ MAX_FRAME_BYTES`), and `B + 12 + len ≥ A`, so the frame
  reaches into the zeroed region. Because it does, its CRC cannot be
  checked; a frame that ends before `A` and still fails its CRC is
  corruption.

The alignment is 512 bytes because that is the smallest sector a
filesystem writes, so delayed-allocation zeros always start on a
multiple of it. Using 512 rather than the 4 KiB block size accepts
every real zero-fill and nothing an aligned 4 KiB rule would reject.

A torn tail is healed by RFC 0008 §6.6 step 4, unchanged:
`ftruncate(2)` to `B`, `fdatasync` the segment, `fsync` the parent
directory. The next append lands at `B`.

### 3.2 What stays corruption

These remain RFC0008.5 corruption (the all-segments-stop path),
exactly as today:

- any zero tail, or any torn tail, on a **closed** segment;
- on the newest segment, a non-zero byte anywhere in `[A, E)`, or in
  `[B, E)` for Z1;
- a zeroed region that does not reach EOF, for example zeros followed
  by a valid-looking frame;
- a frame that lies wholly before `A` and fails its CRC, an unknown
  kind, a non-zero pad, or an oversize `len`.

### 3.3 Operator visibility

A healed zero tail is a recovery event, not silent. Recovery logs one
structured `WARN` event naming the segment UUIDv7, `B`, `E`, and which
rule matched (Z1 or Z2). It also adds `E - B` to a byte counter of
healed tail bytes, whose name goes through the shared `ourios-semconv`
registry. The same event fires for today's RFC0008.4 partial-frame
heal, so one signal covers both shapes. No audit event is emitted,
consistent with the RFC0008.5 audit event being deferred (RFC 0008 §5,
2026-06-13 amendment).

### 3.4 Why this cannot drop an acknowledged frame

A frame is acknowledged only after a `Wal::sync` covering it has
returned (RFC 0008 §6.3). After that, its bytes are on stable storage,
and delayed allocation cannot later turn them back into zeros. So a
zero region produced by a crash lies wholly after the last synced
offset, and every frame before `B` that was acknowledged is replayed.
The rule removes only bytes at or after `B`. The residual risk is a
**device fault** that zeroes the tail of an acknowledged frame, running
from a sector boundary to EOF, on the newest segment. Today that fails
closed; under this RFC it would be healed and that frame lost. §7
asks whether to accept that risk or to close it with a synced-offset
record (§4.3).

### 3.5 Amendments to RFC 0008

- **RFC0008.4** gains the zero-tail shapes: "a partial frame header, a
  valid header followed by a short payload, **or a zero-filled tail per
  RFC 0068 §3.1**".
- **RFC0008.5**'s "a CRC32-C mismatch on a complete frame" and "an
  unknown `kind` byte" are scoped by RFC 0068 §3.2. On the newest
  segment, a frame matching Z1 or Z2 is a torn tail, not corruption.
- **§6.6 step 3** branches to the torn-tail path for Z1 and Z2 before it
  reports `UnknownKind` or `CrcMismatch` on the newest segment. Step 4
  is unchanged.

## 4. Alternatives considered

### 4.1 Keep failing closed (the status quo)

This is the simplest option, and it is the strongest at detecting
corruption, because nothing is ever cut except a partial frame. The cost
is that an ordinary power loss during ingest needs an operator to
truncate the segment by hand. On the replicated backend that cost is
multiplied by every group that was appending. It defends against
device faults at the expense of the much more common crash shape, so
this RFC rejects it.

### 4.2 Preallocate and zero every segment

WALs that preallocate their segments, such as PostgreSQL's, write
zeros (or `fallocate` and zero) before use. End of log is then "the
first header that reads as zero", and the size never changes after a
crash. That makes Z1 the only shape, but it does not remove the
ambiguity: a zero header is still either the end of the log or a
zeroed frame. It also costs a full segment write at rotation (128 MiB by
default) and changes RFC 0008's segment layout. The rule in §3.1 gets
the same availability without a format change.

### 4.3 Record a synced offset

After each `Wal::sync`, persist the synced offset, for example in the
RFC 0052 reclaim sidecar or a small per-segment trailer. Recovery could
then cut a damaged tail only when it lies past that offset, and fail
closed on any damage before it. That removes §3.4's device-fault risk
entirely. The price is a second durable write per sync, either an extra
fsync or the sidecar's own write path, on the ingest hot path that
`criterion` benchmarks gate. If §7 decides the residual risk is not
acceptable, this is the follow-up; the §3.1 rule would then also
require `B ≥ synced offset`.

### 4.4 An operator repair command

A `ourios wal repair` verb that reports and truncates a damaged tail
on request. This keeps fail-closed as the default and makes the fix a
documented command. It still leaves the outage until someone runs it,
which is the problem #1165 asks to remove. It may still be worth having
for real corruption, separately from this RFC.

## 5. Acceptance criteria

> Drafted ahead of `specified` so review can check that they are
> testable.

> **Scenario RFC0068.1 — a zero tail at a frame boundary is healed `[H3]`**
> - **Given** a newest segment with valid frames ending at `B`, followed
>   by `n` zero bytes to EOF, for `n` in {1, 11, 12, 4096, 4096 + 7}
> - **When** `Wal::open` runs recovery
> - **Then** it opens, every frame before `B` is replayed, and the
>   segment is truncated to `B`
> - **And** the next append lands at `B` and survives a reopen

> **Scenario RFC0068.2 — a frame zeroed from a sector boundary is healed `[H3]`**
> - **Given** a newest segment whose last frame starts at `B`, with its
>   bytes intact up to a 512-aligned offset `A` inside it, then zeros to
>   EOF, including cases where `A` falls inside the header and inside the
>   payload
> - **When** `Wal::open` runs recovery
> - **Then** it opens, the frame at `B` is not replayed, every earlier
>   frame is, and the segment is truncated to `B`

> **Scenario RFC0068.3 — damage that is not a zero tail is still corruption `[H3]`**
> - **Given**, each in its own arm, a newest segment with: a non-zero
>   byte after the zero region begins; zeros followed by a valid frame;
>   zeros starting at a non-aligned offset inside a frame; or a CRC
>   mismatch on a frame that ends before any zeros
> - **When** `Wal::open` runs recovery
> - **Then** it fails with `RecoveryError::Corrupt` naming the segment
>   and offset, and no segment is modified

> **Scenario RFC0008.4/.5 regression — closed segments are unchanged `[H3]`**
> - **Given** a closed (non-newest) segment with a zero tail matching Z1
>   or Z2
> - **When** `Wal::open` runs recovery
> - **Then** it fails as RFC0008.5 corruption, exactly as today

> **Scenario RFC0068.4 — a crash test with a zero-extended tail `[H3]`**
> - **Given** the RFC0008.2 crash-recovery test
> - **When** after the kill, the newest segment is zero-extended past its
>   last synced offset, by a whole number of 512-byte sectors and by a
>   partial sector
> - **Then** recovery opens and every acknowledged record is present, as
>   RFC0008.2 already requires

> **Scenario RFC0068.5 — a heal is reported**
> - **Given** a recovery that heals a tail under Z1, Z2 or RFC0008.4
> - **Then** exactly one structured `WARN` event names the segment, `B`,
>   `E` and the rule, and the healed-bytes counter grows by `E - B`

## 6. Testing strategy

- **RFC0068.1–.3 and the closed-segment regression:** integration
  tests in `ourios-wal/tests/it/` beside `rfc0008_4_*` and
  `rfc0008_5_corruption.rs`, using their forged-segment helpers. Add a
  `proptest` over the tail shape (where zeros start, whether that offset
  is aligned, where any non-zero damage is) that asserts the §3.1/§3.2
  partition: healed exactly when Z1 or Z2 holds, corruption otherwise.
- **RFC0068.4:** extend the existing RFC0008.2 crash test
  (AGENTS.md §6.2, the WAL crash-recovery test), run on every PR.
- **RFC0068.5:** a unit test on the recovery driver's event and counter.
- **Benchmarks:** recovery stays O(N) (RFC0008.3). The zero check only
  reads bytes the scan already reads, on the newest segment.

## 7. Open questions

- [ ] **Accept §3.4's residual risk, or add a synced offset (§4.3)?**
  The draft accepts the risk: a device fault that zeroes the tail of
  an acknowledged frame on the newest segment would be healed, not
  reported as corruption. The alternative costs a durable write per
  sync.
- [ ] **Should Z2 be adopted, or Z1 only?** Z1 alone heals the
  `UnknownKind` case, which is the common one (a whole unwritten
  block). Z2 also heals a frame whose header block was written but
  whose payload block was not. Z2 widens §3.4's risk slightly.
- [ ] **The alignment.** This draft uses 512. Is there a supported
  filesystem whose zero-fill starts at a smaller granularity, or that
  fills with something other than zeros?
- [ ] **The counter's name and unit.** It goes through the
  `ourios-semconv` registry.

## 8. References

- RFC 0008 (`accepted`) §5 RFC0008.2, .4 and .5; §6.3, fsync policy;
  §6.6, crash recovery. This RFC amends .4, .5 and §6.6 steps 3 and 4.
- RFC 0052 (`green`), the reclaim sidecar, a possible home for §4.3's
  synced offset.
- RFC 0065 (`red`) §6.1, per-group RFC 0008 WALs, and #1162's
  `votes.log` zero-tail handling.
- #1165, the issue this RFC answers.
- AGENTS.md §3.4 (WAL-before-ack) and §4 hazard 3 (WAL durability
  versus latency); `docs/hazards.md` H3.
- ext4 delayed allocation and the zero-length-file-after-crash
  behaviour: `Documentation/admin-guide/ext4.rst`, the `auto_da_alloc`
  mount option.
