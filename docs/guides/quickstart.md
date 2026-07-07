# Quickstart — single binary

The fastest path from zero to querying logs: one `ourios-server`
process on your machine, local-disk storage, no auth. This is the
development/evaluation posture — see [Kubernetes
(Helm)](./kubernetes.md) for the production topology and
[Authentication](./authentication.md) before exposing any listener
beyond localhost.

## 1. Get the binary

Download a signed release archive (Linux; Apple-silicon and Intel
macOS builds come from `cargo build` today):

```sh
curl -LO https://github.com/jensholdgaard/ourios/releases/latest/download/ourios-server-x86_64-unknown-linux-gnu.tar.xz
tar -xf ourios-server-x86_64-unknown-linux-gnu.tar.xz
```

Every release artifact carries SLSA provenance (`*.intoto.jsonl`
alongside it), verifiable offline:

```sh
gh attestation verify ourios-server-x86_64-unknown-linux-gnu.tar.xz \
  --repo jensholdgaard/ourios \
  --bundle ourios-server-x86_64-unknown-linux-gnu.intoto.jsonl
```

Or build from source with `cargo build --release -p ourios-server`.

## 2. Run it

The binary is one server with three roles — **receiver** (OTLP
ingest), **querier** (the logs-DSL API), and the background
**compactor** (on by default). Enable the two network roles and point everything at a
scratch directory:

```sh
mkdir -p /tmp/ourios/data /tmp/ourios/wal

OURIOS_BUCKET_ROOT=/tmp/ourios/data \
OURIOS_WAL_ROOT=/tmp/ourios/wal \
OURIOS_RECEIVER_ENABLED=1 \
OURIOS_QUERIER_ENABLED=1 \
./ourios-server
```

Startup prints the bound addresses and warns once that auth is in
open mode:

```text
receiver gRPC listening on 0.0.0.0:4317
receiver HTTP listening on 0.0.0.0:4318
querier HTTP listening on 0.0.0.0:4319
```

The ports are the OTLP defaults (4317 gRPC, 4318 HTTP) plus 4319 for
the query API. Prefer a config file over env vars? See
[Configuration](./configuration.md) — `--config ourios.yaml` makes
the file the sole source.

## 3. Send logs

Ourios speaks OTLP and nothing else — any OpenTelemetry SDK or
Collector can ship to it unmodified. The tenant is derived from the
`service.name` resource attribute.

With a Collector, point the OTLP exporter at it:

```yaml
exporters:
  otlp:
    # host:port is version-proof for the gRPC exporter; recent
    # Collectors also accept scheme'd forms.
    endpoint: localhost:4317
    tls:
      insecure: true
```

Or hand-deliver one OTLP/JSON record for a first smoke test:

```sh
curl -s http://localhost:4318/v1/logs \
  -H 'Content-Type: application/json' \
  -d '{
    "resourceLogs": [{
      "resource": { "attributes": [
        { "key": "service.name", "value": { "stringValue": "checkout" } }
      ]},
      "scopeLogs": [{ "logRecords": [{
        "timeUnixNano": "1751971200000000000",
        "severityNumber": 9,
        "body": { "stringValue": "user 42 logged in" }
      }]}]
    }]
  }'
```

An empty `{}` response is the OTLP success shape. The batch is
fsynced to the write-ahead log **before** that acknowledgement — kill
the process mid-ingest and acknowledged data survives.

## 4. Query

`POST /v1/query` takes the logs DSL as plain text, with the tenant in
a header:

```sh
curl -s http://localhost:4319/v1/query \
  -H 'x-ourios-tenant: checkout' \
  -H 'Content-Type: text/plain' \
  -d 'severity >= info | limit 10'
```

The response carries the total match count, the returned rows
(bodies reconstructed from their mined templates), and scan
statistics that show the Parquet pruning at work:

```json
{
  "rows": 1,
  "stats": { "row_groups_scanned": 1, "row_groups_pruned": 0, "bytes_read": 4096 },
  "records": [ {
    "time_unix_nano": 1751971200000000000,
    "severity_number": 9,
    "body": { "kind": "rendered", "line": "user 42 logged in", "reconstruction": "faithful" },
    "...": "..."
  } ]
}
```

The DSL's full grammar — field predicates, regex, time ranges,
aggregation pipelines like
`service == "api" and severity >= error | count by template_id` — is
specified in [RFC 0002](../rfcs/0002-query-dsl.md).

## Where to next

- [Docker](./docker.md) — the same server from the published image.
- [Kubernetes (Helm)](./kubernetes.md) — the production topology on
  S3-compatible object storage.
- [Authentication](./authentication.md) — static bearer tokens and
  OIDC; do this before any listener leaves localhost.
- The MCP surface (agents querying Ourios over the Model Context
  Protocol) rides the querier at `/mcp` — enable with
  `OURIOS_QUERIER_MCP_ENABLED=1`
  ([RFC 0027](../rfcs/0027-mcp-query-surface.md)).
