---
rfc: 0004
title: Configuration policy — tunables vs invariants
status: green
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-05-18
supersedes: —
superseded-by: —
---

# RFC 0004 — Configuration policy: tunables vs invariants

## 1. Summary

Ourios exposes a small, deliberately bounded configuration surface
to its operators. This RFC pins the line between *tunables* — knobs
that can be set globally and overridden per tenant — and
*invariants* — the `CLAUDE.md` §3 commitments that define what
Ourios *is*. Tunables let an organisation place themselves on the
accuracy-vs-compression spectrum without taking the whole product
with them. Invariants are not configurable — every tenant gets the
same `[§3]` guarantees, no matter what. The RFC names the current
four tunables, the boundary they sit inside, and the escalation
path for anyone who wants to cross it.

## 2. Motivation

### 2.1 Different organisations sit at different points

Dev clusters care about cheap ingest and aggressive compression and
tolerate noisier templates. Production caps the noise and pays the
storage. Some customers run high-cardinality logging from legacy
apps; others run carefully structured loggers. A backend that
bakes one trade-off into the algorithm is rigid and harder to
adopt; a backend that lets users tune the trade-off *within* a
guaranteed safety net is exactly Ourios' thesis-shaped use case.

### 2.2 But the safety net is the product

`CLAUDE.md` §1 lists what Ourios is and is not. `CLAUDE.md` §3
lists the load-bearing invariants — strict thresholds, no
unbounded `params`, bit-identical reconstruction, WAL-before-ack,
schema migrations through RFC, single-source-of-truth in object
storage, multi-tenancy from day one. Each of those is the answer
to a specific failure mode (silent template merges, cardinality
blow-ups, lossy reconstruction, lost acked data, ...). If any of
them is configurable per tenant, the *product* becomes
configurable per tenant: query semantics, audit trail, storage
guarantees all vary based on a knob a future operator forgot they
flipped. The cognitive surface alone is a hazard.

### 2.3 Why pin this in an RFC

The boundary is a recurring question (it has already come up in
maintainer discussion 2026-05-18; see `docs/roadmap.md` §5 for the
Perses-integration variant of the same instinct). Pinning the
two-class model now means:

- New PRs that propose a tunable can be reviewed against a written
  rule rather than a half-remembered convention.
- Future RFCs that want to break an invariant know they need a
  `meta:` RFC (per `CLAUDE.md` §6.2 precedent), not a runtime
  toggle.
- Contributors reading the `MinerConfig` rustdoc see the
  *category* of each knob, not just its type.

## 3. Proposed design

### 3.1 Two-class model

Every operator-visible knob is **exactly one** of:

- **Tunable.** Configurable globally; overridable per tenant.
  Validated at process startup; tenants whose override fails
  validation never serve traffic (RFC 0001 §3.2.2 already pins
  this contract for `param_byte_limit`; this RFC generalises it
  to all tunables).
- **Invariant.** Not configurable. The same value applies to
  every tenant. Encoded as an algorithmic property of the code,
  not a field on `MinerConfig`. A change requires an RFC against
  `CLAUDE.md` §3; a *waiver* requires a `meta:` RFC.

There is no third category. A "default but overridable in
production" knob is a tunable; a "default for now, may make
configurable later" knob is an invariant — *configurability is
opt-in*, never an implicit consequence of "we exposed a field."

### 3.2 The current tunables (four)

These are the knobs `MinerConfig` exposes, with the current
defaults and the RFC §3 invariant each lives inside:

| Tunable | Default | Validated range | Inside invariant |
|---|---|---|---|
| `similarity_threshold` | `0.7` (RFC 0001 §3.1.1) | `(0, 1]` | §3.1 — strict-by-default, RFC required to *change the default below 0.7* |
| `similarity_floor` | `0.4` (RFC 0001 §6.3) | `(0, similarity_threshold]` | §3.1 — bounds the §6.3 lossy zone; body retention in that zone is invariant |
| `prefix_depth` | `2` (Drain paper §3.2) | `0..=8` (RFC 0001 §6.1 — "configurable cap of ~8 is the realistic ceiling") | §3.1 — affects tree quality, not safety |
| `param_byte_limit` | `256` (RFC 0001 §3.2.1) | `1..=1024` (`PARAM_BYTE_LIMIT_CEILING`, RFC 0001 §3.2.2) | §3.2 — bounds cardinality; overflow spilling is invariant |

