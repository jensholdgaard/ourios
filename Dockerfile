# Production OCI image for the `ourios-server` binary (first shipping
# milestone, workstream A). Multi-stage, glibc: a Debian-based Rust
# builder pinned to the MSRV, then a distroless `cc` runtime that carries
# only glibc + the binary. Distroless `cc-debian12` is multi-arch, so the
# CI build cross-builds linux/amd64 + linux/arm64 from one Dockerfile.

# Builder: pinned to the workspace MSRV (Cargo.toml `rust-version = 1.85`).
FROM rust:1.85-bookworm AS builder
WORKDIR /build
COPY . .
RUN cargo build --release -p ourios-server

# Runtime: distroless `cc` carries glibc (the builder is glibc, not musl)
# and is published multi-arch, so it works under the QEMU cross-build.
FROM gcr.io/distroless/cc-debian12
COPY --from=builder /build/target/release/ourios-server /usr/local/bin/ourios-server
# OTLP ingest: 4317 = gRPC, 4318 = HTTP (RFC 0003). 4319 is reserved for
# the future query endpoint (RFC 0016) and is intentionally not exposed
# yet.
EXPOSE 4317 4318
ENTRYPOINT ["/usr/local/bin/ourios-server"]
