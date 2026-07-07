---
rfc: 0026
title: Authentication and tenant binding (ingest + query)
status: accepted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-05
supersedes: —
superseded-by: —
---

# RFC 0026 — Authentication and tenant binding (ingest + query)

## 1. Summary

Ourios's multi-tenancy is structural but **unauthenticated**. The
ingest side derives `tenant_id` from resource attributes the *sender*
controls (RFC 0003 §6.3), and the query side takes the
`x-ourios-tenant` header on faith (RFC 0016 shipped deliberately as
"trusted-network for v1; authn as a follow-up RFC"). Any client that
can reach either listener can write into and read from **any**
tenant. RFC 0003 §9 has carried the open question — "does the
authenticated identity feed into the `tenant_id` derivation?" — since
the receiver landed. This RFC is that follow-up:

1. **Ingest authn** — static bearer tokens on the OTLP listeners
   (gRPC metadata / HTTP `Authorization`), configured through the
   RFC 0020 config file with `${env:VAR}` substitution so secrets
   never live in the file.
2. **Query authn** — the same token mechanism on the RFC 0016 HTTP
   API (and thereby everything layered on it, e.g. RFC 0027).
3. **Tenant binding (authz)** — each token carries an allowed tenant
   set. Ingest: the RFC 0003 §6.3 attribute-derived tenant must fall
   inside the token's set, else the batch is rejected before the WAL
   ack. Query: the `x-ourios-tenant` header must fall inside the
   token's set. This closes RFC 0003 §9: identity **constrains**
   derivation rather than replacing it.

Transport encryption stays delegated (TLS termination at the
operator's proxy, per the existing posture); this RFC is identity
and scoping, not channels.

## 2. Motivation

- **The gap is now user-facing.** The tester-recruitment push invites
  people to run Ourios beyond localhost; the first shared deployment
  turns "structural tenancy" into "no tenancy" — sender-controlled
  attributes choose the tenant, so isolation is cooperative, not
  enforced. §3.7 ("multi-tenancy is not bolted on") demands the
  enforcement half before exposure, not after.
- **Two RFCs already point here.** RFC 0003 §9 (identity → tenant
  derivation) and RFC 0016 §1 (authn follow-up) both deferred to an
  authentication RFC. Leaving the question open now blocks RFC 0027
  (an MCP surface productizes remote query access) and the Helm
  chart's security story (workstream C).
- **Ack semantics make ingest authz special.** Rejection must happen
  *before* the WAL ack (§3.4): once acknowledged, data is durable —
  an unauthorized batch must never reach that point.

## 3. Design

### 3.1 Token store (RFC 0020 config amendment)

A new top-level `auth` section:

```yaml
auth:
  tokens:
    - name: edge-collector          # audit/metric label, not secret
      token: ${env:OURIOS_TOKEN_EDGE}
      tenants: ["acme", "globex"]   # explicit allow-list
    - name: admin-cli
      token: ${env:OURIOS_TOKEN_ADMIN}
      tenants: ["*"]                # wildcard: all tenants
```

- Tokens are opaque strings, compared in constant time; the config
  holds them only via `${env:...}` indirection (the RFC 0020
  substitution engine), so files stay committable.
- `tenants` is an exact-string allow-list or the single wildcard
  `"*"`. No patterns — pattern semantics on a security boundary
  invite grief; revisit only with demand (§7).
- **No `auth` section ⇒ open mode**, preserving today's behavior for
  local/dev, with a structured startup warning naming the exposure.
  An *empty* `auth.tokens` list is a startup configuration error
  (locked-out server is never the intent).

### 3.2 Ingest enforcement (RFC 0003 amendment)

- Both OTLP listeners (gRPC + HTTP) require
  `Authorization: Bearer <token>` when auth is enabled. Missing or
  unknown token ⇒ gRPC `UNAUTHENTICATED` / HTTP 401 before any
  decode work.
- Per-batch authz: every `ResourceLogs` group's derived tenant
  (RFC 0003 §6.3, unchanged) is checked against the token's set.
  Any out-of-set tenant rejects the **whole batch** with
  `PERMISSION_DENIED` / 403 **before the WAL append** — partial-batch
  acceptance would make the OTLP partial-success surface a tenancy
  oracle, and §3.4 forbids acking anything not durably accepted.
- The RFC 0003 §9 question resolves as: derivation stays
  attribute-based (the sender's resource attributes remain the
  source of truth for *which* tenant), and identity bounds the set
  of tenants a sender may speak for. A token pinned to one tenant
  is the single-tenant-sender case; no attribute rewriting.

### 3.3 Query enforcement (RFC 0016 amendment)

- The HTTP query API requires the same bearer scheme. Status
  contract: missing/unknown bearer ⇒ 401; missing or empty
  `x-ourios-tenant` ⇒ 400 (today's contract, unchanged — the header
  stays the tenant selector); a well-formed tenant outside the
  token's set ⇒ 403.
- Enforcement composes with — never replaces — the structural
  scoping: the querier still roots every scan under the tenant's
  partition directory (RFC0007.5). The failure bound is worth
  stating precisely: a fail-open authz bug would re-open the
  pre-RFC exposure (any tenant selectable by header for an
  authenticated caller) — a real regression — but the structural
  scoping still confines each request to the single tenant it
  names; no bug in this layer yields cross-tenant reads within one
  query or unscoped scans.

### 3.4 Telemetry and audit

- Rejections count on the existing request counters with
  `error.type` (`unauthenticated` | `permission_denied`) — no new
  metric names (OTel recording-errors convention; new attributes go
  through the weaver registry).
- Ingest authz rejections additionally emit an audit event carrying
  the token *name* (never the token) and the offending tenant —
  cross-tenant write attempts are exactly what an operator audits.

## 4. Alternatives considered

- **mTLS as the identity mechanism.** Delegating TLS to a fronting
  proxy is the project's posture; client-cert identity does not
  survive typical proxy hops without header-forwarding conventions
  that are themselves a trust decision. Bearer tokens work through
  every OTLP exporter and HTTP client today. mTLS remains available
  *at* the proxy layer, orthogonal to this RFC.
- **JWT / OIDC.** Brings expiry, issuers, key rotation, clock
  dependence, and a validation dependency tree — for a system whose
  senders are collectors with static config. Static tokens match the
  OTel Collector ecosystem's operational reality (`headers:` on the
  OTLP exporter). An IdP integration can layer on later (§7) without
  changing the tenant-binding model.
