# RFC 0053 split — history and file map

RFC 0053 originally specified WAL backpressure, publish-time unwind
safety, publication frontiers and tenant settlement, and an RFC 0005 §7
audit-durability amendment together, in one 3172-line document
(`rfc/0053-wal-backpressure-unwind` @ `30a21f80`). It was split into four
RFCs so each could reach its own maturity gate independently; this page
records what moved where. That original draft is the source of the
reviewed prose each of the four RFCs extracts into its own §9 (cited
there as "the quarry").

## Resulting files

| File | Owns | Source in the original draft |
|---|---|---|
| `docs/rfcs/0053-wal-backpressure.md` | Bound, reservation, wire contract, latch, forced-rotation livelock, `max_segments` as a header cap, backpressure telemetry. | §1 first half, §2.1, §3.1 minus seal/owed-rotation essay, §3.3 backpressure rows, RFC0053.1 / .4 / .5, §6 backpressure legs |
| `docs/rfcs/0054-publish-unwind-safety.md` | Invariant: no acknowledged batch becomes unreachable on panic; duplicates over loss; sweep may continue. | §2.2, §3.2 ownership / drain / worker / publisher panic, RFC0053.2 / .3 |
| `docs/rfcs/0055-publication-frontiers.md` | `PUBLISHED` sidecar, per-tenant record+audit watermarks, settlement rebuild, tenant slots, object-key intent, sidecar geometry. | The rest of §3.2, restart/sidecar legs of RFC0053.4 |
| `docs/rfcs/0056-audit-durability.md` | Amends RFC 0005 §7: permanent audit failure is not a successful drop; dependent records stay unpublished; tenant becomes terminal. | Status-note amendment + the three-way `write_owned` result |

## What moved to RFC 0052 instead

Some material in the original draft was a request of RFC 0052 rather than
specification belonging to 0053, and landed there (on #798) instead of
being kept here:

- `Journal::framed_len`, `rotation_due`, `RotationDecision` token on `append_batch`
- `Journal::rotate` result / `RotationKind` (discretionary vs owed)
- `.wal.seal` and the torn-tail heal
- `rebuild_ledger()` / `remeasure_unreclaimed()` as the restart seed
- `HousekeepingProgress.removed_segments`, `reclaimable_now`, `ReclaimSchedule`
- `cadence_failed` retirement, `failure_generation`

## What RFC 0053 kept and what it dropped

RFC 0053 itself kept: pre-append reservation; frame-byte unreclaimed
total; checked add; `TooLarge` first; `503`/`UNAVAILABLE` + protobuf
`Status` + `Retry-After` hint; per-request admission vs refusal latch;
forced rotation predicate; `unreclaimed_bytes_limit` / `max_segments`;
RFC0053.1, .4 (WAL restart figure only), .5; §4 sink-ceiling as a
`validated` prerequisite.

It dropped, to the three new RFCs: `.wal.seal`; `RecoverableBatch` / pool
supervisor; `PUBLISHED` and settlement; the RFC 0005 / 0046 / 0048
amendments; `max_tenants` slot reservation (now RFC 0055).

## Review discipline applied during the split

The split preserved the original draft's already-reviewed wording by
cutting and moving paragraphs rather than rewriting them from memory, and
a Copilot review was deferred until all four files existed and RFC 0053
itself was under 800 lines — so review comments would land on the
settled shape rather than on an intermediate one. Every new file started
at status `drafted`; RFC 0053 itself dropped from `specified` back to
`drafted` until its §3 held only backpressure.

## Result

Four markdown files exist, indexed in `docs/SUMMARY.md`. RFC 0053 is
under 800 lines at `drafted`. The split was announced on
[`#802`](https://github.com/jensholdgaard/ourios/pull/802), which lists
what moved where.
