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

## Naming the tenant (every posture)

Authentication decides *which tenants a caller may touch*; the request
itself must still *name* the one it means
([RFC 0046](../rfcs/0046-out-of-band-tenancy.md): tenancy is
out-of-band — never derived from the OTLP payload). Every ingest
export carries an `x-ourios-tenant` header (gRPC metadata key or HTTP
header) naming the tenant the whole export lands in; an export without
it is rejected (`INVALID_ARGUMENT` / 400) before any WAL work.
Queries and MCP calls name their tenant the same way. With a
Collector, set it on the exporter:

```yaml
exporters:
  otlp:
    endpoint: ourios.example.com:4317
    headers:
      x-ourios-tenant: checkout
```

One export = one tenant: a pipeline feeding several tenants runs one
exporter per tenant (Collector `routing` connectors compose cleanly
with this). The named tenant must fall inside the caller's write set,
or the whole batch is a 403.

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

### Visibility inside a tenant (layer 2)

With the graph configured, every query also runs the RFC 0047 §3.4
**two-step** for the principal in the tenant it queries — query rewrite
at plan time, never per-record checks:

1. `Check(principal, can_read_content, tenant)` allowed → the tenant
   predicate only (today's plan).
2. else `Check(can_read_metadata, tenant)` allowed → every row, with the
   configured `content_columns` returned as null (`body` as
   `{"kind":"masked"}`); a query that filters or aggregates on one of them
   is `403 column_forbidden`, naming the column.
3. else the principal is **scoped** (bound through `scoped_reader`): its
   readable conversations are enumerated through the *streamed*
   `ListObjects` — filtered to this tenant, at most `max_objects` tenant
   ids, within `list_timeout_ms` — and become
   `attr.gen_ai.conversation.id IN (…)`, OR'd with the self fast path
   (`self_principal_column == <subject>`, `user:` principals only). Past
   the bound: `403 visibility_bound` ("ask for tenant-wide read"); a
   cut-off stream: `503 visibility_incomplete` — never a partial predicate.
   Template-level queries (`drift`, `list_templates`, `template_drift`)
   need tenant-wide content read (`403 visibility_scoped`).

```yaml
auth:
  openfga:
    # …
    server_list_objects_deadline_ms: 3000   # OPENFGA_LIST_OBJECTS_DEADLINE
    visibility:
      objects:
        - type: conversation
          column: attr.gen_ai.conversation.id   # a promoted column
      self_principal_column: attr.user.hash     # optional; user: principals only
      # content_columns: [...]                  # optional; REPLACES the default set
      max_objects: 10000                        # tenant ids only
      list_timeout_ms: 2000                     # MUST be < the server deadline
```

`content_columns` defaults to the GenAI content attributes plus `body`;
an explicit list **replaces** that set (list every column to mask) and
may not be empty — masking is never silently disabled. `objects` unset
means scoped principals see nothing (their bound is not enumerable).
Tenant-scoped graph objects are `tenant:<T>` and
`conversation:<enc(T)>/<id>` where `enc` percent-encodes `/` and `%` in
the tenant, so tenants containing `/` never alias; a tenant that cannot
be an object id (`:`, `#`, whitespace) has no graph objects and every
question about it fails closed. The two `Check`s cache with the session TTL; the
enumeration runs per query. The branch a query took is recorded on
`ourios.query.visibility` (`ourios.query.visibility.branch`) and the
request span.

### Feeding the graph from the data

With a `conversation` object bound, Ourios writes the data-derived
tuples itself (RFC 0047 §3.3) — nobody hand-writes conversation grants:
for every stored row the compaction sweep rewrites, and for every batch
the receiver flushes, the emitter derives
`conversation:T/<id>#parent@tenant:T`, `#participant@user:<user.hash |
enduser.pseudo.id>` (plus `tenant:T#scoped_reader@user:<…>`),
`#actor@agent:<gen_ai.agent.id>` (plus its binding tuple) and the
per-tenant `tool:T/<name>#parent@tenant:T` objects, and writes them in
idempotent ≤ 100-tuple batches (`ourios.graph.tuples`). Operators write
only the administrative tuples (`tenant#reader/writer/owner/
metadata_reader`, `team#member`, `tool#caller`, `conversation#delegate`).

**Erasing a conversation** (RFC 0047 §3.6 / RFC 0048 §3.3): the front
door is the CLI, run with the daemon's config —

```text
ourios-server --config ourios.yaml graph erase --tenant acme --conversation c-7
ourios-server --config ourios.yaml graph erasures            # pending + phase
```

`erase` writes the durable request marker
(`erasure/tenant_id=<enc>/conversation=<enc>`, create-if-absent, so a
repeat never resets an in-flight erasure) and the next sweep rewrites
every partition of the tenant with the conversation's rows dropped, then
deletes its graph tuples, then writes a `conversation_erased` audit
event, logs one `ourios.compaction.erasure.completed` event and removes
the marker. Rows first, tuples after — a dangling tuple is harmless, a
dangling row is a leak.

**Backfilling history** (RFC 0048 §3.4): data stored before the graph
was configured is fed to it with

```text
ourios-server --config ourios.yaml graph backfill --tenant acme [--from 2026-08-01T00:00:00Z]
```

— reads every partition of the tenant (`--from` keeps partitions whose
UTC hour starts at or after it), derives and writes the same tuples the
sweep would, in idempotent ≤ 100-tuple batches, and never rewrites
Parquet. Resumable: run it again after an interruption. Backfill and
erasure exclude each other through the store: backfill refuses while
erasures are pending, and the sweep defers a tenant's erasures while a
backfill lock exists (`graph backfill --unlock --tenant acme` clears a
crashed run's lock; `graph erasures` lists both marker kinds).

Both verbs are instrumented like the daemon: each run is one OpenTelemetry
CLI callee span (`process.executable.name`, `process.pid`,
`process.exit.code`, and `error.type` on failure), the backfill's
per-partition progress events reach stderr, and everything exports over
OTLP when the standard variables point somewhere —
`OTEL_EXPORTER_OTLP_ENDPOINT=http://collector:4317`. Silence it the
standard way: `OTEL_SDK_DISABLED=true` for everything, or per signal with
`OTEL_TRACES_EXPORTER=none`, `OTEL_METRICS_EXPORTER=none` and
`OTEL_LOGS_EXPORTER=none`.

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