- **Identity-derived tenancy (token ⇒ tenant, ignore attributes).**
  Breaks the multi-tenant-collector case (one edge collector
  forwarding many teams' telemetry) and silently discards the
  RFC 0003 §6.3 contract. Constraining beats replacing.
- **Per-tenant listeners / network policy as authz.** Pushes tenancy
  into deployment topology; contradicts the single-binary shape and
  makes the Helm chart combinatorial.

## 5. Acceptance criteria

Scenario ids `RFC0026.<m>`.

> **Scenario RFC0026.1 — token store configuration.** Given a config
> with an `auth.tokens` list using `${env:VAR}` values, When the
> server starts, Then tokens resolve through the RFC 0020
> substitution engine; an empty `auth.tokens` list is a startup
> configuration error; a missing `auth` section starts in open mode
> and emits a structured startup warning naming the exposure.

> **Scenario RFC0026.2 — ingest authentication.** Given auth
> enabled, When an OTLP export arrives with a missing or unknown
> bearer token (gRPC metadata and HTTP `Authorization`, both
> listeners), Then it is rejected (`UNAUTHENTICATED` / 401) before
> wire decode, nothing reaches the WAL, and no ack is returned.

> **Scenario RFC0026.3 — ingest tenant binding.** Given a token
> bound to tenants `{a, b}`, When a batch whose derived tenants are
> all within `{a, b}` arrives, Then it is accepted and acked
> normally; When a batch containing any `ResourceLogs` group deriving
> to a tenant outside the set arrives, Then the **whole batch** is
> rejected (`PERMISSION_DENIED` / 403) with **no WAL append and no
> partial success** — nothing of the batch becomes durable.

> **Scenario RFC0026.4 — query enforcement and status contract.**
> Given auth enabled, Then the query API returns 401 for a
> missing/unknown bearer, 400 for a missing or empty
> `x-ourios-tenant` (today's contract, unchanged), 403 for a
> well-formed tenant outside the token's set, and correct results
> for an in-set tenant — with the drift endpoint under the same
> gate.

> **Scenario RFC0026.5 — wildcard binding.** Given a token with
> `tenants: ["*"]`, When it ingests to and queries arbitrary
> tenants, Then both paths behave as if every tenant were listed.

> **Scenario RFC0026.6 — open-mode parity.** Given no `auth`
> section, When the full existing ingest + query acceptance suites
> run, Then behavior is byte-for-byte today's (the amendment is
> invisible until configured), warning aside.

> **Scenario RFC0026.7 — rejection telemetry and audit.** Given
> authn/authz rejections on either path, Then the existing request
> counters increment with `error.type`
> (`unauthenticated` / `permission_denied`) and an ingest authz
> rejection emits an audit event carrying the token *name* and the
> offending tenant — and never any token value, on any surface
> (metrics, audit, logs, errors).

### 5.1 Discharge record (green, 2026-07-06)

- **RFC0026.1** — #390 (token store: config schema, `${env:…}`-only
  secrets, startup error/warning arms) + #395 (store moved to
  `ourios_core::auth` for the ingest enforcement point).
