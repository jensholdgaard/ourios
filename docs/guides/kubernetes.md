# Kubernetes (Helm)

The chart at
[`deploy/helm/ourios`](https://github.com/jensholdgaard/ourios/tree/main/deploy/helm/ourios)
deploys the production topology: three workloads, one binary, backed
by **S3-compatible object storage** (AWS S3, MinIO, R2, Ceph/RGW, …
— [RFC 0019](../rfcs/0019-storage-backend-selection.md)):

- **receiver** — a StatefulSet with a per-replica WAL PVC (the WAL is
  local by design; only data + audit go to S3);
- **querier** — a stateless Deployment, scales independently;
- **compactor** — a singleton Deployment.

## Install

```sh
kubectl create secret generic ourios-s3 \
  --from-literal=OURIOS_S3_ACCESS_KEY_ID=… \
  --from-literal=OURIOS_S3_SECRET_ACCESS_KEY=…

helm install ourios deploy/helm/ourios \
  --set storage.backend=s3 \
  --set storage.s3.bucket=ourios-logs \
  --set storage.s3.region=eu-central-1 \
  --set storage.s3.endpoint=https://s3.eu-central-1.amazonaws.com \
  --set storage.s3.existingSecret=ourios-s3
```

(On AWS EKS, IRSA replaces the secret — leave `existingSecret` empty
and annotate the service account with the role ARN; the two modes are
mutually exclusive.)

The chart renders an [RFC 0020](../rfcs/0020-configuration-file.md)
config file into a ConfigMap; credentials reach it as `${env:…}`
references resolved from the secret — never inline.

The chart's
[README](https://github.com/jensholdgaard/ourios/tree/main/deploy/helm/ourios#readme)
is the authoritative reference: full `values.yaml` documentation, the
topology diagram, local-development (MinIO) recipes, and sizing
notes. This page stays a pointer so the two never drift.

## Sending and querying

In-cluster, point Collectors at the receiver Service
(`ourios-receiver:4317`) and query the querier Service on 4319 —
fronted by whatever ingress/TLS termination your cluster standardises
on. Configure [authentication](./authentication.md) before exposing
either beyond the cluster boundary.
