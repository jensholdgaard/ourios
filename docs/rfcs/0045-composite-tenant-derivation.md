---
rfc: 0045
title: Operator-configured composite tenant derivation
status: superseded
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-08-17
supersedes: —
superseded-by: RFC 0046
---

# RFC 0045 — Operator-configured composite tenant derivation

> **Status: `superseded` by RFC 0046 (2026-08-17).** The maintainer ruled
> the same day that tenancy does not reside in OTLP data — no resource
> attribute is ever a tenancy input — so the in-band composite derivation
> this RFC specified was the wrong model, not a bad rule. RFC 0046
> (out-of-band tenancy) replaced §3.1–§3.4 (rule, epoch log, detector)
> in #702; the `Store` double-encoding fix of §3.2 and `TenantId` opacity
> survive. The registry entries the detector minted are `deprecated`, not
> deleted. Kept as the record of a `green` implementation and of the
> reasoning that led out of it.
>
> *(`green`, earlier the same day, before the ruling:)*
>
> **Status: `green` (2026-08-17).** All ten §5 criteria pass, landed in
> one implementation PR (#692) of six slices the same day as the spec
> (#689): the `TenantRule` key list + `receiver.tenant` config (.1/.2/.3/.4/.6);
> the rule-epoch log (.10, on the RFC0014.5 crash fixture); the divergence
> detector + `ourios.receiver.tenant.divergences` (.7/.9); the served-binary
> sequence over one store + WAL — default → composite → composite + token
> (.2/.3/.4/.5/.8); a Helm `receiver.tenant` passthrough. Two things
> implementation forced on the spec, both recorded inline: the `Store`
> double-encoded any tenant id with a reserved character (§3.2 — fixed as
> `fix(parquet)!`, one-shot prefix rename for legacy objects), and the
> detector compares a digest + length rather than the 128-byte preview
> (§3.4). No thesis-gate applies (`validated` vacuous, RFC 0008/0044
> precedent); `accepted` is a maintainer flip.
>
> *(`specified`, same date: §5 criteria written and testable. Grounded in
> the tenancy concept discussion (#688): the maintainer-settled Q1–Q10
> answers (Q1–Q7 in the issue body, Q8–Q10 raised and settled in its
> comment thread) are this RFC's premises, restated in §2/§3 where they
> bind.)*

## 1. Summary

Expose the tenant-derivation rule RFC 0001 §6.1 reserved: an ordered list of
resource-attribute keys, configured by the operator, whose values join into
the tenant id (`[k8s.cluster.name, service.name]` → `cluster1/fluxcd`). The
default stays `[service.name]`, byte-identical to today. Rule changes have
**append-only epoch** semantics — newly ingested data derives under the new
rule, stored ids never change, nothing repartitions. A **divergence
detector** watches for the misconfiguration this RFC exists to kill: one
tenant whose records span multiple values of a higher-order key (two
clusters silently merged) announces itself with a warning and a counter
instead of corrupting quietly.

## 2. Motivation

**`service.name` is not globally unique — by specification.** The semconv:
*"`service.name` is expected to be unique within the same namespace"*;
global uniqueness holds only for the `service.namespace` / `service.name` /
`service.instance.id` triplet. In Kubernetes the collision is guaranteed,
not incidental: `service.name` is calculated from `k8s.deployment.name`
(the documented k8s-attributes chain), so two clusters running the same
deployment — fluxcd in `cluster1` and `cluster2` — derive the byte-identical
tenant and their telemetry merges into one partition. A cross-tenant data
merge is the §3.7-class corruption this backend exists to prevent, and today
it is silent.

The mechanism is already general — `TenantRule::by_attribute(key)` exists,
derivation is per-`ResourceLogs` group with whole-export rejection when no
tenant resolves — but `ourios-server` hard-codes `TenantRule::service_name()`
and no config surface reaches it. The operator who *knows* about the
collision cannot deploy around it.

Settled premises from #688 that bind here: tenancy is the fused
isolation-and-partitioning unit ("the smallest blast radius of a
credential", Q1); tenancy metadata stays out of the OTLP data model —
derivation *interprets* producer-describing attributes, and no bespoke
in-band tenant stamp is ever trusted (Q2); the token remains the authority
and the derived tenant remains a claim checked against it (Q3); tenant ids
are opaque — no mechanical hierarchy (Q4); cross-tenant queries stay out
(Q3); repartitioning is rejected (Q5).

## 3. Proposed design

### 3.1 Configuration

```yaml
receiver:
  tenant:
    # Ordered resource-attribute keys; values join into the tenant id.
    # Default: [service.name] — today's behaviour, unchanged.
    rule: [k8s.cluster.name, service.name]
    # Keys watched for divergence (§3.4) when not already in `rule`.
    # Default: [k8s.cluster.name].
    watch: [k8s.cluster.name]
    # Upper bound on remembered (tenant, key) pairs (§3.4). Default: 10000.
    watch_capacity: 10000
```

- An empty `rule` list is a startup configuration error. A duplicate key in
  `rule` is a startup configuration error. A key listed in both `rule` and
  `watch` is accepted and simply not watched (§3.4). `watch_capacity` must
  be an integer ≥ 1 — `0` and negative values are startup configuration
  errors (an operator who wants no watching sets `watch: []`).
- Derivation is per-`ResourceLogs` group from `Resource.attributes`,
  unchanged in shape. **Every key in `rule` is required**: any group whose
  resource lacks a `rule` key, or carries it with a non-string or
  empty-string value, rejects the **whole export** — the existing RFC0003.4
  posture, and the RFC 0043 rule that an empty string is never a value.
  Partial joins are explicitly rejected as a design (§4): a group missing
  `k8s.cluster.name` that silently derived plain `fluxcd` would recreate the
  exact collision this RFC exists to close.
- `watch` keys are never required. A group that lacks a `watch` key, or
  carries it as a non-string or empty string, is simply not observed by the
  detector for that key; the export's acceptance is decided by `rule` alone.
  The detector observes, it never enforces (§3.4).

### 3.2 The join is injective, and the single-key case is byte-identical

A **single-key** rule (including the default `[service.name]`) derives the
tenant id as the attribute's string value, verbatim — exactly what
`TenantRule::service_name()` produces today. No escaping is applied: a
`service.name` of `a/b` stays tenant `a/b`, `100%` stays `100%`, so
existing storage paths and token bindings are untouched (RFC0045.6 covers
both characters).

A **composite** rule (two or more keys) percent-encodes `%` (as `%25`) and
`/` (as `%2F`) in each component value, then joins the components with
`/`. For a fixed rule, distinct component tuples therefore produce distinct
tenant ids: `("a", "b/c")` → `a/b%2Fc` and `("a/b", "c")` → `a%2Fb/c`
cannot merge. Injectivity is a per-rule property — within one epoch exactly
one rule is in force, so no two live resources can collide; the cross-epoch
case is §3.3.

The tenant id remains an opaque string to every downstream consumer (auth,
storage, query); the partition layer's existing `percent_encode_tenant`
makes any tenant id path-safe, so the on-disk layout needs no change. One
latent defect on that path does need fixing, and this RFC owns it: the
`Store` resolved keys with `ObjectPath::from`, which escapes the `%` of an
already-encoded tenant a second time (`a%2Fb` → `a%252Fb`), so the local
querier's `tenant_id=<enc>` join and the compactor's `percent_decode_tenant`
never found such a tenant's objects. Invisible while tenant ids were plain
`service.name` values; unavoidable once ids carry `/`. Keys are parsed
(stored verbatim) instead; RFC0045.2/.4 exercise the fix end-to-end.

*Legacy objects.* A pre-fix deployment whose `service.name` values contained
any character outside the unreserved set (`/`, `%`, `=`, `:`, space, …)
wrote that tenant's objects under the doubly-encoded key. Those objects
were already unreadable on the local backend and mis-attributed by the
compactor's `percent_decode_tenant`; on S3 they were readable only because
the writer and the remote read path shared the same double encoding. No
dual-read is built: this is a pre-release fix, no correct deployment could
have depended on the layout, and a read-side fallback would have to live in
every consumer forever. The fix ships as a conventional breaking change
(`fix(parquet)!`) whose note names the one-shot migration — rename the
`tenant_id=<double-encoded>` prefix to `tenant_id=<encoded>` (an object
copy on S3, a directory rename locally). Tenants whose ids are unreserved
throughout — every plain `service.name` — have identical keys before and
after.

### 3.3 Epoch semantics

Derivation happens at ingest, once. A rule change (config edit + restart)
affects newly ingested data only: stored files keep the tenant ids they
were written under, no repartitioning, no rewrite, no epoch qualifier in
the id or the storage key. **Tenant identity is the id string and nothing
else** (opaque ids, Q4): if a later rule derives an id that an earlier rule
also produced, those records are one tenant, intentionally — the same way
they would be if the rule had never changed. Records whose ids differ
across epochs (`fluxcd` before, `cluster1/fluxcd` after) are two tenants,
each queryable under its own id. Nothing happens to old data — with one
qualification, the WAL tail, which is the only place "derive once" needs
a mechanism.

**The WAL tail derives under the rule it was acknowledged under.**
Startup recovery (RFC 0001 §6.9 / RFC0014.5) replays every surviving WAL
frame through the tenant fan-out — un-flushed frames, plus frames a
floor-retained segment still holds. Re-deriving those under a *changed*
rule would either abort startup (a `rule` key the old frames never
carried) or, worse, silently re-tenant acknowledged records into the new
epoch's ids — a duplicate in `cluster1/fluxcd` for a record already stored
under `fluxcd`, and a miner tree fed twice. So the receiver persists a
**rule-epoch log** in the WAL root (`tenant_rule_epochs.json` — a sidecar
like the checkpoint file, not a new WAL frame kind; written
temp-file → rename → directory fsync): an ordered list of
`{rule, after}` entries meaning "frames with offset > `after` derive under
`rule`" (`after: null` = from the beginning). Replay picks each frame's
epoch by offset: the newest entry whose `after` lies strictly below the
frame. On startup, after replay and **before either listener is bound**
(so no frame can be acknowledged under the new rule until the entry is
durable), if the configured rule differs from the newest entry's rule, a
new entry is appended with `after` = the highest offset replay delivered.
Replay delivers every surviving frame, so a `None` there means the WAL
holds no frames at all; the log then collapses to the single entry
`{rule, null}` — nothing exists to attribute to earlier epochs — so only
the first entry is ever unbounded. Every WAL
offset is globally ordered (UUIDv7 segment, byte), so "which epoch" is one
comparison.

*Durability and validation.* The sidecar is written as temp file →
`fsync(file)` → `rename` → `fsync(directory)`, the same sequence as the
checkpoint file, so a crash leaves either the previous log or the new one,
never a torn file. On load the log must be an object with a non-empty
`epochs` array; every entry a valid rule (non-empty, no duplicate keys) and
the first with `after: null` and every later one with a
`{segment: UUID, byte}` offset; and successive `after` values
non-decreasing (an equal boundary is allowed — the later entry wins for
frames above it, which is what append order means).
Anything else, and an absent `epochs`, aborts startup naming the file
(corruption class, like a bad segment header). An absent *file* means one
implicit epoch, `{[service.name], null}` — every pre-RFC WAL is that epoch,
so the upgrade needs no migration. Entries are never pruned; a rule change
is rare and the file stays a few lines.

Read-time tenant *aliasing* (query tenant X also reads legacy tenant Y
through an explicit, audited mapping) is the named escape hatch if
S7-style demand materializes; it is out of scope here.

### 3.4 The divergence detector

For each key in `watch` that is not part of `rule`: per (tenant, key), the
receiver remembers the first observed value. When a later group for the
same tenant carries a *different* value, the receiver emits a rate-limited
warning naming the tenant, the key, and both values, and increments a
counter. The S2 misconfiguration — two clusters merging into one tenant
under a single-key rule — thereby announces itself on the first divergent
batch instead of corrupting silently. Ingest is never rejected by the
detector: it observes, it does not enforce (the operator may genuinely
intend one tenant spanning clusters).

**State bound.** The detector's memory is a map of at most
`receiver.tenant.watch_capacity` (tenant, key) entries — default 10 000 —
each holding the first-observed value. Admission is first-come: once the
map is full, new (tenant, key) pairs are not admitted and are not watched;
a single warning announces saturation (once per process lifetime), so an
un-watched tenant is a known, logged condition rather than a silent one.
No eviction — first-observed semantics have no meaningful "least recently
used" entry, and evicting would only trade one blind spot for another. The
bound is an entry count, not a byte budget: each entry holds the tenant id
(a string storage already holds per partition), the watched key (operator
config), a 64-bit digest and the length of the first value, and its ≤128-byte
preview — so the memory ceiling is `watch_capacity × (|tenant| + |key| +
~160 B)`. State resets on restart (documented; the detector is best-effort
by design): the first value seen after a restart becomes the new baseline,
so a divergence that straddles the restart is not announced — only a
divergence observed within one process lifetime is.

**Value representation.** Only non-empty string values are observed
(§3.1); non-string values never reach the detector, so nothing needs a
serialization. Comparison is exact: the detector keeps a 64-bit digest plus
the byte length of the first value and compares later values against both,
so two values that share a long common prefix are still told apart. What
is *stored for display and logged* is a preview bounded to 128 bytes —
longer values are truncated at a UTF-8 boundary and marked with a trailing
`…` — which caps both the memory per entry and the log line. The warning is
rate-limited per (tenant, key), so a persistently divergent tenant produces
one line per rate window, not one per batch. Redaction is
not applied: `watch` keys are operator-selected producer descriptors, and
selecting a key opts its values into the operator's own logs exactly as
selecting a `rule` key opts them into tenant ids and storage paths.

The counter's name and attributes are minted at implementation time through
the semconv registry + weaver process (provisional:
`ourios.tenant.watch_divergence`, attribute = the watched key; the tenant id
rides the warning log, not the metric, for cardinality). The OTel-naming
check happens then, per house rule.

### 3.5 Auth interaction — none

The derived tenant remains a claim checked against the token's tenant set
(RFC 0026 whole-batch binding; RFC 0029 resolution). Composite ids are
opaque strings to that machinery. A token authorizing `cluster1/fluxcd`
authorizes exactly that string; nothing about binding, rejection, or the
403 contract changes.

**Rollout under a rule change** follows from §3.3 and needs no mechanism:
a token naming `fluxcd` keeps authorizing exactly `fluxcd` — the old-epoch
tenant — and does not authorize `cluster1/fluxcd`. The operator issues (or
extends) tokens naming the new ids before or with the restart; until then,
exports deriving new ids are rejected by the unchanged binding check
(RFC0045.8) rather than silently landing somewhere. Whether old tokens are
revoked once the old-epoch data ages out is the operator's call.

## 4. Alternatives considered

- **Collector-side remapping** (OTTL rewriting `service.name` or stamping a
  synthetic attribute). Works per deployment, still available, but every
  deployment must know to do it, and rewriting `service.name` corrupts its
  semantics. The backend doing the composite once is less operational
  surface.
- **A bespoke trusted in-band `tenant` attribute.** Rejected per #688 Q2:
  OTLP deliberately has no tenant field; an injected stamp trusted on
  arrival is routing metadata smuggled into the data model. (An operator
  may still *point the rule at* such an attribute — it is then a claim like
  any other, bound by the token.)
- **Skip-missing-keys joining.** Rejected: a partial join silently
  reproduces the collision under exactly the conditions (heterogeneous
  resource attributes) where the operator most needs the strictness.
- **Mechanical hierarchy** (prefix queries over `cluster1/…`). Deferred per
  Q4: the separator is a social convention; ids are opaque.
- **Repartitioning on rule change.** Rejected per Q5: a data-rewriting
  migration for a config edit inverts the risk profile of the entire
  design.
- **Replaying the WAL tail under the new rule** (no epoch log; document
  "drain before you change the rule"). Rejected: replay delivers
  floor-retained frames even after a clean shutdown, so the procedure
  cannot be made airtight, and the failure is either a startup abort or a
  silent re-tenanting of acknowledged data — the second is exactly the
  §3.7 class this RFC exists to close. Stamping the tenant id or rule into
  each WAL frame was the other option; it changes the RFC 0008 frame
  format for a once-per-deployment event, where a sidecar keyed by offset
  does not.

## 5. Acceptance criteria

Scenario ids `RFC0045.<n>`.

> **RFC0045.1 — config resolution.** Given a config with no
> `receiver.tenant` section, When the server starts, Then derivation uses
> `[service.name]`; Given `rule: []`, Then startup fails with a
> configuration error; Given `rule: [service.name, service.name]`, Then
> startup fails with a configuration error; Given `watch_capacity: 0` (or a
> negative or non-integer value), Then startup fails with a configuration
> error.

> **RFC0045.2 — the S2 scenario end-to-end.** Given
> `rule: [k8s.cluster.name, service.name]` and two exports whose resources
> share `service.name: fluxcd` but differ in `k8s.cluster.name`
> (`cluster1`, `cluster2`), When both are ingested and queried, Then two
> tenants `cluster1/fluxcd` and `cluster2/fluxcd` exist, each query returns
> only its own records, and no record is reachable from the other tenant.

> **RFC0045.3 — strict missing-key rejection.** Given the composite rule
> and a group whose resource lacks `k8s.cluster.name` (or carries it as a
> non-string or empty string), When the export is ingested, Then the whole
> export is rejected with the same posture as today's missing
> `service.name`, and nothing reaches the WAL.

> **RFC0045.4 — join injectivity.** Given `rule` of two keys and two
> exports with component tuples `("a", "b/c")` and `("a/b", "c")`, When
> both are ingested, Then they land in two distinct tenants and each is
> queryable only under its own id.

> **RFC0045.5 — epoch semantics.** Given records ingested under the default
> rule, When the server restarts with the composite rule and further
> records are ingested, Then the old records remain queryable under their
> original tenant, the new records under the composite tenant, and no
> stored file was rewritten; And Given a later epoch derives an id the
> earlier epoch also produced, Then a query for that id returns records
> from both epochs — one tenant, per §3.3.

> **RFC0045.6 — default regression.** Given no `receiver.tenant` config,
> When the existing RFC 0003 tenancy suite runs, Then it passes unchanged —
> derivation is byte-identical to the pre-RFC behaviour; And Given a
> single-key rule and a `service.name` of `a/b` (and of `100%`), Then the
> derived tenant is exactly `a/b` (`100%`) — no escaping on the single-key
> path.

> **RFC0045.7 — divergence detector.** Given the default rule and default
> `watch`, When two exports share `service.name` but differ in
> `k8s.cluster.name`, Then a warning naming the tenant, key, and both
> values is emitted and the divergence counter increments; And Given
> uniform `k8s.cluster.name` values, Then no warning and no increment; And
> Given a group lacking `k8s.cluster.name` (or carrying it non-string or
> empty), Then the export is accepted and that group is not observed; And
> Given a divergent value longer than 128 bytes, Then the warning carries
> the value truncated at a UTF-8 boundary with a trailing `…`; And Given
> two values that agree on their first 128 bytes and differ after, Then the
> divergence is still detected and counted.

> **RFC0045.8 — auth binding unchanged.** Given auth enabled with a token
> bound to `cluster1/fluxcd` and the composite rule, When an export
> deriving `cluster2/fluxcd` is presented under that token, Then the whole
> batch is rejected per the RFC 0026 contract, with unchanged telemetry.

> **RFC0045.9 — watch state bound.** Given `watch_capacity: 1` and two
> tenants that each later diverge on `k8s.cluster.name`, When both are
> ingested, Then the first tenant's divergence is reported, the second
> tenant's is not, the saturation warning is emitted exactly once, and
> every export is accepted.

> **RFC0045.10 — WAL tail keeps its epoch.** Given records acknowledged
> under the default rule whose frames are still in the WAL — un-flushed
> after a crash, or retained after a clean shutdown — When the server
> restarts with the composite rule, Then recovery derives those frames
> under `[service.name]` — they land only in their original tenant, no
> duplicate exists in any composite tenant, startup succeeds even though
> the frames lack `k8s.cluster.name`, and the epoch log gains one entry;
> And Given no epoch log exists beside a pre-RFC WAL, Then replay behaves
> as a single `[service.name]` epoch; And Given an epoch log that is
> unparseable, has no entries, or whose `after` boundaries go backwards,
> Then startup aborts naming the file.

## 6. Testing strategy

Unit tests in `ourios-ingester` for the rule (single-key verbatim,
composite encode + join, missing/empty/non-string rejection, injectivity
pairs) and for the detector (first-value memory, divergence, watch-key
absence, truncation, capacity admission); a `proptest` over component
tuples asserting the composite join is injective for a fixed key count
(RFC0045.4 in property form). Config resolution (RFC0045.1) as
`FileConfig` unit tests. RFC0045.2/.3/.5/.8 as `ourios-server` integration
tests through the served OTLP → query path, reusing the RFC 0003 / RFC 0026
harnesses; RFC0045.6 is the existing suite plus two rule-level cases.
RFC0045.7/.9 assert on captured `tracing` output and the counter, in the
pattern the RFC 0026 telemetry tests use. RFC0045.10 extends the RFC0014.5
crash/replay harness (ingest → kill before flush → restart with a
different rule) plus unit tests for the epoch log's parse (including the
rejection cases), append, and by-offset lookup; the retained-after-clean-
shutdown arm is the served-binary RFC0045.5 sequence, where the phase-1
frame (which carries the composite keys) must not reappear as a duplicate
in the composite tenant after the rule change — a re-derivation at replay
would put it there.

## 7. Open questions

- [ ] **Counter final name** — `ourios.tenant.watch_divergence` is
      provisional; minted through the semconv registry + weaver at
      implementation, with the OTel-MCP naming check.
- [ ] **Saturation visibility** — a once-per-lifetime warning is the
      minimum; whether the admitted-entry count deserves a gauge is
      decided when the counter is minted (same registry pass).

## 8. References

- #688 — the tenancy concept discussion; Q1–Q10 are this RFC's premises
  (Q1–Q7 in the issue body, Q8–Q10 in its comment thread).
- RFC 0001 §6.1 — the reserved tenant-derivation rule this RFC exposes.
- RFC 0003 §6.3 / RFC0003.4 — per-`ResourceLogs` derivation and
  whole-export rejection.
- RFC 0005 §3.4 — `percent_encode_tenant`, the path-safety layer.
- RFC 0026 / RFC 0029 — whole-batch binding and token resolution the
  derived tenant is checked against.
- RFC 0043 — the empty-string-is-never-a-value rule.
- OTel semantic conventions, `service.name` — uniqueness scoped to
  `service.namespace`; k8s attribute derivation chain.
- `CLAUDE.md` §3.7 — the multi-tenancy invariant this RFC defends.

## 9. Deferred (recorded, not built)

Read-time tenant aliasing (Q5 escape hatch); visibility classes within a
tenant (Q8 — the query-rewrite layer); conversation-scoped erasure (Q9);
the ReBAC/OpenFGA resolver (#688 spike: viable as a third `AuthResolver`,
operational costs to weigh in its own RFC); mechanical hierarchy (Q4).