The §3 invariant column is load-bearing: a tunable that walks
outside its validated range is rejected at startup, *not* mapped
to a clamped value, because clamping silently moves a tenant onto
a trade-off point the operator didn't pick.

### 3.3 The invariants (not tunable)

These come from `CLAUDE.md` §3 and RFC 0001 §6.1 / §6.4 / §6.6 —
they're enforced in code, not exposed as fields:

- **Widening fires on every Fixed mismatch** with a `TemplateWidened`
  audit event (§6.4). There is no `allow_widening` toggle; turning
  off widening means turning off the §3.1 audit signal and the
  miner's compression story together. If a tenant doesn't want
  template merging, they shouldn't use a template-mining backend.
- **`severity_number` and `scope_name` are part of the §6.1
  template-key composition.** There is no `respect_severity`
  toggle; merging INFO and ERROR `"user logged in"` records is
  hazard H1.4 by construction.
- **Body is retained on every §6.3 lossy-zone and parse-failure
  attach.** There is no `LossyMode::Aggressive` toggle; `CLAUDE.md`
  §3.1 reads "MUST retain the original body. No exceptions."
- **Reconstruction is bit-identical** on every record with
  `lossy_flag = false`. There is no `accept_lossy_reconstruction`
  toggle; `CLAUDE.md` §3.3 reads "rendering ... must equal the
  original line byte for byte, or the line must be flagged
  lossy."
- **Mining is per-tenant.** There is no `enable_cross_tenant_dedup`
  toggle; `CLAUDE.md` §3.7 reads "every code path that touches
  data takes a tenant ID."

The list is closed in the sense that *any* new knob that touches
one of these areas is an invariant proposal, not a tunable
proposal — the PR adding it goes through the §6 RFC process, not
review.

### 3.4 Per-tenant override mechanism

`MinerConfig` is `Clone + Copy + 'static` and its docstring
already says "per-tenant miner configuration." The cluster holds
a **cluster default** plus an **optional per-tenant override**;
overrides are seeded before the tenant is first observed (or
default-resolved at lazy `TenantState` allocation when no override
exists). The algorithm code reads `&MinerConfig` from `TenantState`
on every ingest — no global flag, no implicit "current tenant."

Implementation detail (specified in the follow-up PR, not this
RFC): seeding API on `MinerCluster` is `with_tenant_config(
tenant_id, config)` or equivalent; the lookup is `state.config`
inside the per-tenant store the cluster already maintains. No
hot-path overhead beyond the existing `&self.config` deref.

### 3.5 Escalation path

If a future RFC proposes promoting an invariant to a tunable, the
escalation is:

1. A `meta:` RFC against `CLAUDE.md` §3 explaining why the
   invariant should no longer be load-bearing. Majority maintainer
   approval (the precedent is `CLAUDE.md` §6.2's 2026-05-13
   amendment).
2. Only after the `meta:` RFC accepts does the implementation RFC
   propose the `MinerConfig` field and the validation bounds.

Going the other direction — promoting a tunable to an invariant —
follows the same path: the `meta:` RFC justifies the loss of
flexibility, the implementation RFC removes the field.

This is the *only* path. A PR that adds a "small, just-for-now"
field that touches an invariant area is rejected.

## 4. Alternatives considered

### 4.1 Single flat config bag

Stuff everything (tunables + algorithmic constants) into one
`Config` struct with no internal classification. Rejected: the
cognitive surface concern in §2.2 — readers can't see at a glance
which fields are safe to override. Future PRs that add knobs
have no anchored rule to be reviewed against.

### 4.2 Inline classification on each field via a marker trait

Tag each field with `Tunable` or `Invariant` via a Rust trait.
Rejected: invariants aren't fields at all — they're algorithmic
properties (widening fires, severity participates in the key,
body retains). Marking them as fields-with-a-trait would imply
the field is the source of truth, which it isn't. The closed-set
rustdoc in §3.2 / §3.3 is a stronger contract than a marker.

### 4.3 A `DrainConfig` separate from `MinerConfig`

