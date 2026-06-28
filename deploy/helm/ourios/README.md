# Ourios Helm chart

Deploys [Ourios](https://github.com/jensholdgaard/ourios) — a log storage and
query backend on Apache Parquet, a Drain-derived template miner, and Apache
DataFusion — **S3-native** (RFC 0019).

Ourios is one binary (`ourios-server`) running three roles. This chart deploys
them as three workloads sharing a data + audit store on S3:

- **receiver** — OTLP log ingest, a **StatefulSet** with a per-replica
  write-ahead-log PVC;
- **querier** — the logs-DSL query API, a stateless **Deployment** that scales
  independently and reads S3 (no PVC);
- **compactor** — the always-on background compactor, a singleton
  **Deployment**.

## Topology

```
            OTLP                         query
              │                            │
   ┌──────────▼──────────┐     ┌───────────▼───────────┐
   │ receiver StatefulSet │     │  querier Deployment    │
   │  4317/gRPC 4318/HTTP │     │      4319/HTTP         │
   │  WAL PVC per replica │     │  stateless (N replicas)│
   └──────────┬──────────┘     └───────────┬───────────┘
              │  Parquet write              │  read
              └──────────────┬──────────────┘
                             ▼
                   ┌───────────────────┐        ┌──────────────────────┐
                   │   S3 data store   │◀───────│ compactor Deployment  │
                   │ (data/audit/man.) │ sweep  │     (1 replica)       │
                   └───────────────────┘        └──────────────────────┘
```

Only the data/audit/manifest live on S3. The **WAL is always a local durable
PVC, never S3** (CLAUDE.md §3.4 WAL-before-ack / §3.6 object storage is the
source of truth).

## Install

S3 backend (production):

```sh
helm install ourios deploy/helm/ourios \
  --set image.tag=<release> \
  --set storage.s3.bucket=my-ourios-bucket \
  --set storage.s3.region=us-east-1 \
  --set aws.existingSecret=ourios-aws        # OR use IRSA (see below)
```

Local backend (single-node / dev only):

```sh
helm install ourios deploy/helm/ourios --set storage.backend=local
```

Verify:

```sh
helm test ourios
```

## AWS credentials

Credentials are **never** chart config as plaintext. Supply exactly one of:

1. **`aws.existingSecret`** — the name of a `Secret` holding
   `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` (and optionally
   `AWS_SESSION_TOKEN`). Injected via `envFrom` so the SDK credential chain
   picks it up. Create it yourself:

   ```sh
   kubectl create secret generic ourios-aws \
     --from-literal=AWS_ACCESS_KEY_ID=... \
     --from-literal=AWS_SECRET_ACCESS_KEY=...
   ```

2. **IRSA** (preferred on EKS) — leave `aws.existingSecret` empty and set the
   role ARN on the service account:

   ```sh
   --set serviceAccount.annotations."eks\.amazonaws\.com/role-arn"=arn:aws:iam::<acct>:role/<role>
   ```

   The pod assumes the role; no static keys exist anywhere.

## Compactor topology

The `ourios-server` binary runs the compaction role by default. To avoid every
pod sweeping, the receiver and querier workloads set `OURIOS_COMPACTION_ENABLED=0`,
so a **single dedicated `compactor` Deployment (1 replica)** is the only sweeper.

Scaling that compactor `Deployment` past 1 replica is **safe but unnecessary**:
the manifest publish-CAS commit (RFC 0009 §3.2 / RFC0013.3–.4) makes concurrent
sweeps correct — a losing sweeper's consolidated file is just an orphan a later
sweep reclaims — but it duplicates the per-interval object listing for no gain.
A `replicas: 1` Deployment self-heals (k8s reschedules a dead pod; a brief gap
in this background maintenance is harmless), so leader election isn't needed and
is intentionally out of scope. Tune the cadence via `compactor.intervalSecs`.

## Key values

| Key | Default | Description |
| --- | --- | --- |
| `image.repository` | `ghcr.io/jensholdgaard/ourios` | Image repository. |
| `image.tag` | `""` (chart `appVersion`) | Image tag. |
| `storage.backend` | `s3` | `s3` or `local`. |
| `storage.s3.bucket` | `""` | **Required for s3** (`OURIOS_S3_BUCKET`). |
| `storage.s3.endpoint` | `""` | S3-compatible endpoint (MinIO/LocalStack). |
| `storage.s3.region` | `""` | Bucket region (`OURIOS_S3_REGION`). |
| `storage.s3.prefix` | `""` | Key prefix within the bucket. |
| `storage.local.bucketRoot` | `/var/lib/ourios/data` | Data dir for the local backend. |
| `storage.local.size` | `10Gi` | Local data PVC size. |
| `aws.existingSecret` | `""` | Secret with AWS keys, injected via `envFrom`. |
| `aws.region` | `""` | `AWS_DEFAULT_REGION` for the credential chain. |
| `receiver.enabled` | `true` | OTLP ingest StatefulSet (gRPC `:4317` + HTTP `:4318`). |
| `receiver.replicas` | `1` | Receiver replicas (each gets its own WAL PVC). |
| `receiver.wal.size` | `2Gi` | WAL PVC size (`OURIOS_WAL_ROOT`, always local). |
| `receiver.wal.storageClassName` | `""` | WAL StorageClass (`""` = cluster default). |
| `querier.enabled` | `true` | Querier Deployment (HTTP `:4319`). |
| `querier.replicas` | `2` | Querier replicas (scales independently, no PVC). |
| `querier.defaultWindowSecs` | `3600` | Default look-back for a query with no `range(...)`. |
| `compactor.enabled` | `true` | Dedicated singleton compactor Deployment. |
| `compactor.intervalSecs` | `300` | Compaction cadence (applied to every workload). |
| `serviceAccount.annotations` | `{}` | IRSA `eks.amazonaws.com/role-arn` goes here. |
| `otel.exporterEndpoint` | `""` | OTLP endpoint for Ourios's own self-telemetry. |
| `extraEnv` | `[]` | Extra env vars (e.g. `OTEL_*`). No plaintext creds. |

The image runs as nonroot (uid 65532) with a read-only root filesystem; the
chart sets `fsGroup` so the process can write the WAL PVC.

## Probes

The binary exposes no HTTP health route yet (the OTLP and query endpoints are
POST-only), so the chart uses **TCP socket probes** on the bound role ports
(receiver `:4318`, querier `:4319`). The compactor has no listening port and
no probe; it is supervised by the process. Swap these for HTTP probes once a
`/healthz` endpoint lands.
