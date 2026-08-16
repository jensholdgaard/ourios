---
rfc: 0045
title: Operator-configured composite tenant derivation
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-08-17
supersedes: —
superseded-by: —
---

# RFC 0045 — Operator-configured composite tenant derivation

> **Status: `specified` (2026-08-17).** §5 criteria written and testable.
> Grounded in the tenancy concept discussion (#688): the maintainer-settled
> Q1–Q10 answers are this RFC's premises, restated in §2/§3 where they bind.

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
```

- An empty `rule` list is a startup configuration error. A duplicate key in
  `rule` is a startup configuration error.
- Derivation is per-`ResourceLogs` group from `Resource.attributes`,
  unchanged in shape. **Every configured key is required**: any group whose
  resource lacks a key, or carries it with a non-string or empty-string
  value, rejects the **whole export** — the existing RFC0003.4 posture, and
  the RFC 0043 rule that an empty string is never a value. Partial joins are
  explicitly rejected as a design (§4): a group missing `k8s.cluster.name`
  that silently derived plain `fluxcd` would recreate the exact collision
  this RFC exists to close.

### 3.2 The join is injective

Each component value percent-encodes `%` (as `%25`) and `/` (as `%2F`)
before the components join with `/`. Distinct component tuples therefore
produce distinct tenant ids: `("a", "b/c")` → `a/b%2Fc` and `("a/b", "c")`
→ `a%2Fb/c` cannot merge. The tenant id remains an opaque string to every
downstream consumer (auth, storage, query); the partition layer's existing
`percent_encode_tenant` already makes any tenant id path-safe, so no
storage change is required.

### 3.3 Epoch semantics

Derivation happens at ingest, once. A rule change (config edit + restart)
affects newly ingested data only: stored files keep the tenant ids they
were written under, no repartitioning, no rewrite. Both epochs remain
independently queryable under their own ids. This is documented operator
guidance, not mechanism — the mechanism is precisely that nothing happens
to old data. Read-time tenant *aliasing* (query tenant X also reads legacy
tenant Y through an explicit, audited mapping) is the named escape hatch if
S7-style demand materializes; it is out of scope here.

### 3.4 The divergence detector

For each key in `watch` that is not part of `rule`: per (tenant, key), the
receiver remembers the first observed value (bounded in-memory state, reset
on restart — documented). When a later group for the same tenant carries a
*different* value, the receiver emits a rate-limited warning naming the
tenant, the key, and both values, and increments a counter. The S2
misconfiguration — two clusters merging into one tenant under a single-key
rule — thereby announces itself on the first divergent batch instead of
corrupting silently. Ingest is never rejected by the detector: it observes,
it does not enforce (the operator may genuinely intend one tenant spanning
clusters).

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

## 5. Acceptance criteria

Scenario ids `RFC0045.<n>`.

> **RFC0045.1 — config resolution.** Given a config with no
> `receiver.tenant` section, When the server starts, Then derivation uses
> `[service.name]`; Given `rule: []`, Then startup fails with a
> configuration error; Given `rule: [service.name, service.name]`, Then
> startup fails with a configuration error.

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
> stored file was rewritten.

> **RFC0045.6 — default regression.** Given no `receiver.tenant` config,
> When the existing RFC 0003 tenancy suite runs, Then it passes unchanged —
> derivation is byte-identical to the pre-RFC behaviour.

> **RFC0045.7 — divergence detector.** Given the default rule and default
> `watch`, When two exports share `service.name` but differ in
> `k8s.cluster.name`, Then a warning naming the tenant, key, and both
> values is emitted and the divergence counter increments; And Given
> uniform `k8s.cluster.name` values, Then no warning and no increment.

> **RFC0045.8 — auth binding unchanged.** Given auth enabled with a token
> bound to `cluster1/fluxcd` and the composite rule, When an export
> deriving `cluster2/fluxcd` is presented under that token, Then the whole
> batch is rejected per the RFC 0026 contract, with unchanged telemetry.

## 6. Telemetry

The §3.4 warning and counter are the only additions. The counter is minted
via `semconv/registry/` + weaver at implementation; the live-check gate
covers it like every other emission.

## 7. Deferred (recorded, not built)

Read-time tenant aliasing (Q5 escape hatch); visibility classes within a
tenant (Q8 — the query-rewrite layer); conversation-scoped erasure (Q9);
the ReBAC/OpenFGA resolver (#688 spike: viable as a third `AuthResolver`,
operational costs to weigh in its own RFC); mechanical hierarchy (Q4).
