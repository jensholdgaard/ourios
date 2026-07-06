//! `ourios-server` library surface.
//!
//! The crate ships the `ourios-server` binary (`src/main.rs`); this library
//! target exposes the pieces that are worth driving in-process from
//! integration tests and reusing across roles. Today that is the **querier
//! role** (RFC 0016) — the HTTP query API over the logs DSL (RFC 0002), built
//! on the `ourios-querier` engine (RFC 0007). The OTLP receiver role lives in
//! the binary (`src/receiver.rs`); the querier lives here so its `serve` /
//! `router` are testable without spawning the process. The **configuration**
//! pieces (RFC 0020) live here too, in [`config`], so the substitution
//! resolver and schema are unit-testable, as does the RFC 0026 token store
//! ([`auth`]) that both roles' enforcement points consume.

pub mod auth;
pub mod config;
mod mcp;
pub mod querier;