- **RFC0026.2/.3** — #398: bearer authn before wire decode on both
  listeners (gRPC interceptor / HTTP handler), whole-batch tenant
  binding before the WAL append, served-stack gRPC arm; WAL emptiness
  asserted on the journal.
- **RFC0026.4/.5/.6** — #408: the 401→400→403 gate order pinned with
  exact bodies, wildcard binding on both halves, open-mode parity
  (exactly-once warning + a live listener connection).
- **RFC0026.7** — #409: rejections on the existing counters via
  `error.type` (`unauthenticated` | `permission_denied`; the query
  histogram under the new `rejected` kind member), and the
  `ingest_denied` audit event (kind 8, `denied_token_name` column —
  §3.7 additive-OPTIONAL, schema pin updated) with a no-token-value
  sweep across every surface.

### 5.2 Validation record (validated, 2026-07-07)

Run: `scratch/validation/rfc0026-0027-validate.sh` — the release
binary served over real sockets with a file config whose tokens
resolve through `${env:…}` (RFC 0020), two tokens (tenant-bound +
wildcard), both roles + MCP enabled. 16/16 checks pass:

- **Ingest matrix** (OTLP/HTTP): no bearer 401, unknown bearer 401,
  out-of-set tenant 403, in-set 200, wildcard-to-arbitrary-tenant 200.
- **Query matrix**: no bearer 401, missing tenant 400 (valid bearer),
  out-of-set 403, in-set 200.
- **Denial audit**: the `ingest_denied` event is durable in the
  store's audit Parquet (event-type string present in the flushed
  files) after the cadence/shutdown flush.
- **End-to-end data flow**: rows ingested under a valid token survive
  a graceful restart (the RFC 0014 drain) and serve on both query
  surfaces.

## 6. Testing strategy

RFC0026.1 in `ourios-server` config tests (the RFC 0020 suite's
home); .2/.3 as receiver integration tests against both listeners
(the RFC 0003 suite pattern), asserting WAL emptiness on rejection;
.4/.5 in the querier-role HTTP tests (RFC 0016 suite pattern); .6
runs the existing suites under a no-`auth` config — parity is the
assertion; .7 through the in-memory OTel reader (the established
telemetry-test pattern) plus the audit-sink test fixtures. Token
comparison is constant-time by construction (a dedicated comparison
helper with a unit test on the API shape, not a timing measurement —
timing assertions in CI are noise).

## 7. Open questions

1. **Token rotation ergonomics.** Config reload vs restart; whether
   two tokens per name (old + new) is worth first-class support.
2. **IdP / OIDC layering.** If demanded, a validator that maps a
   verified claim to the same `(name, tenants)` shape — the binding
   model is designed to be the stable layer.
3. **Tenant patterns.** Prefix grants (`team-*`) if explicit lists
   prove operationally painful; needs careful semantics before any
   wildcard beyond `"*"`.
4. **Rate limiting per token.** Adjacent concern; deliberately out
   of scope here.

## 8. References

- RFC 0003 §6.3 (tenant derivation) and §9 (the open question this
  closes), RFC 0016 §1/§3 (the query API and its authn deferral),
  RFC 0020 (config file + `${env}` substitution the token store
  rides), CLAUDE.md §3.4 (ack-before-WAL interplay) and §3.7
  (multi-tenancy invariant), RFC 0027 (the MCP surface gated on
  this RFC).
