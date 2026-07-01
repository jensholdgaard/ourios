//! Configuration for `ourios-server` (RFC 0020).
//!
//! The config file (`--config <path>`) maps onto the resolved server config the
//! binary builds; this module holds the file-loading pieces: the
//! environment-variable substitution resolver ([`env_subst`]) and the YAML
//! schema plus its substitution walk ([`file`](mod@file)). The `--config` CLI wiring and
//! the mapping onto the resolved `ServerConfig` (through the existing `build_*`
//! validators — the single validation path) land in a subsequent RFC 0020 green
//! slice.
//!
//! The standard `OTEL_*` SDK environment is deliberately **not** handled here —
//! it configures the server's own telemetry SDK and is read by the SDK directly
//! (RFC 0020 §3.8). See `docs/rfcs/0020-configuration-file.md`.

pub mod env_subst;
pub mod file;
