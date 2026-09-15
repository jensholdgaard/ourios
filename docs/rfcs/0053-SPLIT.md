# Claude brief — split RFC 0053, do not keep specifying it

Source of truth for the next session. The 3172-line document on
`rfc/0053-wal-backpressure-unwind` @ `30a21f80` is a **quarry**, not a
document to grow.

## Hard rules

1. Do not add a new protocol to RFC 0053.
2. Do not request a Copilot review until the four files below exist and
   0053 is slimmer than 800 lines.
3. Default on a Copilot finding: classify (contradiction / other RFC /
   implementation / already decided). Do not "accept and specify here."
4. Keep reviewed wording when you move a paragraph. Cut, do not rewrite
   from memory.
5. Status of every new file is `drafted`. Drop 0053 from `specified` to
   `drafted` until its §3 is only backpressure.

## Four files

| File | Owns | Source in current 0053 |
|---|---|---|
| `docs/rfcs/0053-wal-backpressure-and-unwind-safety.md` | **Rename title to "WAL backpressure".** Bound, reservation, wire contract, latch, forced-rotation livelock, `max_segments` as a header cap, backpressure telemetry. | §1 first half, §2.1, §3.1 minus seal/owed-rotation essay, §3.3 backpressure rows, RFC0053.1 / .4 / .5, §6 backpressure legs |
| `docs/rfcs/0054-publish-unwind-safety.md` | **New.** Invariant: no acknowledged batch becomes unreachable on panic; duplicates over loss; sweep may continue. | §2.2, §3.2 ownership / drain / worker / publisher panic, RFC0053.2 / .3 |
| `docs/rfcs/0055-publication-frontiers.md` | **New.** `PUBLISHED` sidecar, per-tenant record+audit watermarks, settlement rebuild, tenant slots, object-key intent, sidecar geometry. | The rest of §3.2, restart/sidecar legs of RFC0053.4 |
| `docs/rfcs/0056-audit-durability.md` | **New.** Amends RFC 0005 §7: permanent audit failure is not a successful drop; dependent records stay unpublished; tenant becomes terminal. | Status-note amendment + the three-way `write_owned` result |

## Leave on #798 / RFC 0052

Do not keep specifying these in 0053. One bullet each under
"Amendments requested of RFC 0052":

- `Journal::framed_len`, `rotation_due`, `RotationDecision` token on `append_batch`
- `Journal::rotate` result / `RotationKind` (discretionary vs owed)
- `.wal.seal` and the torn-tail heal
- `rebuild_ledger()` / `remeasure_unreclaimed()` as the restart seed
- `HousekeepingProgress.removed_segments`, `reclaimable_now`, `ReclaimSchedule`
- `cadence_failed` retirement, `failure_generation` if 0052 still needs it

File those bullets on #798. If 0052 cannot land without them, they were
never 0053's.

## What "slim 0053" means

Keep: pre-append reservation; frame-byte unreclaimed total; checked add;
`TooLarge` first; `503`/`UNAVAILABLE` + protobuf `Status` + `Retry-After`
hint; per-request admission vs refusal latch; forced rotation predicate;
`unreclaimed_bytes_limit` / `max_segments`; RFC0053.1, .4 (WAL restart
figure only), .5; §4 sink-ceiling as a `validated` prerequisite.

Move out: `.wal.seal`; `RecoverableBatch` / pool supervisor; `PUBLISHED`
and settlement; RFC 0005 / 0046 / 0048 amendments; `max_tenants` slot
reservation (0055).

## Done when

- Four markdown files exist and are in `docs/SUMMARY.md`
- 0053 < 800 lines and status is `drafted`
- No Copilot review requested in that session
- A short comment on #802 lists what moved where
