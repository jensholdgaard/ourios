# Production OCI image for the `ourios-server` binary (first shipping
# milestone, workstream A). Multi-stage, glibc: a Debian-based Rust
# builder pinned to the MSRV, then a distroless `cc` runtime that carries
# only glibc + the binary. Distroless `cc-debian12` is multi-arch, so the
# CI build cross-builds linux/amd64 + linux/arm64 from one Dockerfile.

# Builder: at or above the workspace MSRV (Cargo.toml `rust-version = 1.88`)
# by digest for reproducibility; the tag comment lets Renovate's docker
# manager bump it.
FROM rust:1.96-bookworm@sha256:5e2214abe154fe26e39f64488952e5c991eeed1d6d6da7cc8381ae83927f0cfc AS builder
WORKDIR /build
COPY . .
# `--locked` so a stale Cargo.lock fails the build instead of being
# silently updated inside the image. BuildKit cache mounts keep the
# crate registry + compiled deps across builds (esp. the slow QEMU arm64
# leg); the binary is copied out of the cached `target/` so the runtime
# COPY can pick it up (cache mounts don't persist into the layer).
# The registry/git caches are arch-independent (downloaded sources) and
# safely shared; `target/` holds arch-specific objects, so it's keyed per
# $TARGETARCH to keep the amd64 + arm64 legs from sharing one dir.
ARG TARGETARCH
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target,id=ourios-target-${TARGETARCH} \
    cargo build --release --locked -p ourios-server \
    && cp /build/target/release/ourios-server /ourios-server

# Runtime: distroless `cc` carries glibc (the builder is glibc, not musl)
# and is published multi-arch, so it works under the QEMU cross-build.
# Digest-pinned for reproducibility (Renovate bumps via the tag comment).
FROM gcr.io/distroless/cc-debian12@sha256:d703b626ba455c4e6c6fbe5f36e6f427c85d51445598d564652a2f334179f96e
COPY --from=builder /ourios-server /usr/local/bin/ourios-server
# OTLP ingest: 4317 = gRPC, 4318 = HTTP (RFC 0003). 4319 is reserved for
# the future query endpoint (RFC 0016) and is intentionally not exposed
# yet.
EXPOSE 4317 4318
# Run as distroless's built-in nonroot user (uid 65532) — the numeric
# form lets a Kubernetes `runAsNonRoot` securityContext verify it.
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/ourios-server"]
