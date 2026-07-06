//! The consolidated miner integration-test harness (RFC 0028 slice 2).
//!
//! Three binaries fold here (RFC 0028 §1 / epic #382 — per-binary link
//! cost dominates `cargo test` wall time); files moved verbatim from
//! `tests/*.rs`, test names gaining only this harness's module-path
//! prefix (RFC0028.1). Four binaries stay dedicated (RFC0028.2):
//! `invariants.rs`, `hazards.rs`, `rfc0023_bounded_memory.rs`, and
//! `rfc_internal.rs` each install the process-global `OTel` meter
//! provider (`init_in_memory`) for one telemetry arm, and two installers
//! in one process race — see `tests/README.md`.

mod rfc0004_configuration_policy;
mod rfc0017_template_created;
mod rfc0024_properties;
