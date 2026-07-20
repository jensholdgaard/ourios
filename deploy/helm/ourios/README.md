# Ourios Helm chart

Deploys [Ourios](https://github.com/jensholdgaard/ourios) — a log storage and
query backend on Apache Parquet, a Drain-derived template miner, and Apache
DataFusion — backed by **S3-compatible object storage** (RFC 0019).

"S3" here means the S3 API, **not AWS specifically**: the store works with AWS
S3 and any S3-compatible provider — MinIO, Cloudflare R2, Hetzner Object
Storage, Ceph/RADOS Gateway, Google Cloud Storage via its S3 interop endpoint,
and so on. Point `storage.s3.endpoint` at a non-AWS provider (see
[Credentials](#credentials)).

Ourios is one binary (`ourios-server`) running three roles. This chart deploys
them as three workloads sharing a data + audit store on object storage:

- **receiver** — OTLP log ingest, a **StatefulSet** with a per-replica
  write-ahead-log PVC;
- **querier** — the logs-DSL query API, a stateless **Deployment** that scales
  independently and reads the store (no PVC);
- **compactor** — the always-on background compactor, a singleton
  **Deployment**.

## Topology

![Ourios Helm chart topology](https://raw.githubusercontent.com/jensholdgaard/ourios/5a5b1ed398f60ee23117d4f4032f564691a7a1a7/deploy/helm/ourios/docs/topology.png)

Text fallback (for `helm show readme` in a terminal):

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
                   │  object store     │◀───────│ compactor Deployment  │
                   │  (S3 API)         │ sweep  │     (1 replica)       │
                   │ (data/audit/man.) │        │                      │
                   └───────────────────┘        └──────────────────────┘
```

Only the data/audit/manifest live on object storage. The **WAL is always a
local durable PVC, never object storage** (CLAUDE.md §3.4 WAL-before-ack / §3.6
object storage is the source of truth).

Diagram source: `docs/topology.py` in the repo checkout (regeneration
instructions in its docstring). The image URL above is pinned to the
commit that last regenerated the PNG — re-pin it when regenerating.

## Install

**S3 backend on AWS (production):**

```sh
helm install ourios deploy/helm/ourios \
  --set image.tag=<release> \
  --set storage.backend=s3 \
  --set storage.s3.bucket=my-ourios-bucket \
  --set storage.s3.region=us-east-1 \
  --set storage.s3.existingSecret=ourios-s3   # OR use IRSA (see below)
```

**S3-compatible backend (MinIO / R2 / Hetzner / Ceph / … — production):** the
same, plus `storage.s3.endpoint`:

```sh
helm install ourios deploy/helm/ourios \
  --set image.tag=<release> \
  --set storage.backend=s3 \
  --set storage.s3.bucket=my-ourios-bucket \
  --set storage.s3.endpoint=https://<provider-s3-endpoint> \
  --set storage.s3.region=auto \
  --set storage.s3.existingSecret=ourios-s3
```

The chart **fails to render** an `s3` backend with no `storage.s3.bucket`, so a
broken store can never install.

**Local backend (the default — single-node / dev only):**

```sh
helm install ourios deploy/helm/ourios        # storage.backend=local
```

`local` provisions one shared `ReadWriteOnce` PVC mounted by all three
workloads. That is coherent **only on a single node** (the pods must
co-schedule) **or with a `ReadWriteMany` StorageClass** — on a typical
multi-node RWO cluster some workloads will stay `Pending`. It is intended for
kick-the-tires / dev; **use `s3` in production**, where the workloads share the
store over object storage and scale independently.

Verify:

```sh
helm test ourios
```

> The image tag defaults to `latest` (no image is published for the
> pre-release `0.0.0` app version); pin a released tag via `image.tag` in
> production.

## Configuration

The chart renders an RFC 0020 **configuration file** — a per-role key in a
`ConfigMap` — and mounts it into each workload at `/etc/ourios/config.yaml`,
passing `--config` to the binary. The file is the authoritative source of
Ourios's data-plane configuration (storage, roles, compaction); the `values.yaml`
settings map onto it. Object-store credentials are **not** written into the file
— they appear as `${env:OURIOS_S3_*}` references resolved from the `Secret` via
`envFrom` (see [Credentials](#credentials)). The self-telemetry `OTEL_*` endpoint
and the AWS SDK region stay plain environment variables — those are read by their
own SDKs, never modelled in the config (RFC 0020 §3.8). A `checksum/config` pod
annotation rolls the workloads when the rendered config changes.

## Credentials

Credentials are **never** chart config as plaintext, and never written into the
mounted config file — the file references them as `${env:…}` (RFC 0020 §3.5) and
they are injected as environment variables at startup. Static credentials use the
**S3-named** keys Ourios reads (`OURIOS_S3_*`, RFC 0019 §3.4) — not AWS-specific;
they work against AWS S3 and every S3-compatible provider (MinIO, R2, Hetzner,
Ceph, …). Supply exactly one of:

1. **`storage.s3.existingSecret`** — the name of a `Secret` holding
   `OURIOS_S3_ACCESS_KEY_ID` and `OURIOS_S3_SECRET_ACCESS_KEY` (and optionally
   `OURIOS_S3_SESSION_TOKEN`). Injected via `envFrom`. Works for AWS S3 **and any
   S3-compatible provider**. Create it yourself:

   ```sh
   kubectl create secret generic ourios-s3 \
     --from-literal=OURIOS_S3_ACCESS_KEY_ID=... \
     --from-literal=OURIOS_S3_SECRET_ACCESS_KEY=...
   ```

2. **IRSA** (AWS EKS only) — leave `storage.s3.existingSecret` empty and set the
   role ARN on the service account (this path uses the AWS credential chain):

   ```sh
   --set serviceAccount.annotations."eks\.amazonaws\.com/role-arn"=arn:aws:iam::<acct>:role/<role>
   ```

   The pod assumes the role; no static keys exist anywhere. This mode is
   AWS-specific; on other providers use `storage.s3.existingSecret`. It requires
   `serviceAccount.create=true` (the default) so the chart renders the
   ServiceAccount and applies the annotation — the chart fails render if a
   role-arn is set with `serviceAccount.create=false`. To use an existing SA,
   annotate it out-of-band instead.

Setting **both** `storage.s3.existingSecret` and the IRSA `role-arn` annotation
**fails render** — the static keys would shadow the web-identity credentials, so
exactly one mode must be chosen.

For a **non-AWS provider**, also set `storage.s3.endpoint` to its S3 endpoint URL
(e.g. `http://minio:9000`, a Cloudflare R2 / Hetzner endpoint, …) and
`storage.s3.region` to the provider's region (or a placeholder like `us-east-1`
if it has none).

## Per-role IAM (least privilege)

A single credential shared by all three workloads holds the union of their
privileges — a compromised querier could then delete data it never needed to
touch. The roles' object-store needs are strictly narrower, and they split
cleanly:

| Role      | S3 actions                                            | Holds delete? |
| --------- | ----------------------------------------------------- | ------------- |
| querier   | `GetObject`, `ListBucket` (see cache note)            | no            |
| receiver  | `GetObject`, `PutObject`, `ListBucket`                | no            |
| compactor | `GetObject`, `PutObject`, `DeleteObject`, `ListBucket`| **only one**  |

The receiver writes data/audit objects and swaps manifests (a conditional
`PutObject`) but deletes nothing; the querier only reads; reclaiming compacted
inputs is the compactor's job alone. With this split, a compromised querier can
read but not destroy, and *nothing* except the singleton compactor can delete
data.

**Querier cache note (RFC 0033):** after a cache miss the querier attempts a
*best-effort* write-through of its template-map cache artifact (and cleanup of
the stale v1 key). Under the read-only policy above this publish simply fails —
**by contract that never fails a query** (it is a telemetry-only outcome), but
the cache never populates, so every query re-pays the audit fold. To keep the
cache warm without widening the read path, grant the querier `PutObject` +
`DeleteObject` scoped to the one cache key it writes:
`arn:aws:s3:::<bucket>/[<prefix>/]audit/tenant_id=*/template_map*` — the
querier still cannot touch data objects.

Each role opts into its own ServiceAccount (falling back to the shared
`serviceAccount` otherwise), carrying its own IRSA role:

```sh
helm install ourios ./ourios \
  --set storage.backend=s3 --set storage.s3.bucket=<bucket> \
  --set receiver.serviceAccount.create=true \
  --set receiver.serviceAccount.annotations."eks\.amazonaws\.com/role-arn"=arn:aws:iam::<acct>:role/ourios-receiver \
  --set querier.serviceAccount.create=true \
  --set querier.serviceAccount.annotations."eks\.amazonaws\.com/role-arn"=arn:aws:iam::<acct>:role/ourios-querier \
  --set compactor.serviceAccount.create=true \
  --set compactor.serviceAccount.annotations."eks\.amazonaws\.com/role-arn"=arn:aws:iam::<acct>:role/ourios-compactor \
  --set serviceAccount.create=false
```

(The last flag skips the shared ServiceAccount — with all three roles on
their own accounts it would be rendered but unused.)

Because IRSA credentials come from STS, they are **short-lived and rotated
automatically** — no static key exists to leak or forget to rotate. Each IAM
role's trust policy federates to its ServiceAccount
(`system:serviceaccount:<namespace>:<fullname>-<role>` — check the
rendered name with `helm template`, since the chart's fullname collapses
to the release name when it already contains "ourios"); `eksctl create
iamserviceaccount` or the Terraform `iam-role-for-service-accounts` module wires
this in one step. The permission policy per role (swap the `Action` list per the
table; the example shows the `storage.s3.prefix`-scoped form — with no
prefix, drop the `Condition` and use `<bucket>/*` on the object arn):

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"],
      "Resource": "arn:aws:s3:::<bucket>/<prefix>/*"
    },
    {
      "Effect": "Allow",
      "Action": "s3:ListBucket",
      "Resource": "arn:aws:s3:::<bucket>",
      "Condition": { "StringLike": { "s3:prefix": "<prefix>/*" } }
    }
  ]
}
```

(Without the `s3:prefix` condition, `ListBucket` on the bucket arn allows
listing **every** key in the bucket — object access would be scoped but
listing would not.)

Two hardening notes:

- **Second control on deletes**: S3 versioning (or object lock) on the bucket
  makes the compactor's `DeleteObject` reversible — destructive action then
  requires defeating two independent mechanisms, not one.
- **Non-AWS providers**: the same split works anywhere the provider offers
  scoped credentials (MinIO policies, Ceph users, R2 API tokens) — issue three
  static credentials with the table's permissions and give each role its own
  `Secret` out-of-band. The chart's `storage.s3.existingSecret` currently wires
  one shared Secret; per-role static Secrets need per-role values files or
  out-of-band env injection.

For mTLS identity rotation (SPIRE, cert-manager): the binary hot-reloads its TLS
listener certificates (RFC 0030), so a sidecar like `spiffe-helper` writing
rotating certs to a shared volume integrates without restarts. TLS listener
config is delivered via the RFC 0020 config file out-of-band today — chart-level
TLS values are not yet exposed.

## Compactor topology

The `ourios-server` binary runs the compaction role by default. To avoid every
pod sweeping, the receiver and querier disable compaction in their config file
(`compaction.enabled: false`), so a **single dedicated `compactor` Deployment
(1 replica)** is the only sweeper.

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
| `image.tag` | `""` (→ `latest`) | Image tag; pin a released tag in production. |
| `storage.backend` | `local` | `local` (single-node/dev) or `s3` (production — the S3 API, AWS or any S3-compatible provider). |
| `storage.s3.bucket` | `""` | **Required for s3** (`OURIOS_S3_BUCKET`); render fails if empty. |
| `storage.s3.endpoint` | `""` | S3-compatible endpoint URL — set for any non-AWS provider (MinIO/R2/Hetzner/Ceph/LocalStack); empty targets AWS. |
| `storage.s3.region` | `""` | Bucket region; drives both `OURIOS_S3_REGION` and `AWS_DEFAULT_REGION`. |
| `storage.s3.prefix` | `""` | Key prefix within the bucket. |
| `storage.s3.existingSecret` | `""` | Secret with the S3-named credential keys (`OURIOS_S3_ACCESS_KEY_ID`/`OURIOS_S3_SECRET_ACCESS_KEY`), injected via `envFrom`. Used by any S3-compatible provider. |
| `storage.local.bucketRoot` | `/var/lib/ourios/data` | Data dir for the local backend. |
| `storage.local.size` | `10Gi` | Local data PVC size. |
| `receiver.enabled` | `true` | OTLP ingest StatefulSet (gRPC `:4317` + HTTP `:4318`). |
| `receiver.replicas` | `1` | Receiver replicas (each gets its own WAL PVC). |
| `receiver.wal.size` | `2Gi` | WAL PVC size (`OURIOS_WAL_ROOT`, always local). |
| `receiver.wal.storageClassName` | `""` | WAL StorageClass (`""` = cluster default). |
| `querier.enabled` | `true` | Querier Deployment (HTTP `:4319`). |
| `querier.replicas` | `2` | Querier replicas (scales independently, no PVC). |
| `querier.defaultWindowSecs` | `3600` | Default look-back for a query with no `range(...)`. |
| `compactor.enabled` | `true` | Dedicated singleton compactor Deployment. Setting `false` **fails render** (the only sweeper — hazard #4). |
| `compactor.intervalSecs` | `300` | Compaction sweep cadence (used only by the dedicated compactor). |
| `serviceAccount.annotations` | `{}` | AWS EKS IRSA `eks.amazonaws.com/role-arn` goes here (alternative to `storage.s3.existingSecret`). One identity for all roles — prefer the per-role split. |
| `<role>.serviceAccount.create` | `false` | Role-scoped ServiceAccount for `receiver`/`querier`/`compactor` — the least-privilege IAM seam ("Per-role IAM" above). `false` falls back to the shared `serviceAccount`. |
| `<role>.serviceAccount.annotations` | `{}` | The role's own IRSA `role-arn` (querier read-only, receiver no-delete, compactor sole delete-holder). |
| `<role>.serviceAccount.name` | `""` | With `create=true`: overrides the rendered `<fullname>-<role>` name. With `create=false`: binds an **existing** ServiceAccount of that name (managed out-of-band). |
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
