//! The consolidated server integration-test harness (RFC 0028 slice 2).
//!
//! One binary instead of 8 (RFC 0028 §1 / epic #382 — per-binary link
//! cost dominates `cargo test` wall time; this crate links the whole
//! stack). Files here moved verbatim from `tests/*.rs`; test names gain
//! only this harness's module-path prefix (RFC0028.1). The served-binary
//! suites spawn the `ourios-server` **child** process
//! (`CARGO_BIN_EXE_ourios-server`), never mutating their own — harness-
//! safe. Two binaries stay exempt (RFC0028.2), each installing a
//! process-global `OTel` provider that cannot share a process with another
//! installer: `rfc0016_6_query_metrics.rs` (the global meter via
//! `init_in_memory`) and `rfc0038_1_mcp_span.rs` (a global tracer — rmcp
//! `tokio::spawn`s the tool dispatch, so a scoped subscriber can't capture
//! the `execute_tool <tool>` span; RFC0038.1 MCP arm).

mod collector_interop;
mod rfc0003_16_served_binary;
mod rfc0008_10_recovery_driver;
mod rfc0013_6_wal_stays_local;
mod rfc0016_5_7_served_querier;
mod rfc0016_query_endpoint;
mod rfc0019_storage_backend;
mod rfc0020_config_file;
mod rfc0026_auth;
mod rfc0027_mcp;
mod rfc0029_oidc;
mod rfc0030_tls;
mod rfc0032_query_schema;
mod rfc0038_1_request_spans;
