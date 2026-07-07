# Authentication

Three postures, one enforcement path
([RFC 0026](../rfcs/0026-authentication-tenant-binding.md) +
[RFC 0029](../rfcs/0029-oidc-bearer-layer.md)). Whatever
authenticates a request, the result is the same `(name, tenants)`
binding: ingest batches must fall entirely inside the binding's
tenant set (whole-batch 403 otherwise, before the WAL), queries and
MCP tool calls enforce the same set, and the `name` labels the audit
trail and metrics. Rejections are deliberately undifferentiated — one
401 shape, no probing oracle.

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
    endpoint: https://ourios.example.com:4317
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
and again only when an unseen key id appears (rotation). Signatures
are RS256/ES256-family only; `alg: none` and HMAC never verify.

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
    endpoint: https://ourios.example.com:4317
    auth:
      authenticator: oauth2client
```

Both halves coexist in one config — a static-token Collector and
JWT-bearing senders authenticate side by side, each confined to its
own tenant binding.

## TLS

The listeners speak plaintext today; terminate TLS in front (ingress,
service mesh, or an L4 proxy) — bearer tokens over plaintext are not
auth. Native listener TLS is tracked on the
[auth epic](https://github.com/jensholdgaard/ourios/issues/331).
