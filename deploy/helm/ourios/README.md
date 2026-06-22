# Ourios Helm chart

Deploys [Ourios](https://github.com/jensholdgaard/ourios) — a log storage and
query backend on Apache Parquet, a Drain-derived template miner, and Apache
DataFusion.

Ourios is one binary (`ourios-server`) running three roles in a single process:
the always-on background **compactor**, the optional OTLP **receiver**, and the
optional **querier** (the logs-DSL query API). This chart runs that process as a
single-replica StatefulSet with the Parquet data store and the write-ahead log
on PVCs.

## Install

```sh
helm install ourios deploy/helm/ourios \
  --set image.tag=<release>            # defaults to the chart appVersion
```

By default both the receiver and the querier are enabled. Verify:

```sh
helm test ourios
```

## Topology note

Until the object-storage backend (RFC 0013) lands, the data store is a
`ReadWriteOnce` volume the roles share in one pod, so this is a **single
replica** — the compactor is the store's single writer and the chart must not be
scaled. When S3 arrives, the store is shared via object storage and the querier
splits into its own horizontally-scaled Deployment.

## Key values

| Key | Default | Description |
| --- | --- | --- |
| `image.repository` | `ghcr.io/jensholdgaard/ourios` | Image repository. |
| `image.tag` | `""` (chart `appVersion`) | Image tag. |
| `roles.receiver.enabled` | `true` | OTLP ingest over gRPC `:4317` + HTTP `:4318`. Provisions the WAL PVC. |
| `roles.querier.enabled` | `true` | Logs-DSL query API over HTTP `:4319`. |
| `compaction.intervalSecs` | `300` | Background compaction cadence. |
| `querier.defaultWindowSecs` | `3600` | Default look-back for a query with no `range(...)` stage. |
| `persistence.data.size` | `10Gi` | Size of the data-store PVC (`OURIOS_BUCKET_ROOT`). |
| `persistence.data.storageClassName` | `""` | StorageClass for the data PVC (`""` = cluster default). |
| `persistence.wal.size` | `2Gi` | Size of the WAL PVC (`OURIOS_WAL_ROOT`, receiver only). |
| `otel.exporterEndpoint` | `""` | OTLP endpoint for Ourios's own self-telemetry (RFC 0001 §6.8). |
| `extraEnv` | `[]` | Extra env vars (e.g. `OTEL_*` overrides). |
| `resources` | `{}` | Container resource requests/limits. |

A receiver-only or querier-only deployment is available by toggling the
respective `roles.*.enabled`; disabling both leaves only the compactor.

The image runs as nonroot (uid 65532) with a read-only root filesystem; the
chart sets `fsGroup` so the process can write its PVCs.
