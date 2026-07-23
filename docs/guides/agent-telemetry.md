# Observe your coding agent

Coding agents (Claude Code, GitHub Copilot CLI, …) emit OpenTelemetry
already. Point that telemetry at a local Ourios and you close a loop: the
agent's own API cost, token usage, and tool decisions land as OTLP logs,
and the agent can query them back — through Ourios's MCP surface — about
itself.

The whole loop runs on your machine. Agent telemetry is sensitive
(prompts, tool output, source, and whatever flows through them), so the
value here is that **none of it leaves the host** — no SaaS, no
phone-home. That is the point, not a footnote.

Three steps: run Ourios, point the agent at it, ask the agent about
itself.

## 1. Run Ourios

Aggregating by attribute (`count by attr.model`, `sum(attr.cost_usd)`)
needs those attributes **promoted** to columns, which is a config-file
setting ([RFC 0022](../rfcs/0022-queryable-attribute-columns.md)). Write an
`ourios.yaml`:

```yaml
storage:
  backend: local
  local:
    bucket_root: /var/lib/ourios/store
  # Claude Code emits these as flat log-attribute keys (not the OTel
  # GenAI semantic-convention dotted names); promote the ones you want to
  # group or sum by.
  promoted_attributes:
    log: [model, cost_usd, tool_name, decision]
receiver:
  enabled: true
  grpc_addr: "0.0.0.0:4317"
  http_addr: "0.0.0.0:4318"
  wal_root: /var/lib/ourios/wal
querier:
  enabled: true
  http_addr: "0.0.0.0:4319"
  mcp:
    enabled: true
```

Run it — mapping the ports to **loopback only** (`127.0.0.1:`), because
this config has no `auth` section and so runs open (RFC 0026); a bare
`-p 4318:4318` would expose an unauthenticated receiver to your LAN:

```sh
docker run --rm \
  -p 127.0.0.1:4317:4317 -p 127.0.0.1:4318:4318 -p 127.0.0.1:4319:4319 \
  -v ourios-data:/var/lib/ourios \
  -v "$PWD/ourios.yaml:/etc/ourios/ourios.yaml:ro" \
  ghcr.io/jensholdgaard/ourios:0.1.1 \
  --config /etc/ourios/ourios.yaml
```

See [Docker](./docker.md) for image variants and signature
verification. `docker stop` flushes the ingest pipeline on SIGTERM.

## 2. Point the agent at it

Telemetry is read at process **startup**, so set these, then start a
**new** agent session. The enable flag alone ships nothing — you also
need the exporter block.

```sh
# where + how (any OTLP-log source)
export OTEL_LOGS_EXPORTER=otlp
export OTEL_METRICS_EXPORTER=none          # Ourios is logs-only
export OTEL_TRACES_EXPORTER=none
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
export OTEL_SERVICE_NAME=my-agent          # → the Ourios tenant

# then the per-tool enable flag:
export CLAUDE_CODE_ENABLE_TELEMETRY=1      # Claude Code
# export COPILOT_OTEL_ENABLED=true         # Copilot CLI
```

The **tenant** is the telemetry's `service.name` — `OTEL_SERVICE_NAME`
sets it; if an agent overrides it with its own default, that default is
the tenant instead. The tenants that exist show up as directories under
`bucket_root/data/` — e.g. `/var/lib/ourios/store/data/tenant_id=my-agent/`.

Prompt and tool bodies are **not** captured by default. Turn them on only
on data you're willing to retain — this is the sensitive part:

```sh
export OTEL_LOG_USER_PROMPTS=1 OTEL_LOG_TOOL_DETAILS=1   # Claude Code, opt-in
```

## 3. Add the MCP surface, and ask

Connect the same agent to Ourios's read-only MCP:

```sh
claude mcp add --transport http ourios http://127.0.0.1:4319/mcp
```

Now ask the agent about itself in plain language — it reads the
`ourios://query-schema` resource and composes the query:

> What has `my-agent` cost so far, by model? Use the ourios tools.

Queries are scoped by **tenant**, which is the whole `my-agent`
`service.name`, not a single run — so this reports every session that
shared that tenant, not just the current one (see the last note below).

Or query the DSL directly. First find your templates — the
`list_templates` MCP tool (or just ask the agent) lists each
`template_id` with its rendered text; note the id of the
`claude_code.api_request` template. Then:

```sh
# your tool-use distribution
curl -s http://127.0.0.1:4319/v1/query \
  -H 'X-Ourios-Tenant: my-agent' -H 'Content-Type: text/plain' \
  -d 'true | count by attr.tool_name'

# total spend per model — substitute the api_request template id for <ID>
curl -s http://127.0.0.1:4319/v1/query \
  -H 'X-Ourios-Tenant: my-agent' -H 'Content-Type: text/plain' \
  -d 'template_id == <ID> | sum(attr.cost_usd) by attr.model'
```

## Things that will trip you up

- **Query by `template_id` or an attribute, not `severity`.** These
  events carry `severity_number 0` (unset), so `severity >= trace`
  silently excludes them. Use `template_id == N` (find `N` with
  `list_templates`) or a promoted attribute, or the match-all `true`.
- **Fresh records sit in the WAL for up to ~5 minutes** before they flush
  to Parquet, and the querier reads Parquet only — so a query right after
  a burst of activity can come back empty. Give it a few minutes, or
  `docker stop` (which flushes) and restart.
- **Promotion is write-side.** It applies to telemetry captured *after*
  the server starts with the config above; earlier data has no
  `attr.model` column to group on.
- **`CLAUDE_CODE_ENABLE_TELEMETRY=1` alone exports nothing** — the
  `OTEL_LOGS_EXPORTER`/endpoint block in step 2 is what ships the logs.
- **Queries are tenant-wide, not per-session.** The tenant is the whole
  `service.name`, so every session that used that name aggregates
  together. To scope to one run, promote `session.id` (add it to
  `promoted_attributes.log`) and filter on it — e.g.
  `attr.session.id == "…" | sum(attr.cost_usd) by attr.model`.

## Where to next

- [Authentication](./authentication.md) — before any listener leaves
  loopback, put a token or OIDC in front (the config above is open).
- [RFC 0027](../rfcs/0027-mcp-query-surface.md) — the MCP surface;
  [RFC 0032](../rfcs/0032-query-schema-cost-model-resource.md) — the query-schema
  resource the agent reads to compose queries.
- [RFC 0002](../rfcs/0002-query-dsl.md) — the full query DSL, including
  the `count`/`sum`/`min`/`max`/`avg` aggregations.
