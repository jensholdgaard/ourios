---
rfc: 0029
title: OIDC bearer layer (issuer-agnostic, Dex-validated)
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-07
supersedes: —
superseded-by: —
---

# RFC 0029 — OIDC bearer layer (issuer-agnostic, Dex-validated)

## 1. Summary

RFC 0026 (accepted) authenticated both data-plane surfaces with
**static bearer tokens** bound to tenant sets, and deliberately
deferred identity-provider integration (§7.2) — designing the
`(name, tenants)` binding as "the stable layer" a verified-claim
validator could later map onto. This RFC is that layer:

1. **OIDC JWT verification** as a second credential kind on every
   RFC 0026 gate (OTLP ingest, the query API, the RFC 0027 MCP
   surface): standard `iss`/`aud`/`exp`/signature validation against
   the issuer's published JWKS, with a configured **claim → tenant
   mapping** that resolves each verified token to exactly the
   `(name, tenants)` shape the existing enforcement consumes. No
   enforcement point changes; only the resolution in front of it
   grows a branch.
2. **Issuer-agnostic by construction, Dex-blessed by test.** Ourios
   implements the OIDC standard, not a vendor SDK; any conforming
   issuer works. **Dex** (the CNCF identity broker) is the
   recommended lightweight deployment and the implementation the
   acceptance suite runs against (a real Dex container via
   testcontainers — the LocalStack pattern from RFC 0019).
3. **Additive, never replacing.** Static tokens (RFC 0026 §3.1)
   remain fully supported and can coexist with OIDC in one config —
   static for dev/single-box, OIDC for fleets. Open mode is
   untouched.

Touches invariant §3.7 (multi-tenancy — the binding derivation
gains a second source) and rides the RFC 0026 audit/telemetry
surfaces unchanged. Resolves RFC 0026 §7.1 (token rotation) as a
side effect: JWTs expire and renew; no long-lived shared secret
crosses the wire.

## 2. Motivation

- **Fleets outgrow static tokens.** A handful of collectors with
  `${env}` tokens is fine; dozens of teams rotating shared secrets
  through config management is the operational failure mode OIDC
  exists to remove. Expiry, rotation, and revocation become the
  issuer's job — solved once, not per backend.
- **The ecosystem path already exists.** The OTel Collector's
  `oauth2client` extension performs the client-credentials flow
  against any OAuth2 token endpoint and attaches the bearer to
  exporters — collectors can authenticate to Ourios through an IdP
  **today**, with zero collector-side custom code. Dex supports the
  grant (opt-in: `DEX_CLIENT_CREDENTIAL_GRANT_ENABLED_BY_DEFAULT`)
  and token exchange as the documented machine-to-machine paths.
- **MCP's authorization model is OAuth 2.1.** RFC 0027 shipped the
  agent surface under the static-bearer gate; the MCP specification's
  own auth story is OAuth. An OIDC layer is the prerequisite for
  spec-compliant agent authentication rather than a parallel
  invention.
