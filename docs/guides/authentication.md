# Authentication

Three postures, one enforcement path
([RFC 0026](../rfcs/0026-authentication-tenant-binding.md) +
[RFC 0029](../rfcs/0029-oidc-bearer-layer.md)), plus an optional
authorization graph behind them
([RFC 0047](../rfcs/0047-rebac-resolver-and-graph-visibility.md)).
Whatever authenticates a request, the result is the same
`(name, read tenants, write tenants)` binding: ingest batches must
fall entirely inside the binding's write set (whole-batch 403
otherwise, before the WAL), queries and MCP tool calls enforce the
read set, and the `name` labels the audit trail and metrics. Static
tokens and OIDC claims bind both sets identically; the graph binds them
separately. Rejections are deliberately undifferentiated — one 401
shape, no probing oracle.

## Open mode (development only)

No `auth` section at all. Every request passes unbound; the server
warns once at startup. Never expose an open-mode listener beyond
localhost or a trusted network segment.

## Static bearer tokens

The Collector-friendly baseline — static credentials in the config
file, values injected via `${env:…}` (inline literals fail startup):

```yaml
auth:
  tokens:
    - name: edge-collector
      token: ${env:OURIOS_EDGE_TOKEN}
      tenants: [checkout, payments]   # or ["*"] for all tenants
```

Senders attach `Authorization: Bearer <token>`; with a Collector:

```yaml
extensions:
  bearertokenauth:
    token: ${env:OURIOS_EDGE_TOKEN}
exporters:
  otlp:
    endpoint: ourios.example.com:4317   # TLS by default; gRPC host:port
    auth:
      authenticator: bearertokenauth
```

Comparison is constant-time; token values never appear in logs,
errors, metrics, or audit events — only the `name` does.

## OIDC (JWTs from an identity provider)

Adds standards-based machine identity in front of the same
enforcement — any conforming issuer works;
[Dex](https://dexidp.io/) (CNCF) is the recommended lightweight
deployment and the one the acceptance suite runs against:

```yaml
auth:
  oidc:
    issuer: https://dex.example.com
    audience: ourios-collector        # your client id
    tenant_claim: groups              # a string-list claim → the tenant set
    name_claim: name                  # the audit/metric label
```

Verification is local: the issuer is contacted once at startup
(discovery + JWKS — an unreachable issuer fails startup, by design)
and again only when an unseen key id appears (rotation). Signatures verify against the asymmetric allow-list only —
RS256/384/512, PS256/384/512, ES256/384; `alg: none` and HMAC never
verify.

Machine senders use the OAuth2 client-credentials flow — with a
Collector this is zero custom code:

```yaml
extensions:
  oauth2client:
    client_id: ourios-collector
    client_secret: ${env:DEX_CLIENT_SECRET}
    token_url: https://dex.example.com/token
    scopes: [openid, profile, groups]
exporters:
  otlp:
    endpoint: ourios.example.com:4317   # TLS by default; gRPC host:port
    auth:
      authenticator: oauth2client
```

Both halves coexist in one config — a static-token Collector and
JWT-bearing senders authenticate side by side, each confined to its
own tenant binding.

## OpenFGA (relationship graph binds the tenants)

[RFC 0047](../rfcs/0047-rebac-resolver-and-graph-visibility.md) adds
[OpenFGA](https://openfga.dev/) as an *authorization* layer behind
either authenticator — OpenFGA never authenticates. Once a bearer is
known (static token or verified JWT), the principal is mapped
(`service_account:<token name>`, `user:<sub>`, or `agent:<sub>` when
the token carries the configured `agent_claim`) and the graph answers
which tenants it may query (`can_query`) and write (`can_write`),
using the in-tree model `deploy/openfga/model.fga`:

```yaml
auth:
  tokens:
    - name: collector-cluster1
      token: ${env:OURIOS_COLLECTOR_TOKEN}
      tenants: ["*"]                  # the graph decides; a list here only narrows
  oidc:
    issuer: https://dex.example.com
    audience: ourios
    groups_claim: groups              # → contextual team#member tuples
    agent_claim: ourios_principal_type=agent
    # tenant_claim is optional here — the graph binds the tenants
  openfga:
    api_url: http://openfga.auth.svc:8080
    store_id: 01M07RYMXRDW4ND5M7XQV04W8R
    authorization_model_id: 01M07RZE9RHPVPTYCV22RX0TDA   # pinned; omit = latest
    api_token: ${env:OURIOS_OPENFGA_TOKEN}                 # ${env:…} only
    session_ttl_secs: 60              # revocation latency = binding cache TTL
    consistency: minimize_latency     # higher_consistency bypasses OpenFGA's cache
```

Grants are administrative tuples written through OpenFGA's own API or
CLI, never by Ourios: `tenant:acme#reader@user:alice`,
`tenant:acme#writer@service_account:collector-cluster1`,
`tenant:acme#owner@team:platform#member`. A token's group claim rides
along as request-scoped `team:<group>#member@<principal>` tuples
(never persisted; at most 100), so team membership needs no sync
pipeline. A credential's own tenant list — a static token's `tenants`,
an OIDC `tenant_claim` — can only narrow what the graph grants, never
widen it; a principal the graph grants nothing is unbound (401).

The binding is cached per credential for `session_ttl_secs` and is
**fail-closed**: an unreachable or slow OpenFGA answers `503` on the
query and MCP surfaces and `UNAVAILABLE`/`503` on ingest, and
`ourios.auth.resolutions` counts the failure as
`error.type = upstream_unavailable`. Static tokens and OIDC keep working
without an `openfga` section; a deployment that wants coarse tenants only
never touches it.

## TLS

The listeners speak plaintext today; terminate TLS in front (ingress,
service mesh, or an L4 proxy) — bearer tokens over plaintext are not
auth. Native listener TLS is tracked on the
[auth epic](https://github.com/jensholdgaard/ourios/issues/331).
