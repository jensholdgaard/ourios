//! Configuration for `ourios-server` (RFC 0020).
//!
//! The config file (`--config <path>`) maps onto the resolved server config the
//! binary builds; this module holds the file-loading pieces. Today that is the
//! environment-variable substitution resolver ([`env_subst`]); the YAML schema,
//! `--config` wiring, and validation land in subsequent RFC 0020 green slices.
//!
//! The standard `OTEL_*` SDK environment is deliberately **not** handled here —
//! it configures the server's own telemetry SDK and is read by the SDK directly
//! (RFC 0020 §3.8). See `docs/rfcs/0020-configuration-file.md`.

pub mod env_subst;
