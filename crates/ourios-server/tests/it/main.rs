//! The consolidated server integration-test harness (RFC 0028 slice 2).
//!
//! One binary instead of 8 (RFC 0028 §1 / epic #382 — per-binary link
//! cost dominates `cargo test` wall time; this crate links the whole
//! stack). Files here moved verbatim from `tests/*.rs`; test names gain
//! only this harness's module-path prefix (RFC0028.1). The served-binary
//! suites spawn the `ourios-server` **child** process
//! (`CARGO_BIN_EXE_ourios-server`), never mutating their own — harness-
//! safe. One binary stays exempt (RFC0028.2):
//! `rfc0016_6_query_metrics.rs` installs the process-global `OTel` meter
//! provider (`init_in_memory`), which cannot share a process with another
//! installer.

mod rfc0003_16_served_binary;
mod rfc0008_10_recovery_driver;
mod rfc0013_6_wal_stays_local;
mod rfc0016_5_7_served_querier;
mod rfc0016_query_endpoint;
mod rfc0019_storage_backend;
mod rfc0020_config_file;
mod rfc0026_auth;
mod rfc0027_mcp;
