---
rfc: 0063
title: Configurable receiver flush age
status: red
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-11-01
supersedes: —
superseded-by: —
---

# RFC 0063 — Configurable receiver flush age

> **Status: `red`.** Every §5 scenario has an `#[ignore]`d `todo!` stub
> in `ourios-server` (`tests/it/rfc0063_flush_age.rs`) naming the slice
> that discharges it.
> Specified 2026-11-01 (#1143, maintainer-approved: option 1 of #1141
> with the `1..=300` range). Tracks #1141 (option 1). This RFC puts RFC
> 0014 §3.2's `max_buffer_age` on the server config surface, the
> promotion RFC 0014 §7 leaves open ("Promoting them to RFC 0004 config
> knobs … remains open"). It contradicts no accepted text: the default
> stays 300 s, and the field can only lower it. RFC 0014 gets a dated
> pointer here once this RFC is accepted. It coordinates with RFC 0052
> (`green`), whose `barrier_secs` keeps its own 300 s (§3.3).

## 1. Summary

`receiver.max_buffer_age_secs` sets how old a partition's oldest
buffered record may get before the receiver flushes it to Parquet. The
default is today's 300 s, and the value can only be lowered (`1..=300`).
The age sweep runs every `min(30 s, max_buffer_age_secs)`, so a quiet
tenant's partition is drained within `max_buffer_age_secs` plus one
sweep; its records become queryable once that write reaches the store.
A development or low-volume install can trade more small files,
which the compactor already merges, for fresher queries.

## 2. Motivation

The querier reads Parquet only. A partition below the 256 MiB size
target is written when its oldest record is 300 s old
(`SINK_MAX_BUFFER_AGE`, `ourios-server/src/receiver.rs`), checked every
30 s (`SINK_FLUSH_TICK`). A quiet tenant's records are therefore
invisible for up to about five and a half minutes after they are acked.

#1141 found this while dogfooding 0.16.2: 336 records were ingested and
`/v1/query` returned `"rows":0` until the flush. Two cases suffer:

- **A first install.** A new user sends a few lines, queries, gets
  nothing, and reads it as data loss. The README and the quickstart
  guide now warn about it (#1140), but a warning is not a fix.
- **Incident use.** "What did this service log in the last minute" is the
  most common incident query. Loki answers it from ingester memory.

Serving buffered records from the querier is the full answer to tailing
(#1141 option 2), but it touches `AGENTS.md` §3.4 and §3.6 and needs its
own RFC. A configurable age is a small change that helps both cases now,
and it stays useful after option 2: it bounds how long records live
only in the WAL.

## 3. Proposed design

### 3.1 The field

`receiver.max_buffer_age_secs` (YAML, with `${env:VAR}` substitution
like every `ReceiverSection` field; bare env
`OURIOS_RECEIVER_MAX_BUFFER_AGE_SECS` without `--config`) is an integer
number of seconds.

- **Default `300`**, today's `SINK_MAX_BUFFER_AGE`.
- **Validation: `1..=300`.** Zero, a value over 300, or a non-integer is
  a config error at startup that names the field and the range. It is
  never clamped (RFC 0004 §3.2: clamping moves the operator onto a
  trade-off point they did not pick).
- **Why it can only be lowered.** Two consumers assume no record stays
  buffered longer than 300 s. The compactor's erasure settle
  (`compaction_policy`, `ourios-server/src/main.rs`) waits twice the
  receiver's maximum flush age, which makes it rare in normal operation
  that a row acked before an erasure request is still unpublished when
  the rewrite runs (RFC 0009 §3.4). It does not make that window empty:
  a flush backlog or a WAL replay can still publish later, the residual
  RFC 0009 §3.4 records as decision D3. The compactor can run as its own
  pod and cannot read the receiver's setting, so it keeps using the
  300 s ceiling. A receiver flushing sooner keeps that mitigation as it
  is and leaves the residual unchanged. RFC 0052's barrier
  (§3.3) drains every partition each 300 s, so a higher age would not
  take effect anyway. Raising the ceiling is a later RFC if anyone needs
  it.
- **Helm:** exposed through `extraEnv`, as RFC 0061 §3.1 does for
  `max_request_bytes`. The rendered receiver config carries
  `max_buffer_age_secs: "${env:OURIOS_RECEIVER_MAX_BUFFER_AGE_SECS:-}"`;
  setting the variable in `receiver.extraEnv` sets the age, and leaving
  it unset renders an empty value, which reads as unset and takes the
  default. `values.yaml` documents the variable next to `encodeWorkers`.
- **Classification:** a Tunable under RFC 0004. It moves a cost along
  hazard H4 (small files) and touches no `AGENTS.md` §3 invariant (§3.5).

### 3.2 The sweep follows the age

`flush_config()` takes `max_buffer_age` from the resolved config instead
of the constant. The age sweep's interval becomes
`min(SINK_FLUSH_TICK, max_buffer_age)`: at the default it stays 30 s;
at 10 s it is 10 s, so a partition is drained within 20 s rather than
40 s. Visibility adds the time to write the object and for the query to
run, and a sweep waits for the previous sweep's writes to settle. RFC 0014 §3.2's trigger rule (flush when age ≥ the configured
max) is unchanged.

### 3.3 The barrier keeps its own cadence

RFC 0052 §3.1 runs the WAL barrier on `barrier_secs`, "defaulting to the
sink's age trigger (300 s)", today the same constant (`BARRIER_TICK`).
This RFC keeps the barrier at 300 s whatever `max_buffer_age_secs` is.
The barrier cut quiesces ingest and drains every partition. Tying it to
a 10 s age would add a stall every 10 s for no gain, because the age
trigger already writes those partitions. A lowered age makes the barrier
cheaper, not more frequent: most partitions are empty when it runs.

### 3.4 Telemetry

No new instrument. Each flush already lands in the record sink's
existing flush-duration histogram and flushed-records counter
(`ourios-ingester/src/metrics.rs`), so the extra objects a lowered age
writes show there as more, smaller flushes.

### 3.5 Invariants and hazards

- **§3.4 WAL-before-ack:** unchanged. The age only decides when acked,
  WAL-durable records are copied to Parquet. A lower age publishes
  sooner but does not shorten recovery replay: the checkpoint still
  advances only at RFC 0052's barrier, and records an age flush
  published after the last cut are re-published on replay (RFC 0052
  §3.1).
- **§3.6 object storage as truth:** unchanged; nothing new is kept on
  local disk.
- **§3.7 tenancy:** the age is per receiver, the same for every tenant,
  and buffers stay per partition (and so per tenant).
- **H4 (small files):** a lower age writes up to one object per active
  partition per `max_buffer_age_secs` instead of per 300 s. At 10 s that
  is up to 30 times as many small objects for a quiet tenant. Compaction
  (RFC 0009, RFC 0036) merges them, and the cost is the operator's
  choice. The documentation says so next to the variable, and
  recommends the default for production.
- No other invariant or hazard is touched: no miner, schema or query
  change.

## 4. Alternatives considered

**Lower the default.** A 30 s default would fix first impressions
without any config, but it writes ten times as many small objects for
every low-volume tenant on every install, production included. The
default stays where RFC 0014 put it, and the README and quickstart
guide name the variable.

**Allow values above 300 s.** It would let a very-low-volume install
write fewer objects. The compactor's erasure settle and RFC 0052's
barrier both assume 300 s (§3.1), so a higher age would need both
changed. Nobody has asked for it.

**Expose the whole flush policy** (size target, ceiling, sweep tick).
RFC 0014 §7 leaves all of them open. Only the age has a user-visible
cost today; the others stay constants until a measurement asks for
them.

**A Helm value key.** RFC 0061 chose `extraEnv` for the same kind of
knob; one pattern for receiver tunables is easier to document.

**A per-tenant age.** RFC 0004 §3.4 has a per-tenant override mechanism
for miner tunables, but the sink's sweep is per receiver. A per-tenant
age would mean per-tenant sweep bookkeeping for a dev-install problem.

## 5. Acceptance criteria

> **Scenario RFC0063.1 — the age is configurable and validated**
> - **Given** `receiver.max_buffer_age_secs` unset, set to the empty
>   string, set to `10`, set to `0`, set to `301` and set to `ten`
> - **When** the server resolves its config
> - **Then** the resolved ages are `300`, `300` and `10` for the first
>   three, and the last three fail startup with an error naming the
>   field and the range `1..=300`
> - **And** the Helm chart's rendered receiver config resolves to `10`
>   with `OURIOS_RECEIVER_MAX_BUFFER_AGE_SECS=10` in
>   `receiver.extraEnv`, and to the default without it

> **Scenario RFC0063.2 — the sink drains on the configured age**
> - **Given** a record sink built from the resolved config with
>   `max_buffer_age_secs = 1`, holding records for one partition
> - **When** `drain_aged` runs at once, and again after 1.2 s of real
>   time
> - **Then** the first drain returns nothing and the second returns the
>   partition, without a shutdown or a WAL rotation

> **Scenario RFC0063.5 — a lowered age makes records queryable sooner**
> - **Given** a running server with `max_buffer_age_secs = 2` on the
>   local filesystem store, with no other load
> - **When** a few records are sent for one tenant and no more arrive
> - **Then** a query polled every 500 ms returns them within 15 s of
>   the ack, well under the 300 s default's ~330 s

> **Scenario RFC0063.3 — the sweep follows the age, the barrier does not**
> - **Given** resolved ages of `300` and `10`
> - **When** the receiver builds its sweep and barrier intervals
> - **Then** the sweep runs every 30 s and every 10 s respectively, and
>   the barrier runs every 300 s in both

> **Scenario RFC0063.4 — the compactor's erasure settle is unchanged**
> - **Given** any valid `max_buffer_age_secs`
> - **When** the compactor builds its policy
> - **Then** `erasure_settle_nanos` is `2 × 300 s + grace`, as today

## 6. Testing strategy

- RFC0063.1: config resolution unit tests in `ourios-server/src/config`,
  beside the `encode_workers` ones; the Helm arm in
  `deploy/helm/render-tests.sh`.
- RFC0063.2: a unit test in `ourios-server/src/receiver.rs` on the
  sink `flush_config` builds. It waits in real time, because the sink
  ages its buffers with `std::time::Instant`
  (`ourios-ingester/src/record_sink.rs`), which a paused Tokio clock
  does not move. RFC0063.3 covers the sweep interval with a paused
  clock.
- RFC0063.5: an integration test in `ourios-server/tests`, beside the
  existing served-role tests, using the bare-env form and a short age.
- RFC0063.3 and .4: unit tests in `ourios-server/src/receiver.rs` and
  `main.rs` on the interval and policy builders.
- RFC0014.2 (the age trigger) keeps passing unchanged.
- No benchmark: the hot path is untouched; only the sweep interval and
  one config read change.
- The quickstart guide and the README note from #1140 name the variable
  when this ships.

## 7. Open questions

- [x] Is 300 s the right ceiling, or should the range stop lower (say
  `1..=60`)? **Resolved (maintainer, 2026-11-01): `1..=300`**, as
  proposed. The ceiling only has to stay at or below the compactor's
  300 s assumption (§3.1).

## 8. References

- RFC 0014 §3.2 (flush triggers), §7 (open promotion to config); RFC
  0004 §3.2, §3.4 (tunables, clamping, per-tenant overrides); RFC 0009
  §3.4 (erasure settle); RFC 0052 §3.1 (`barrier_secs`); RFC 0061 §3.1
  (`extraEnv` pattern); RFC 0020 (config file front-end)
- `AGENTS.md` §3.4, §3.6, §3.7; `docs/hazards.md` H4
- Issue #1141, PR #1140
