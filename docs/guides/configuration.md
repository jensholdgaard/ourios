# Configuration

Two mutually exclusive sources ([RFC
0020](../rfcs/0020-configuration-file.md) /
[RFC 0004](../rfcs/0004-configuration-policy.md)):

- **A YAML file** via `--config <path>` — the file is then the *sole*
  source; the environment participates only through `${env:NAME}`
  substitution inside it (with `${env:NAME:-default}` defaults, `$$`
  escaping — the OTel Collector data model).
- **`OURIOS_*` environment variables** when no `--config` is given —
  the container/dev posture.

Parsing is **strict**: a malformed value is a startup error in either
mode, and in config-file mode an unknown YAML key is rejected too
(unrecognised `OURIOS_*`-lookalike env vars are simply not read —
there is no unknown-key concept in the environment).

## A complete file example

```yaml
storage:
  # local (a filesystem directory as the store — dev/single-node) or
  # s3 (object storage as the source of truth — production; RFC 0019).
  backend: s3
  s3:
    bucket: ourios-logs
    region: eu-central-1
    # Any S3-compatible provider: AWS, MinIO, R2, Ceph/RGW, …
    endpoint: https://s3.eu-central-1.amazonaws.com
    # Secret hygiene is enforced: credentials MUST be ${env:…}
    # references — inline literals fail startup.
    access_key_id: ${env:OURIOS_S3_ACCESS_KEY_ID}
    secret_access_key: ${env:OURIOS_S3_SECRET_ACCESS_KEY}
  # RFC 0022: per-key promoted attribute columns (service.name is
  # always promoted). Each key costs bytes on every row — opt in
  # deliberately.
  promoted_attributes:
    resource: [k8s.namespace.name]
    log: [http.request.method, http.route]

receiver:
  enabled: true
  grpc_addr: 0.0.0.0:4317
  http_addr: 0.0.0.0:4318
  # The WAL stays on local disk by design, S3 or not (RFC 0019).
  wal_root: /var/lib/ourios/wal
  # RFC 0035: concurrent Parquet-encode workers (default: all cores).
  encode_workers: 4

querier:
  enabled: true
  http_addr: 0.0.0.0:4319
  default_window_secs: 3600
  mcp:
    enabled: false

auth:
  # See the Authentication guide. Omit the whole section for open
  # mode (development only — the server warns once at startup).
  tokens:
    - name: edge-collector
      token: ${env:OURIOS_EDGE_TOKEN}
      tenants: [checkout, payments]
  oidc:
    issuer: https://dex.example.com
    audience: ourios-collector
    tenant_claim: groups              # optional once openfga binds the tenants
    name_claim: name
    # RFC 0047: agent principals + group claims for the graph resolver
    agent_claim: ourios_principal_type=agent
    groups_claim: groups
  openfga:                            # RFC 0047 — the graph binds tenants
    api_url: http://openfga.auth.svc:8080
    store_id: 01M07RYMXRDW4ND5M7XQV04W8R
    authorization_model_id: 01M07RZE9RHPVPTYCV22RX0TDA   # pinned; omit = latest
    api_token: ${env:OURIOS_OPENFGA_TOKEN}                 # ${env:…} only
    session_ttl_secs: 60
    consistency: minimize_latency     # or higher_consistency
    request_timeout_secs: 5
    server_list_objects_deadline_ms: 3000
    visibility:                       # RFC 0047 §3.4 — layer 2 inside a tenant
      objects:
        - type: conversation
          column: attr.gen_ai.conversation.id
      # Optional (RFC 0048 §3.2). Which promoted columns carry the
      # principals in a conversation. An omitted list takes the semconv
      # default shown here and is exempt from the promoted-column check;
      # every EXPLICITLY listed entry must be a promoted column (startup
      # error otherwise). self_principal_column, when set, must be a
      # promoted column AND one of user_columns.
      # identities:
      #   user_columns: [attr.user.hash, attr.enduser.pseudo.id]
      #   agent_columns: [attr.gen_ai.agent.id]
      self_principal_column: attr.user.hash
      # Optional. REPLACES the default set (body + the GenAI content
      # attributes) — list every column to mask; must not be empty.
      # content_columns: [body, attr.gen_ai.input.messages, attr.gen_ai.output.messages]
      max_objects: 10000
      list_timeout_ms: 2000           # must be < server_list_objects_deadline_ms
```

## Environment variables (no `--config`)

| Variable | Meaning |
|---|---|
| `OURIOS_STORAGE_BACKEND` | `local` (default) or `s3` |
| `OURIOS_BUCKET_ROOT` | local-backend store root |
| `OURIOS_S3_BUCKET` / `OURIOS_S3_REGION` / `OURIOS_S3_ENDPOINT` / `OURIOS_S3_PREFIX` | S3 addressing |
| `OURIOS_S3_ACCESS_KEY_ID` / `OURIOS_S3_SECRET_ACCESS_KEY` / `OURIOS_S3_SESSION_TOKEN` | S3 credentials |
| `OURIOS_RECEIVER_ENABLED` / `OURIOS_RECEIVER_GRPC_ADDR` / `OURIOS_RECEIVER_HTTP_ADDR` | receiver role |
| `OURIOS_WAL_ROOT` | WAL directory (receiver) |
| `OURIOS_RECEIVER_ENCODE_WORKERS` | concurrent encode pool size (RFC 0035; default: all cores) |
| `OURIOS_QUERIER_ENABLED` / `OURIOS_QUERIER_HTTP_ADDR` / `OURIOS_QUERIER_DEFAULT_WINDOW_SECS` | querier role |
| `OURIOS_QUERIER_MCP_ENABLED` | the `/mcp` agent surface (RFC 0027) |
| `OURIOS_COMPACTION_ENABLED` / `OURIOS_COMPACTION_INTERVAL_SECS` | background compactor |

Auth configuration is **file-only** — there are deliberately no
`OURIOS_AUTH_*` variables; token values reach the file through
`${env:…}` references.

There is no tenant-derivation configuration: the tenant is named **out of
band on every export** (RFC 0046) — the `X-Ourios-Tenant` header over
OTLP/HTTP, `x-ourios-tenant` metadata over OTLP/gRPC — exactly as the
querier's `X-Ourios-Tenant`. It is required in open mode too (no default
tenant) and, with auth on, must be one of the credential's tenants. A
Collector sets it once, e.g. `exporters.otlp.headers.x-ourios-tenant: acme`
(or per request via the `headers_setter` extension). Resource attributes such
as `service.name` describe the producer and never choose the tenant.
