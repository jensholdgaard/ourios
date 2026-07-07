# Docker

The release pipeline publishes a multi-arch image (amd64 + arm64) to
GHCR, cosign-signed keyless:

```sh
docker pull ghcr.io/jensholdgaard/ourios:0.1.1
```

Verify the signature before trusting it — the identity is pinned to
the exact release tag
([SECURITY.md](https://github.com/jensholdgaard/ourios/blob/main/SECURITY.md)
is the authoritative verification reference):

```sh
cosign verify \
  --certificate-identity 'https://github.com/jensholdgaard/ourios/.github/workflows/image.yml@refs/tags/v0.1.1' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  ghcr.io/jensholdgaard/ourios:0.1.1
```

## Run

Same binary, same configuration surface as the
[quickstart](./quickstart.md) — env vars, or a mounted config file:

```sh
docker run --rm \
  -p 4317:4317 -p 4318:4318 -p 4319:4319 \
  -v ourios-data:/var/lib/ourios \
  -e OURIOS_BUCKET_ROOT=/var/lib/ourios/data \
  -e OURIOS_WAL_ROOT=/var/lib/ourios/wal \
  -e OURIOS_RECEIVER_ENABLED=1 \
  -e OURIOS_QUERIER_ENABLED=1 \
  ghcr.io/jensholdgaard/ourios:0.1.1
```

With a config file instead (the production posture — auth lives in
the file):

```sh
docker run --rm \
  -p 4317:4317 -p 4318:4318 -p 4319:4319 \
  -v ourios-data:/var/lib/ourios \
  -v "$PWD/ourios.yaml:/etc/ourios/ourios.yaml:ro" \
  -e OURIOS_EDGE_TOKEN \
  ghcr.io/jensholdgaard/ourios:0.1.1 \
  --config /etc/ourios/ourios.yaml
```

Secrets stay out of the file via `${env:OURIOS_EDGE_TOKEN}`
references (the `-e` above passes it through), and the server shuts
down gracefully on SIGTERM — `docker stop` flushes the ingest
pipeline before exit.

Local note: any OCI runtime works — with containerd,
`nerdctl run`/`nerdctl compose` take the same arguments.
