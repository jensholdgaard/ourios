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

Parsing is **strict** in both modes: an unknown key or a malformed
value is a startup error, never a silent ignore.

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
    access_key_id: ${env:AWS_ACCESS_KEY_ID}
    secret_access_key: ${env:AWS_SECRET_ACCESS_KEY}
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
    tenant_claim: groups
    name_claim: name
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
| `OURIOS_QUERIER_ENABLED` / `OURIOS_QUERIER_HTTP_ADDR` / `OURIOS_QUERIER_DEFAULT_WINDOW_SECS` | querier role |
| `OURIOS_QUERIER_MCP_ENABLED` | the `/mcp` agent surface (RFC 0027) |
| `OURIOS_COMPACTION_ENABLED` / `OURIOS_COMPACTION_INTERVAL_SECS` | background compactor |

Auth configuration is **file-only** — there are deliberately no
`OURIOS_AUTH_*` variables; token values reach the file through
`${env:…}` references.
