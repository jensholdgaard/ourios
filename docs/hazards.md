# Hazards

> Referenced from `CLAUDE.md` §4 ("Before any change to the hot path,
> re-read `docs/hazards.md`") and §10 ("When in doubt: 1. Read
> `docs/hazards.md`"). This document is the load-bearing reading for
> any hot-path reviewer. Each hazard names a specific failure mode,
> the mitigation we have committed to, the detection signal, and the
> rule for when a deviation is a tuning question vs. an architectural
> one.

## How to use this document

- **Before** opening a PR that touches any subsystem named in a
  hazard section: re-read that section. The PR description must
  explicitly say *which* hazard it touches and *how* the change
  preserves the mitigation.
- **In review:** if a hazard is touched and not addressed in the PR
  description, that is a block, not a nit.
- **In production:** the named detection signals are the alerts that
  cannot be silenced without an RFC. They exist precisely so the
  failure mode is visible *before* it corrupts data.

Hazards map onto invariants in `CLAUDE.md` §3. Hazards describe what
goes wrong; invariants describe what we promised. They are two faces
of the same constraint.

---

## H1 — Template miner correctness

**Failure mode.** The miner merges semantically-distinct templates
because they share token structure. The canonical horror: `user
logged in <*>` and `user logged out <*>` differ in one token; below
a permissive threshold they merge into `user logged <*> <*>`. A
query for the login event silently returns logout rows. The
operator never knows.

**Mitigation.**
- Default similarity threshold ≥ 0.7 (strict).
- Lowering the threshold below 0.7 requires an RFC, not a config
  change.
- Three-zone confidence model: clean match (≥ threshold) / lossy
  match (floor ≤ x < threshold, retain body, set `lossy_flag`) /
  parse failure (< floor, retain body, increment counter).
- Every template-widening event is audited: the audit record names
  the old template, the new template, tenant, timestamp, and reason.

**Detection.** All metrics carry `tenant_id`; some carry `service`.
- `merges_total` counter: spike on stable input → service-version
  change *or* threshold drift.
- `body_retention_ratio` gauge: rising → input shifted *or*
  threshold is too tight.
- `confidence_p01` histogram tail: collapsing → many matches are
  barely passing; threshold should be revisited.
- `parse_failures_total`: nonzero is genuine failure, not lossy.

**Escalation.** A spike on one tenant is a tuning question (masking
rules, per-tenant threshold). A spike across many tenants on a
stable corpus is a policy question — RFC.

**See also.** `CLAUDE.md` §3.1; `docs/rfcs/0001-template-miner.md`
§§6.3–6.4; `docs/benchmarks.md` C2 (template count convergence),
C3 (merge rate).

---

## H2 — Parameter cardinality blowup

**Failure mode.** A `params` slot captures something it should not
— an entire stack trace, a base64 payload, a request body, a
megabyte JSON blob. Parquet's dictionary encoding for that column
collapses (every value distinct). File sizes explode. Query latency
on that column degrades by orders of magnitude. The backend's
compression claim evaporates for that workload.

**Mitigation.**
- Per-parameter byte limit, **default 256 B**, ceiling **1 KiB** —
  raising the ceiling requires an RFC.
- Overflow spills the original value into the `body` column; the
  `params` slot is replaced by a truncation marker (length + hash,
  no original payload).
- Counter increments on overflow.

**Detection.**
- `params_overflow_ratio` per service: alert when **> 1 %** of
  lines for any one service hit overflow.
- Parquet column-size variance: a column whose dictionary efficiency
  drops sharply between compactions usually means a new overflow
  pattern.

**Escalation.** Service-specific spike → masking rule that
pre-redacts the offending field. Broad spike → revisit the limit
(still ≤ 1 KiB). Anyone proposing > 1 KiB → RFC.

**See also.** `CLAUDE.md` §3.2; RFC 0001 §6.5; benchmarks C4.

---

## H3 — WAL durability vs. latency

**Failure mode.** The ingester acknowledges an OTLP batch before
the write is durably persisted. The ingester then crashes (process
kill, host failure, container reschedule). The producer believes
the data was accepted; we have lost data we promised to keep.

**Mitigation.**
- An ack is emitted **only after** fsync (or equivalent durability
  primitive) on the WAL.
- Batched fsync with an explicit operator-tunable knob: default
  flush every **100 ms** or when the current segment fills,
  whichever first.
- Crash-recovery test is part of CI: SIGKILL the ingester
  mid-batch, restart, assert no acknowledged data is missing. Test
  runs on every PR; failure blocks merge.
- Replication, when added, is **in addition to** the WAL, not a
  replacement.

**Detection.**
- `ingest_ack_latency_p99`: rising trend usually means fsync is the
  bottleneck.
- `wal_unflushed_bytes`: bytes acked but not yet on durable storage
  — must always be bounded.
- CI crash-recovery test: any failure is critical, regardless of
  flake history.

**Escalation.** Fsync latency rising → tune batch size or move to
faster storage. Ack-without-fsync ever observed in code review → P0
bug, hotfix path.

**See also.** `CLAUDE.md` §3.4; future RFC 0003 (WAL design);
benchmark D2 (compaction keeps up).

---

## H4 — The small-file problem

**Failure mode.** WAL segments get rotated and flushed to Parquet
too eagerly. The result is thousands of small files per tenant per
day. Object-storage `LIST` calls dominate query planning time. Cold
cache hits are murderous. Operators see "query took 12 s on 4 GB of
logs" and lose faith in the backend.

**Mitigation.**
- Target **row-group size 128 MB – 1 GB** inside each Parquet file.
- Target **file size 256 MB – 2 GB** post-compaction.
- Background compaction job per tenant; cadence is a tunable.
- Compaction is required to keep the WAL backlog bounded under
  sustained ingest (D2).

**Detection.**
- File-size histogram per tenant: fewer than **5 %** of files
  below 128 MiB at steady state.
- File count vs. data volume: file count must grow sub-linearly
  with bytes ingested.

**Escalation.** Skewed file-size distribution on a single tenant →
compaction tuning. Sustained small-file emission across the cluster
→ ingest-scaling block, RFC.

**See also.** `CLAUDE.md` §4 hazard 4; benchmarks D3.

---

## H5 — Template schema evolution across deploys

**Failure mode.** A service ships a new version. Log format
changes — a new field, a renamed token, reordered words. The
template tree built from last month's logs no longer matches the
new format cleanly. Queries against `template_id = X` start
returning *incomplete* results because some rows are now stored
under `template_id = X'`. The operator sees a 30 % drop in event
volume and misdiagnoses it as an outage.

**Mitigation.**
- Templates are versioned: a template's *internal representation*
  can change; the *logical identity* persists across versions.
- Explicit alias mechanism: `template_id.resolves_to(X)` in the
  DSL resolves a query across all aliases of `X`.
- Drift detection is a first-class query — operators can ask "what
  templates drifted in the last 24 h?" and get a list.
- A new `template_version` emits an audit event, just like a merge.

**Detection.**
- Spike in distinct template count immediately after a deploy →
  expected; investigate only if it persists past the deploy window.
- Diff between `template_id = X` and `template_id.resolves_to(X)`
  result counts → measures alias coverage.
- Audit event volume: drift events should correlate with deploy
  cadence, not appear randomly.

**Escalation.** Alias graph becomes a tangle (templates with > N
aliases or cycles) → revisit alias semantics, RFC. Drift correlated
with deploys → expected; not an alert.

**See also.** `CLAUDE.md` §3.5; RFC 0001 §6.7.

---

## H6 — Query DSL vs. DataFusion SQL surface

**Failure mode.** A user-facing query surface accidentally exposes
DataFusion specifics — a SQL keyword leaks into an error message, a
planner hint becomes documented, a join type that doesn't make
sense in a logs context becomes reachable. We then cannot upgrade
DataFusion or change the planner without breaking saved user
queries and dashboards. The DSL has become a contract we never
intended to sign.

**Mitigation.**
- The DSL is a **separately specified** layer (`docs/rfcs/0002`).
- All DSL constructs compile to DataFusion `LogicalPlan`, never to
  SQL strings. SQL never appears in any user-visible output.
- No SQL escape hatch by default. If one is added later, it ships
  under a separate RFC, sandboxed, opt-in, and tenant-gated.
- DSL evolution is a written semver contract with users; major
  versions ship with a deprecation window.

**Detection.**
- PR review: any test or error message containing the substring
  "DataFusion" or referring to a DataFusion type by name in a
  user-facing surface is a block.
- Any code path that constructs SQL strings from user input is a
  block.
- User report: "this query worked yesterday after the upgrade"
  triggers a regression review.

**Escalation.** Leak found in user-facing surface → block + hotfix.
Recurring temptation in implementation → tighten the API boundary,
move shared helpers behind a non-exported module.

**See also.** `CLAUDE.md` §4 hazard 6; RFC 0002.

---

## H7 — Bit-identical body reconstruction

**Failure mode.** An operator opens the UI and asks "show me what
was actually logged." We render the row from `template + params`
and produce a string that drops a space, a quote, a separator, or a
trailing newline. The operator chases a bug that doesn't exist —
or, worse, *fails to chase* a bug that does, because the rendered
line looked normal.

**Mitigation.**
- The miner either captures inter-token whitespace and separators
  *or* it sets `lossy_flag = true` on the row. There is no third
  option.
- Reconstruction is a **property test** against the testdata
  corpus: for every non-lossy row, `reconstruct(record) ==
  ingested_bytes` exactly. Property failure blocks merge.
- The reader honours `lossy_flag`. The UI surfaces lossy rows with
  an explicit warning ("this row's body cannot be exactly
  reconstructed") rather than rendering them.
- Tenants may opt into default-on body retention at a storage cost.

**Detection.**
- Reconstruction property test (CI): zero failures, ever, on the
  committed corpus.
- `body_retention_ratio` gauge: a sudden rise indicates input
  distribution change OR a regression in whitespace capture.
- User complaint of "the rendered log does not match what we sent"
  → reproduce, add to corpus, fix.

**Escalation.** Ever fails on a real-world corpus → block + hotfix.
Whitespace-capture state machine becomes a complexity sink →
simplify by retaining more bodies; the storage cost is real but
acceptable, lying to the user is not.

**See also.** `CLAUDE.md` §3.3; RFC 0001 §6.6; benchmarks C1.

---

## Adding a new hazard

A new hazard belongs in this document if **all** of the following
hold:

- It is a failure mode that silently corrupts data, lies to the
  user, or destroys the project's value proposition.
- It is *not* obvious from reading the code (otherwise it is a
  bug, not a hazard).
- It has at least one named mitigation in the codebase or a
  committed RFC.

A new hazard is added via a `meta:` RFC, the same path as changes
to `CLAUDE.md` §3 invariants.
