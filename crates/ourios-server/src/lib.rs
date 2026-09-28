//! `ourios-server` library surface.
//!
//! The crate ships the `ourios-server` binary (`src/main.rs`); this library
//! target exposes the pieces that are worth driving in-process from
//! integration tests and reusing across roles. That is the **querier role**
//! (RFC 0016) — the HTTP query API over the logs DSL (RFC 0002), built on the
//! `ourios-querier` engine (RFC 0007) — together with the two surfaces it
//! mounts: the MCP query surface (`mcp`, RFC 0027), nested under `/mcp` when
//! enabled, and the layer-2 visibility step (`visibility`, RFC 0047) that
//! resolves each request's branch against the authorization graph before the
//! engine runs. The OTLP receiver role lives in the binary
//! (`src/receiver.rs`); the querier lives here so its `serve` / `router` are
//! testable without spawning the process. The **configuration** pieces
//! (RFC 0020) live here too, in [`config`], so the substitution resolver and
//! schema are unit-testable, as does the RFC 0026 token store ([`auth`]) that
//! both roles' enforcement points consume.

pub mod auth;
pub mod config;
mod mcp;
pub mod querier;
mod visibility;
