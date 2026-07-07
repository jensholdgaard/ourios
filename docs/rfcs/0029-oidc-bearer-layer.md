---
rfc: 0029
title: OIDC bearer layer (issuer-agnostic, Dex-validated)
status: drafted
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
- At least one of `tokens` / `oidc` must be present in an `auth`
  section; both together are valid. No `auth` section remains open
  mode with the RFC 0026 startup warning.

### 3.2 Verification and resolution

- One resolution path in front of the existing gates: a presented
  bearer is first matched against the static store (constant-time,
  RFC 0026 §6); an unmatched credential that **parses as a JWT** is
  verified OIDC-side — signature against the cached JWKS
  (asymmetric algorithms only: RS256/ES256 family; `alg: none` and
  HMAC are rejected outright), `iss` equality, `aud` containment,
  `exp`/`nbf` with a small configured clock skew. A verified token
  resolves to `(name_claim, tenant_claim)` and flows into the
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

Written at the `specified` gate (docs/rfcs/README.md lifecycle).
Expected shape: config resolution (`oidc` section, coexistence, at
least-one rule); verification matrix (valid/expired/wrong-aud/
wrong-iss/bad-sig/`alg:none`/HMAC-downgrade all rejected as one
401); claim → tenant binding driving the unchanged ingest/query/MCP
enforcement incl. wildcard; JWKS rotation mid-run; static+OIDC
coexistence; the Dex-container end-to-end (Collector
client-credentials flow included); telemetry/audit parity with
RFC 0026 §5.7.

## 6. Testing strategy

Follows §5 at the `specified` gate. Unit level: a fixture issuer
(local keypair) for the verification matrix — fast, deterministic,
no container. Acceptance level: the real-Dex testcontainers job,
CI-gated alongside the S3 integration jobs. The RFC 0026 §5 suite
re-runs unchanged with an OIDC-resolved binding substituted for the
static one — the enforcement-invariance proof.

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
