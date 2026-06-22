//! `ourios-server` library surface.
//!
//! The crate ships the `ourios-server` binary (`src/main.rs`); this library
//! target exposes the pieces that are worth driving in-process from
//! integration tests and reusing across roles. Today that is the **querier
//! role** (RFC 0016) — the HTTP query API over the logs DSL (RFC 0002), built
//! on the `ourios-querier` engine (RFC 0007). The OTLP receiver role lives in
//! the binary (`src/receiver.rs`); the querier lives here so its `serve` /
//! `router` are testable without spawning the process.

pub mod querier;