- **RFC 0026 planned for this.** §4 rejected JWT/OIDC as the
  *baseline* ("expiry, issuers, key rotation, clock dependence, and
  a validation dependency tree — for senders that are collectors
  with static config") and §7.2 named the layering as the follow-up.
  The baseline argument stands; this RFC adds the layer without
  disturbing it.

## 3. Design

### 3.1 Configuration (RFC 0020 amendment)

A sibling to `auth.tokens`:

```yaml
auth:
  tokens:                          # RFC 0026, unchanged; optional
    - name: dev-cli
      token: ${env:OURIOS_TOKEN_DEV}
      tenants: ["dev"]
  oidc:                            # this RFC; optional
    issuer: https://dex.internal.example
    audience: ourios
    tenant_claim: ourios_tenants   # claim carrying the tenant list
    name_claim: sub                # audit/metric label (default sub)
```

- `issuer` is the OIDC discovery root: Ourios fetches
  `/.well-known/openid-configuration` once at startup and the JWKS
  it names, then re-fetches keys on rotation (cache with the
  standard kid-miss refresh; a bounded grace covers issuer blips —
  §7).
- `audience` is required — an Ourios deployment must never accept
  tokens minted for another service.
- `tenant_claim` names a claim whose value is a list of tenant ids
  (or the wildcard `"*"`), mapped verbatim onto RFC 0026's
  `TenantSet`; `name_claim` (default `sub`) feeds the audit/metric
  label. The mapping is deliberately dumb — group-to-tenant
  indirection lives in the issuer (Dex connectors already map
  upstream groups into claims), not in Ourios.
- At least one of `tokens` / `oidc` must be configured in an
  `auth` section; both together are valid. The RFC 0026 empty-list
  rule is unchanged and unconditional: an explicit `tokens: []`
  **always** fails startup — to run OIDC-only, omit `tokens`
  entirely. No `auth` section remains open mode with the RFC 0026
  startup warning.

### 3.2 Verification and resolution

- One resolution path in front of the existing gates: a presented
  bearer is first matched against the static store (constant-time,
  RFC 0026 §6); an unmatched credential that **parses as a JWT** is
  verified OIDC-side — signature against the cached JWKS
  (asymmetric algorithms only: RS256/ES256 family; `alg: none` and
  HMAC are rejected outright), `iss` equality, `aud` containment,
  `exp`/`nbf` with a small configured clock skew. A verified token
  resolves to the RFC 0026 `(name, tenants)` binding — the *values*
  of the configured `name_claim` / `tenant_claim` keys — and flows
  into the
  **unchanged** RFC 0026 enforcement: whole-batch tenant binding
  before the WAL ack, the query/MCP 403 contract, the same
  rejection telemetry (`error.type` values unchanged) and
  `ingest_denied` audit event carrying the name label.
- Verification is local (a signature check against cached keys) —
  **no per-request issuer round-trip**, so the §3.4-adjacent ingest
  hot path gains arithmetic, not network. The issuer is contacted
  only at startup, on JWKS rotation, and on kid misses.
- Failure stays one undifferentiated 401 on the wire (RFC 0026's
  no-oracle rule); the telemetry may distinguish `unauthenticated`
  reasons only at the existing low-cardinality `error.type` level.

### 3.3 Dex as the blessed deployment

- Docs and the acceptance suite treat Dex as the reference issuer:
  single Go binary, CNCF, federates upstream identity (LDAP, GitHub,
  SAML, OIDC) through connectors, and issues the JWTs Ourios
  verifies. Machine senders use the client-credentials grant
  (Collector `oauth2client` → Dex token endpoint) or token
  exchange; humans/agents use the standard flows Dex provides.
- The §5 suite runs against a **real Dex container**
  (testcontainers, CI-gated like the LocalStack S3 jobs): mint real
  tokens, verify against Dex's real JWKS, exercise expiry and
  rotation. Nothing in `ourios-server` links Dex-specific code —
  conformance is to the OIDC standard.

### 3.4 What deliberately does not change

- Static tokens, open mode, the enforcement points, the audit
  schema, the metric names, and the `(name, tenants)` model are all
  untouched. This RFC is a second *resolver*, not a second *model*.
- Transport encryption remains the fronting-proxy posture (RFC 0026
  §1); bearer-over-plaintext caveats apply identically to JWTs.

## 4. Alternatives considered

- **Keycloak (or a cloud IdP) as the blessed issuer.** Heavier to
  run than Dex and no more standard; since Ourios implements the
  protocol, they all work anyway — the blessing is about docs and CI
  weight, and Dex's single-binary, connector-broker shape matches
  this project's deployment story. CNCF alignment is a tiebreaker,
  not the argument.
- **Vendor-SDK integration (issuer-specific).** Couples the backend
  to one IdP's release train and dependency tree for zero standard
  coverage gain. Rejected.
- **OpenFGA (Zanzibar-style ReBAC) for the authorization half.**
  Answers a different question — *what may this identity touch* —
  and answers it with a separate stateful service plus a check-API
  round-trip on the pre-ack ingest path, where today's model is one
  in-memory set-membership test over a flat tenant list. **Adopt-if**:
  tenancy grows hierarchy (orgs → teams), per-stream ACLs, or
  delegation. The seam is already clean — RFC 0026's binding check
  is a single `tenants().allows(...)` call an FGA-backed resolver
  could slot behind without reshaping the model. Until that
  requirement exists, an external authz service is operational
  surface without a question to answer.
- **mTLS client identity.** Re-rejected on RFC 0026 §4's grounds:
  it does not survive the fronting-proxy posture without
  header-forwarding trust decisions.
- **Opaque tokens + issuer introspection (RFC 7662).** Puts the
  issuer on the request path (introspection call per token) — the
  availability coupling §3.2 exists to avoid. JWTs verify locally.

## 5. Acceptance criteria

Scenario ids `RFC0029.<m>`. Scenario .1 is pure config resolution
(no issuer); .2–.6 run against a fixture issuer (a local keypair
serving discovery + JWKS over a loopback listener — fast,
deterministic, no container); .7 is the real-Dex acceptance arm.

> **Scenario RFC0029.1 — config resolution.** Given a config with an
> `auth.oidc` section whose values use `${env:VAR}`, When the server
> starts, Then they resolve through the RFC 0020 substitution
> engine; a missing `audience` is a startup configuration error; an
> `auth` section with neither `tokens` nor `oidc` is a startup
> configuration error; an explicit `tokens: []` is a startup
> configuration error **regardless of whether `oidc` is present**;
> an `oidc`-only section starts and serves; a missing `auth` section
> starts in open mode with the RFC 0026 warning, unchanged.

> **Scenario RFC0029.2 — verification matrix.** Given OIDC
> configured against the fixture issuer, When a request presents a
> bearer that is (a) a valid in-audience token, Then it is accepted;
> and when it presents (b) an expired token, (c) a token before its
> `nbf` beyond the configured skew, (d) a wrong-`aud` token, (e) a
> wrong-`iss` token, (f) a token with a corrupted signature, (g) an
> `alg: none` token, (h) an HMAC-signed token whose key is the
> public JWKS material (downgrade), or (i) a non-JWT unknown bearer,
> Then every one of (b)–(i) is rejected as the **same
> undifferentiated 401** (identical status and body — no oracle),
> before wire decode on ingest, and nothing reaches the WAL.

> **Scenario RFC0029.3 — claim binding drives unchanged
> enforcement.** Given a verified token whose `tenant_claim` value
> is `["a", "b"]`, Then the RFC 0026 §5.3/§5.4 contracts hold
> verbatim with the OIDC-resolved binding substituted for the static
> one: in-set ingest batches ack; any batch touching a tenant
> outside `{a, b}` is whole-batch 403 with no WAL append; the query
> API and the MCP surface enforce the same 401→400→403 order; and
> the `name_claim` value appears as the name label where the token
> name appears today.

> **Scenario RFC0029.4 — wildcard claim.** Given a verified token
> whose `tenant_claim` value is `["*"]`, Then ingest and query to
> arbitrary tenants behave as if every tenant were listed
> (RFC 0026 §5.5 parity).

> **Scenario RFC0029.5 — coexistence and resolution order.** Given
> one config with both `tokens` and `oidc`, Then a static token
> authenticates via the constant-time store, a JWT from the issuer
> authenticates via OIDC, each carrying its own tenant binding
> side by side; a static-only config and an `oidc`-only config each
> serve; and with no `auth` section the full RFC 0026 §5.6 open-mode
> parity arm passes unchanged.

> **Scenario RFC0029.6 — JWKS rotation.** Given a served instance
> verifying against the fixture issuer, When the issuer rotates its
> signing key mid-run, Then a token signed by the new key (unseen
> `kid`) triggers a JWKS re-fetch and verifies without restart, and
> a token signed by the withdrawn key is rejected once the refreshed
> key set no longer contains it.

> **Scenario RFC0029.7 — Dex end-to-end with telemetry parity.**
> Given a real Dex container (testcontainers, CI-gated like
> RFC 0019's `s3 integration (localstack)` job) with the
> client-credentials grant enabled and a
> static client whose claims carry the tenant list, When a token
> minted from Dex's token endpoint drives ingest, query, and MCP
> against a served instance verifying Dex's real JWKS, Then all
> three succeed; a short-TTL token is rejected with the
> undifferentiated 401 after expiry; rejections increment the
> existing counters with the unchanged `error.type` values and an
> ingest authz denial emits the `ingest_denied` audit event carrying
> the `name_claim` value — and no JWT material (token, header,
> claims payload) appears on any surface (metrics, audit, logs,
> error bodies).

## 6. Testing strategy

Unit level: .1 is pure config resolution (no issuer at all); the
§5 fixture issuer (local keypair) covers .2–.6 — fast,
deterministic, no container. Acceptance level: the real-Dex
testcontainers job (.7), CI-gated alongside RFC 0019's
`s3 integration (localstack)` job. The RFC 0026 §5 suite re-runs
unchanged with an OIDC-resolved binding substituted for the static
one — the enforcement-invariance proof behind .3–.5.

## 7. Open questions

1. **JWKS outage grace.** How long verified-key caches may serve
   after the issuer becomes unreachable (bounded staleness vs.
   fail-closed on rotation-with-outage).
2. **Human/agent flows for the query and MCP surfaces.** Device
   flow via Dex for CLI/agent login, and whether `/mcp` should
   advertise OAuth metadata per the MCP authorization spec once
   this layer exists.
3. **Claim schema convention.** Whether `ourios_tenants` becomes a
   documented convention Dex configs ship, or stays fully
   deployment-chosen.
4. **Revocation latency.** Short TTLs are the plan; whether any
   deployment class needs sub-TTL revocation (and thus
   introspection after all) is demand-driven.

## 8. References

- RFC 0026 (the binding model, §4's JWT-baseline rejection, §7.1–.2
  the rotation/IdP follow-ups this RFC discharges), RFC 0027 (the
  MCP surface; MCP's OAuth 2.1 authorization model), RFC 0020
  (config schema + `${env}`), RFC 0019 §6 (the testcontainers
  CI-gating pattern), `CLAUDE.md` §3.7.
- Dex: https://dexidp.io (CNCF; client-credentials grant opt-in via
  `DEX_CLIENT_CREDENTIAL_GRANT_ENABLED_BY_DEFAULT`, token exchange
  per its machine-auth guide). OTel Collector `oauth2client`
  extension (the collector-side client-credentials flow). OpenFGA:
  https://openfga.dev (the adopt-if ReBAC engine, §4).
