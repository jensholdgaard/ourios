# Docker

The release pipeline publishes a multi-arch image (amd64 + arm64) to
GHCR, cosign-signed keyless:

```sh
docker pull ghcr.io/jensholdgaard/ourios:0.1.1
```

Verify the signature before trusting it — the identity is pinned to
the exact release tag, so **substitute both occurrences of the
version** when verifying another release
([SECURITY.md](https://github.com/jensholdgaard/ourios/blob/main/SECURITY.md)
is the authoritative verification reference):

```sh
cosign verify \
  --certificate-identity 'https://github.com/jensholdgaard/ourios/.github/workflows/image.yml@refs/tags/v0.1.1' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  ghcr.io/jensholdgaard/ourios:0.1.1
```

## Image variants

Every release publishes three signed multi-arch images from the same
source:

- **default** (`:<version>`) — glibc binary on `distroless/cc`.
- **`-static`** (`:<version>-static`) — static musl binary on
  `distroless/static`: no libc, libgcc, or libssl in the image, so the
  OS-package vulnerability surface scanners report is ~empty. Pick this
  one for the strictest supply-chain posture with the operational
  niceties (CA bundle, tzdata, nonroot passwd entry) kept.
- **`-scratch`** (`:<version>-scratch`) — the same musl binary on bare
  `scratch`, plus only the CA bundle TLS needs. Nothing else in the
  filesystem: the absolute minimum attack surface, but also zero
  operational conveniences — no tzdata, no passwd entry, and CA-bundle
  updates arrive only with Ourios releases rather than base-image bumps.

All three run identically (same flags, ports, and config surface below).

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
  -e OURIOS_S3_ACCESS_KEY_ID -e OURIOS_S3_SECRET_ACCESS_KEY \
  ghcr.io/jensholdgaard/ourios:0.1.1 \
  --config /etc/ourios/ourios.yaml
```

Secrets stay out of the file via `${env:…}` references — pass through
**every** variable your file references (the example forwards the
auth token and, for an S3-backend file like the
[Configuration](./configuration.md) example, the store credentials;
a local-backend file needs neither `OURIOS_S3_*` variable). The server shuts down
gracefully on SIGTERM — `docker stop` flushes the ingest pipeline
before exit.

Local note: any OCI runtime works — with containerd,
`nerdctl run`/`nerdctl compose` take the same arguments.