External LLM proposal 2026-05-18 (Grok session — link in
maintainer's memory under `reference_grok-design-conversations`).
Rejected: `MinerConfig` already exists and already covers three
of the four tunables. A second config type duplicates the
validation surface, splits the per-tenant override mechanism, and
introduces a new boundary type to maintain. The naming convention
"`<subsystem>Config` is the tunables surface, invariants live in
code" is the simpler shape.

### 4.4 RFC the implementation, not the policy

Skip this RFC; let the implementation PR add `prefix_depth` to
`MinerConfig`. Rejected: the boundary keeps coming up
(`docs/roadmap.md` §5 Perses row, Grok DrainConfig, future CRD
proposals); a one-shot implementation PR doesn't give those
recurrences an anchor to be reviewed against. The RFC is the
artifact, the PR is the action.

## 5. Acceptance criteria

> **Scenario RFC0004.1 — Every tunable validates at startup**
> - **Given** a `MinerConfig` constructed via `try_new_full` with
>   a value outside the §3.2 ranges for any field
> - **When** the constructor is called
> - **Then** it returns `Err(MinerConfigError::*)` naming the
>   offending field
> - **And** no `MinerConfig` instance is produced

> **Scenario RFC0004.2 — Per-tenant override is honoured**
> - **Given** a `MinerCluster` with a default `MinerConfig` and a
>   per-tenant override for tenant `T` that differs from the
>   default in at least one tunable
> - **When** tenant `T` ingests a line that exercises the differing
>   knob's decision boundary
> - **Then** the cluster's behaviour matches the per-tenant override,
>   not the default

> **Scenario RFC0004.3 — No invariant-breaking field exists**
> - **Given** the `MinerConfig` type as defined by this RFC
> - **When** `cargo doc` is rendered or the type is grep'd in CI
> - **Then** there is no `allow_widening`, `respect_severity`,
>   `lossy_mode`, `enable_cross_tenant_dedup`, or
>   `accept_lossy_reconstruction` field — adding one is a
>   compile-time visible change that fails this scenario
> - **And** the implementation PR adds a test that pins the
>   tunable-set against this RFC

## 6. Testing strategy

- **RFC0004.1** — exhaustive unit tests on `try_new_full` per
  failure variant (one test per `MinerConfigError` arm). Already
  partially in place; the follow-up implementation PR adds the
  `PrefixDepthTooLarge` variant + test.
- **RFC0004.2** — integration test in `crates/ourios-miner/tests/`
  ingesting the same line through two tenants with different
  `similarity_threshold`s and asserting different
  template-allocation outcomes.
- **RFC0004.3** — a "tunable-set pin" test that uses a `match`
  against `MinerConfig`'s public fields (exhaustive on a struct
  pattern); adding a new field forces the test author to think
  through which side of the boundary it sits on, and reviewers
  see the change as part of the RFC against §3.

## 7. Open questions

- [ ] Should the per-tenant override mechanism allow *dynamic*
  reconfiguration (operator API at runtime), or only at startup?
  RFC defers to the implementation PR's preference; current
  proposal is startup-only because `TenantState` is allocated
  lazily and config is captured at allocation.
- [ ] Does the documentation route stop at `MinerConfig`'s rustdoc,
  or does it also need a page under `docs/architecture/`? Defer
  until the implementation PR lands.

## 8. References

- `CLAUDE.md` §1 (project charter), §3 (invariants), §3.7
  (multi-tenancy from day one), §5.1 (RFC process), §6.2 (tests
  as specifications, 2026-05-13 `meta:` amendment).
- RFC 0001 §3.1.1 (`similarity_threshold` default), §3.2.1
  (`param_byte_limit` default), §3.2.2 (startup rejection
  contract), §6.1 (template-key composition, prefix-depth cap),
  §6.3 (three-zone model + floor default), §6.4 (widening +
  audit), §6.6 (reconstruction).
- `docs/roadmap.md` §5 (deliberately-out-of-MVP table — Perses
  row is a related "is/is-not" discussion).
- `docs/hazards.md` H1 (silent merges), H2 (cardinality blow-up),
  H7 (reconstruction).
- Drain paper §3.2 (prefix tree, prefix-depth convention).
